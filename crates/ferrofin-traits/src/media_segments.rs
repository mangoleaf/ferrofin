//! Media-segment manager trait — commercial/intro/outro/recap segments.
//!
//! Port of `MediaBrowser.Controller.MediaSegments.IMediaSegmentManager`.
//!
//! Port rules applied:
//! - The C# `BaseItem` receivers become [`uuid::Uuid`] identity arguments; the
//!   `LibraryOptions` argument is resolved from live library configuration by
//!   the implementation; providers use the object-safe registry below.
//! - Segments crossing the API boundary reuse the [`MediaSegmentDto`] wire DTO
//!   (create returns the persisted DTO; queries yield DTO lists). The
//!   `ferrofin-db` [`MediaSegmentEntity`](ferrofin_db::entities::playback::MediaSegmentEntity)
//!   row stays inside the impl.
//! - `typeFilter` becomes an optional [`MediaSegmentType`] slice; the
//!   `filterByProvider` flag is retained.
//! - Synchronous C# predicates (`IsTypeSupported`, `HasSegments`) stay `async
//!   fn -> Result<bool, _>` so the impl may hit the database uniformly.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` is
//!   dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::media_segments::{
    MediaSegmentDto, MediaSegmentGenerationRequest, MediaSegmentType,
};
use uuid::Uuid;

use crate::error::ServiceError;

/// A registered media-segment provider: its display name and stable id.
///
/// Port of the C# `(string Name, string Id)` tuple returned by
/// `GetSupportedProviders`; a named struct reads better across the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaSegmentProviderInfo {
    /// The provider's display name.
    pub name: String,
    /// The provider's stable identifier.
    pub id: String,
}

/// Stable Jellyfin media-segment provider identity (lower-case display name).
#[must_use]
pub fn media_segment_provider_id(name: &str) -> String {
    ferrofin_common::extensions::get_md5(&ferrofin_util::string_extensions::lower_invariant(name))
        .simple()
        .to_string()
}

/// A producer-owned snapshot mutation, distinct from ordinary served-row CRUD.
#[derive(Debug, Clone)]
pub enum MediaSegmentCacheMutation {
    /// Replace all output, or just one type while retaining other types.
    Replace {
        /// The type to replace; `None` replaces the complete snapshot.
        type_filter: Option<MediaSegmentType>,
        /// Complete new output for the selected scope.
        segments: Vec<MediaSegmentDto>,
    },
    /// Append one output, allowing multiple commercial segments.
    Append(MediaSegmentDto),
    /// Remove the matching type/range, independent of served-row identity changes.
    Remove {
        /// The served segment whose producer timestamp should be removed.
        segment: MediaSegmentDto,
        /// Inclusive range tolerance used by the producer, in 100 ns ticks.
        tolerance_ticks: u64,
    },
}
impl MediaSegmentCacheMutation {
    /// Applies a mutation to the current complete producer snapshot.
    #[must_use]
    pub fn apply(&self, current: &[MediaSegmentDto]) -> Vec<MediaSegmentDto> {
        match self {
            Self::Replace {
                type_filter,
                segments,
            } => {
                let mut updated: Vec<_> = current
                    .iter()
                    .filter(|segment| type_filter.is_some_and(|kind| segment.type_ != kind))
                    .cloned()
                    .collect();
                updated.extend(segments.iter().cloned());
                updated
            }
            Self::Append(segment) => {
                let mut updated = current.to_vec();
                updated.push(segment.clone());
                updated
            }
            Self::Remove {
                segment,
                tolerance_ticks,
            } => current
                .iter()
                .filter(|old| {
                    old.type_ != segment.type_
                        || old.start_ticks.abs_diff(segment.start_ticks) > *tolerance_ticks
                        || old.end_ticks.abs_diff(segment.end_ticks) > *tolerance_ticks
                })
                .cloned()
                .collect(),
        }
    }
}

