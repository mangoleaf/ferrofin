//! Media-segment manager trait — commercial/intro/outro/recap segments.
//!
//! Port of `MediaBrowser.Controller.MediaSegments.IMediaSegmentManager`.
//!
//! Port rules applied:
//! - The C# `BaseItem` receivers become [`uuid::Uuid`] identity arguments; the
//!   `LibraryOptions` argument (a per-library config the impl resolves itself)
//!   and the plugin-provider machinery are dropped from the trait surface.
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

use async_trait::async_trait;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
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

/// Creates, queries and deletes the media segments attached to library items.
///
/// Port of `IMediaSegmentManager`.
#[async_trait]
pub trait MediaSegmentManager: Send + Sync {
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
    /// deciding which providers support it. Divergence worth knowing: upstream
    /// catches a *provider's* own failure inside this call and only logs it, so
    /// it never reaches the caller; an implementation here may surface one, and
    /// a caller should read it as "this item got no segments", not as a reason
    /// to abandon a whole pass.
    async fn run_segment_providers(
        &self,
        item_id: Uuid,
        overwrite: bool,
    ) -> Result<usize, ServiceError> {
        let _ = (item_id, overwrite);
        Ok(0)
    }
}

fn _assert_object_safe_media_segment_manager(_: &dyn MediaSegmentManager) {}

#[cfg(test)]
mod tests {
    use super::MediaSegmentProviderInfo;

    #[test]
    fn provider_info_default_is_empty() {
        let p = MediaSegmentProviderInfo::default();
        assert!(p.name.is_empty());
        assert!(p.id.is_empty());
    }
}