/// A provider of previously detected or freshly extracted media segments.
#[async_trait]
pub trait MediaSegmentProvider: Send + Sync {
    /// Advertised display name, used by the library enable/order settings.
    fn name(&self) -> &str;
    /// Default execution order before a library saves a specific order.
    fn order(&self) -> i32 {
        0
    }
    /// Explicit former namespaces belonging to this registered provider.
    fn legacy_ids(&self) -> Vec<String> {
        Vec::new()
    }
    /// Whether this provider can supply segments for this item now.
    async fn supports(&self, item: &BaseItemEntity) -> Result<bool, ServiceError>;
    /// Obtain this provider's segments; existing rows belong only to this provider.
    async fn get_segments(
        &self,
        request: &MediaSegmentGenerationRequest,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError>;
    /// Remove the provider's persistent data when an item is explicitly erased.
    async fn cleanup(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Save producer output separately from the served segment rows.
    async fn cache_segments(
        &self,
        _item_id: Uuid,
        _segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Read producer data without invoking extraction; older rows seed absent snapshots.
    async fn cached_segments(
        &self,
        _item_id: Uuid,
        existing: &[MediaSegmentDto],
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        Ok(existing.to_vec())
    }
    /// Read, modify and persist the complete producer snapshot under its shared lock.
    /// A failed publication must preserve the previous snapshot.
    async fn mutate_cache(
        &self,
        item_id: Uuid,
        existing: &[MediaSegmentDto],
        mutation: &MediaSegmentCacheMutation,
    ) -> Result<(), ServiceError> {
        self.cache_segments(item_id, &mutation.apply(existing))
            .await
    }
    /// Erase producer-owned output even when no served rows remain for its items.
    async fn erase_cached_segments(
        &self,
        _type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    /// Adopt older rows before an overwrite, without replacing an existing cache.
    async fn initialize_cache(
        &self,
        _item_id: Uuid,
        _segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// Creates, queries and deletes the media segments attached to library items.
///
/// Port of `IMediaSegmentManager`.
#[async_trait]
pub trait MediaSegmentManager: Send + Sync {
    /// Register a loaded producer and return the retained instance with its shared cache lock.
    /// Duplicate identities return the first registration.
    fn register_segment_provider(
        &self,
        provider: Arc<dyn MediaSegmentProvider>,
    ) -> Arc<dyn MediaSegmentProvider> {
        provider
    }

    /// Whether an identity/namespace belongs to an actual registered producer.
    fn is_registered_segment_provider(&self, _identifier: &str) -> bool {
        false
    }

    /// Adopt exact former namespaces of an actual loaded producer as output evidence.
    /// Unloaded identities are never candidates for this operation.
    async fn adopt_loaded_segment_provider(
        &self,
        _provider: Arc<dyn MediaSegmentProvider>,
    ) -> Result<bool, ServiceError> {
        Ok(false)
    }

    /// Current registered provider choices, in default execution order.
    fn registered_segment_providers(&self) -> Vec<MediaSegmentProviderInfo> {
        Vec::new()
    }

    /// Live library gate for an advertised producer name or its registered former namespace.
    async fn is_provider_enabled(
        &self,
        _item_id: Uuid,
        _provider: &str,
    ) -> Result<bool, ServiceError> {
        Ok(true)
    }

    /// Whether the item's type supports media segments at all.
    async fn is_type_supported(&self, item_id: Uuid) -> Result<bool, ServiceError>;

    /// Creates a new media segment for an item, recording the provider id.
    async fn create_segment(
        &self,
        segment: &MediaSegmentDto,
        segment_provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError>;

    /// Deletes a single media segment by its id.
    async fn delete_segment(&self, segment_id: Uuid) -> Result<(), ServiceError>;

    /// Deletes all media segments belonging to an item.
    async fn delete_segments(&self, item_id: Uuid) -> Result<(), ServiceError>;

    /// Deletes an item's segments that were written by `provider_id`, optionally
    /// limited to one type. Lets a provider (e.g. the intro skipper) replace only
    /// its own rows on re-analysis, leaving user-authored and other providers'
    /// segments intact.
    async fn delete_provider_segments(
        &self,
        item_id: Uuid,
        provider_id: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError>;

    /// Deletes every segment written by `provider_id` across all items, optionally
    /// limited to one type. Backs a provider's bulk "erase timestamps" action.
    /// Defaults to a no-op so stub/test managers need not implement it.
    async fn delete_all_provider_segments(
        &self,
        _provider_id: &str,
        _type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    /// Reads an actual producer's authoritative output, independent of its served projection.
    async fn get_producer_segments(
        &self,
        item_id: Uuid,
        _provider_id: &str,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        self.get_segments(item_id, None, false).await
    }
    /// Publishes producer output before replacing its selected served-row scope.
    async fn replace_producer_segments(
        &self,
        item_id: Uuid,
        provider_id: &str,
        type_filter: Option<MediaSegmentType>,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        self.delete_provider_segments(item_id, provider_id, type_filter)
            .await?;
        for segment in segments {
            self.create_segment(segment, provider_id).await?;
        }
        Ok(())
    }
    /// Publishes an authenticated loaded producer's complete output using its retained adapter.
    /// Newly loaded task producers are advertised after successful persistence.
    async fn replace_loaded_producer_segments(
        &self,
        item_id: Uuid,
        provider: Arc<dyn MediaSegmentProvider>,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        provider.cache_segments(item_id, segments).await?;
        let provider = self.register_segment_provider(provider);
        self.replace_producer_segments(
            item_id,
            &media_segment_provider_id(provider.name()),
            None,
            segments,
        )
        .await
    }
    /// Publishes a user-provided producer segment before creating its served row.
    async fn create_producer_segment(
        &self,
        segment: &MediaSegmentDto,
        provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        self.create_segment(segment, provider_id).await
    }
    /// Deletes producer data and its served projection for a selected type/range.
    async fn delete_producer_segment(
        &self,
        _item_id: Uuid,
        _provider_id: &str,
        segment_id: Uuid,
        _type_filter: MediaSegmentType,
    ) -> Result<(), ServiceError> {
        self.delete_segment(segment_id).await
    }
    /// Erases a producer's snapshots and served rows, including currently hidden output.
    async fn delete_all_producer_segments(
        &self,
        provider_id: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        self.delete_all_provider_segments(provider_id, type_filter)
            .await
    }

    /// Lists the segments for an item, optionally filtered by type and/or to
    /// providers currently enabled on the item's library.
    async fn get_segments(
        &self,
        item_id: Uuid,
        type_filter: Option<&[MediaSegmentType]>,
        filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError>;

    /// Whether any segments are stored for the item.
    async fn has_segments(&self, item_id: Uuid) -> Result<bool, ServiceError>;

    /// Lists the segment providers that support the item.
    async fn get_supported_providers(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError>;

    /// Runs every segment provider enabled for the item's library over it,
    /// returning how many providers ran.
    ///
    /// Port of `IMediaSegmentManager.RunSegmentPluginProviders(item,
    /// libraryOptions, forceOverwrite, ct)`, which backs upstream's
    /// `MediaSegmentExtractionTask`. Every provider enabled for the item's
    /// library runs either way; `overwrite` deletes the item's existing
    /// segments first, so a provider that would have returned exactly what is
    /// already stored writes them again instead of being skipped. The
    /// `LibraryOptions` argument is resolved by the implementation from the
    /// item itself. The provider count is a Ferrofin addition (upstream
    /// returns nothing) so a caller can log what actually ran.
    ///
    /// Defaults to "no provider ran" so stub/test managers need not implement
    /// it.
    ///
    /// # Errors
    /// Backend errors from resolving the item, clearing its segments, or
    /// deciding which providers support it. A provider's extraction failure is
    /// logged and the remaining providers still run, matching upstream.
    async fn run_segment_providers(
        &self,
        item_id: Uuid,
        overwrite: bool,
    ) -> Result<usize, ServiceError> {
        let _ = (item_id, overwrite);
        Ok(0)
    }
}

fn _assert_object_safe_media_segment_provider(_: &dyn MediaSegmentProvider) {}

fn _assert_object_safe_media_segment_manager(_: &dyn MediaSegmentManager) {}

#[cfg(test)]
mod tests {
    use super::{MediaSegmentProviderInfo, media_segment_provider_id};

    #[test]
    fn stable_identity_matches_dotnet_utf16_md5_and_ignores_name_case() {
        assert_eq!(
            media_segment_provider_id("Intro Skipper"),
            "b0338b450421c081992860f1d02f261f"
        );
        assert_eq!(
            media_segment_provider_id("INTRO SKIPPER"),
            media_segment_provider_id("Intro Skipper")
        );
        assert_ne!(
            media_segment_provider_id("IntroSkipper"),
            media_segment_provider_id("Intro Skipper")
        );
    }

    #[test]
    fn provider_info_default_is_empty() {
        let p = MediaSegmentProviderInfo::default();
        assert!(p.name.is_empty());
        assert!(p.id.is_empty());
    }
}
