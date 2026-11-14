//! Registered media-segment providers, live library policy, and stored segments.
//! Port of Jellyfin's `MediaSegmentManager`; producer caches survive row refreshes.

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::{base_items::BaseItemEntity, playback::MediaSegmentEntity};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_model::entities_media::owning_library;
use ferrofin_model::media_segments::{
    MediaSegmentDto, MediaSegmentGenerationRequest, MediaSegmentType,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::media_segments::{
    MediaSegmentCacheMutation, MediaSegmentManager, MediaSegmentProvider, MediaSegmentProviderInfo,
    media_segment_provider_id,
};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;
use crate::kinds::{is_audio, is_video};
use crate::media_segment_repository;

/// A producer whose snapshot a write published to, with its snapshot from
/// before the write.
type PublishedSource = (Arc<dyn MediaSegmentProvider>, Vec<MediaSegmentDto>);

#[derive(Default)]
struct MutationState {
    // Only items a `run_provider` is extracting for, each run holding the `Arc` behind the
    // `Weak`: a write no run watches records nothing, so the map stays that small (gl-1316).
    revisions: std::collections::HashMap<Uuid, (std::sync::Weak<()>, u64)>,
    // A provider-wide erase also changes cache-only items without SQL rows to enumerate.
    epoch: u64,
}
impl MutationState {
    /// Starts a run's watch on `item`: its handle, and the revision it compares at commit.
    fn watch(&mut self, item: Uuid) -> (Arc<()>, (u64, u64)) {
        let (watchers, revision) = self.revisions.entry(item).or_default();
        let run = watchers.upgrade().unwrap_or_else(|| {
            let run = Arc::new(());
            *watchers = Arc::downgrade(&run);
            run
        });
        (run, (self.epoch, *revision))
    }
    /// Ends `run`'s watch on `item` and returns the revision now. The entry goes
    /// with the last run watching it; an aborted run's goes with the item's next
    /// write or run.
    fn unwatch(&mut self, item: Uuid, run: Arc<()>) -> (u64, u64) {
        drop(run);
        // Unreachable while `run` was live; were it reached, `u64::MAX` reads as superseded.
        let Some((watchers, revision)) = self.revisions.get(&item) else {
            return (self.epoch, u64::MAX);
        };
        let revision = *revision;
        if watchers.strong_count() == 0 {
            self.revisions.remove(&item);
        }
        (self.epoch, revision)
    }
}

/// Persists segments and runs registered producers under their owning library's policy.
#[derive(Clone)]
pub struct FerrofinMediaSegmentManager {
    db: Database,
    library_manager: Arc<dyn LibraryManager>,
    virtual_folders: Option<Arc<dyn VirtualFolderManager>>,
    providers: Arc<RwLock<Vec<Arc<dyn MediaSegmentProvider>>>>,
    // Short DB/cache publication sequences only; Supports/GetSegments run outside this gate.
    mutations: Arc<tokio::sync::Mutex<MutationState>>,
}
impl std::fmt::Debug for FerrofinMediaSegmentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinMediaSegmentManager")
            .finish_non_exhaustive()
    }
}
impl FerrofinMediaSegmentManager {
    /// Creates the manager; the composition root attaches actual loaded providers.
    #[must_use]
    pub fn new(db: Database, library_manager: Arc<dyn LibraryManager>) -> Self {
        Self {
            db,
            library_manager,
            virtual_folders: None,
            providers: Arc::new(RwLock::new(Vec::new())),
            mutations: Arc::new(tokio::sync::Mutex::new(MutationState::default())),
        }
    }
    /// Resolve current per-library options for every read or provider run.
    #[must_use]
    pub fn with_virtual_folders(mut self, folders: Arc<dyn VirtualFolderManager>) -> Self {
        self.virtual_folders = Some(folders);
        self
    }
    fn providers(&self) -> Vec<Arc<dyn MediaSegmentProvider>> {
        self.providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn identifies(provider: &dyn MediaSegmentProvider, id: &str) -> bool {
        media_segment_provider_id(provider.name()) == id
            || provider.legacy_ids().iter().any(|legacy| legacy == id)
    }
    fn provider(&self, id: &str) -> Option<Arc<dyn MediaSegmentProvider>> {
        self.providers()
            .into_iter()
            .find(|p| Self::identifies(p.as_ref(), id))
    }
    fn namespaces(provider: &dyn MediaSegmentProvider) -> Vec<String> {
        let mut ids = vec![media_segment_provider_id(provider.name())];
        ids.extend(provider.legacy_ids());
        ids
    }
    async fn options(&self, item: &BaseItemEntity) -> Result<LibraryOptions, ServiceError> {
        let Some(folders) = &self.virtual_folders else {
            return Ok(LibraryOptions::default());
        };
        let folders = folders.library_folders().await?;
        Ok(owning_library(
            &folders,
            item.top_parent_id.as_deref(),
            item.path.as_deref(),
        )
        .and_then(|folder| folder.library_options.clone())
        .unwrap_or_default())
    }
    fn enabled(provider: &dyn MediaSegmentProvider, options: &LibraryOptions) -> bool {
        !options.disabled_media_segment_providers.iter().any(|name| {
            ferrofin_util::string_extensions::equals_ordinal_ignore_case(name, provider.name())
        })
    }
    /// Maps a stored `MediaSegments` row onto the wire DTO (C#
    /// `MediaSegmentManager.Map`). An unnamed `Type` keeps its integer.
    fn to_dto(entity: MediaSegmentEntity) -> Result<MediaSegmentDto, ServiceError> {
        let guid = |column: &str, value: &str| {
            Uuid::parse_str(value).map_err(|e| {
                ServiceError::backend(format!("invalid guid in column `{column}`: {e}"))
            })
        };
        let MediaSegmentEntity {
            id,
            item_id,
            type_,
            start_ticks,
            end_ticks,
            ..
        } = entity;
        Ok(MediaSegmentDto {
            id: guid("MediaSegments.Id", &id)?,
            item_id: guid("MediaSegments.ItemId", &item_id)?,
            type_: MediaSegmentType::from_json_value(type_),
            start_ticks,
            end_ticks,
        })
    }
    async fn rows(&self, item: Uuid) -> Result<Vec<MediaSegmentEntity>, ServiceError> {
        self.selected_rows(Some(item), None).await
    }
    async fn selected_rows(
        &self,
        item: Option<Uuid>,
        provider: Option<&str>,
    ) -> Result<Vec<MediaSegmentEntity>, ServiceError> {
        let (sql, identifier) = if let Some(item) = item {
            (
                r#"SELECT * FROM "MediaSegments" WHERE "ItemId" = ?1 ORDER BY "StartTicks""#,
                guid_to_db(item),
            )
        } else if let Some(provider) = provider {
            (
                r#"SELECT * FROM "MediaSegments" WHERE "SegmentProviderId" = ?1 ORDER BY "StartTicks""#,
                provider.to_owned(),
            )
        } else {
            return Ok(Vec::new());
        };
        sqlx::query_as::<_, MediaSegmentEntity>(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(identifier)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)
    }
    async fn provider_rows(
        &self,
        item: Uuid,
        provider: &dyn MediaSegmentProvider,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        self.rows(item)
            .await?
            .into_iter()
            .filter(|row| Self::identifies(provider, &row.segment_provider_id))
            .map(Self::to_dto)
            .collect()
    }
    fn changed(revisions: &mut MutationState, item: Uuid) {
        let Some((watchers, revision)) = revisions.revisions.get_mut(&item) else {
            return;
        };
        if watchers.strong_count() == 0 {
            revisions.revisions.remove(&item);
        } else {
            *revision = revision.wrapping_add(1);
        }
    }
    /// Publishes `mutation` to the snapshot of the producer `provider_id`
    /// names, when one is registered, and returns that producer with its
    /// snapshot from before, for [`Self::restore_source`].
    async fn publish_source(
        &self,
        item: Uuid,
        provider_id: &str,
        mutation: &MediaSegmentCacheMutation,
    ) -> Result<Option<PublishedSource>, ServiceError> {
        let Some(provider) = self.provider(provider_id) else {
            return Ok(None);
        };
        let previous = self.publish_to(item, provider.as_ref(), mutation).await?;
        Ok(Some((provider, previous)))
    }
    /// Publishes `mutation` to `provider`'s snapshot and returns the snapshot
    /// from before it.
    async fn publish_to(
        &self,
        item: Uuid,
        provider: &dyn MediaSegmentProvider,
        mutation: &MediaSegmentCacheMutation,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        let existing = self.provider_rows(item, provider).await?;
        let previous = provider.cached_segments(item, &existing).await?;
        provider.mutate_cache(item, &existing, mutation).await?;
        Ok(previous)
    }
    /// Returns `written`, the served-row write that followed a publication.
    /// When it failed, first puts back the producer's snapshot from before
    /// the publication, so the producer does not report a write its rows
    /// never took (gl-1356); a restore that fails too is logged.
    async fn restore_source<T>(
        item: Uuid,
        source: Option<PublishedSource>,
        written: Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        if written.is_err()
            && let Some((provider, previous)) = source
            && let Err(restore_error) = provider.cache_segments(item, &previous).await
        {
            tracing::warn!(item_id = %item, provider = provider.name(), %restore_error, "could not restore the producer snapshot after its served rows failed to change");
        }
        written
    }
    fn loaded_provider(
        &self,
        candidate: Arc<dyn MediaSegmentProvider>,
    ) -> Result<Arc<dyn MediaSegmentProvider>, ServiceError> {
        let Some(registered) = self.provider(&media_segment_provider_id(candidate.name())) else {
            return Ok(candidate);
        };
        let namespaces = candidate.legacy_ids();
        if !namespaces.is_empty()
            && !namespaces
                .iter()
                .any(|namespace| registered.legacy_ids().contains(namespace))
        {
            return Err(ServiceError::invalid_input(format!(
                "media-segment provider name {} belongs to another loaded producer",
                candidate.name()
            )));
        }
        Ok(registered)
    }
    fn provider_id(&self, identifier: &str) -> String {
        self.provider(identifier).map_or_else(
            || identifier.to_owned(),
            |provider| media_segment_provider_id(provider.name()),
        )
    }
    async fn replace_provider_namespaces(
        &self,
        item: Uuid,
        identifier: &str,
        type_filter: Option<MediaSegmentType>,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        let ids = self.provider(identifier).map_or_else(
            || vec![identifier.to_owned()],
            |provider| Self::namespaces(provider.as_ref()),
        );
        media_segment_repository::replace_rows(
            &self.db,
            item,
            &ids,
            type_filter,
            &self.provider_id(identifier),
            segments,
        )
        .await
    }
    async fn delete_stored(
        &self,
        segment_id: Uuid,
    ) -> Result<Option<MediaSegmentEntity>, ServiceError> {
        sqlx::query_as::<_, MediaSegmentEntity>(
            r#"DELETE FROM "MediaSegments" WHERE "Id" = ?1 RETURNING *"#,
        )
        .bind(guid_to_db(segment_id))
        .fetch_optional(self.db.writer())
        .await
        .map_err(db_err)
    }
    async fn clear_all_provider(
        &self,
        provider_id: &str,
        types: Option<MediaSegmentType>,
        revisions: &mut MutationState,
    ) -> Result<(), ServiceError> {
        let ids = self.provider(provider_id).map_or_else(
            || vec![provider_id.to_owned()],
            |p| Self::namespaces(p.as_ref()),
        );
        for item in media_segment_repository::clear_rows(&self.db, &ids, types).await? {
            Self::changed(revisions, item);
        }
        Ok(())
    }
    async fn create_stored(
        &self,
        segment: &MediaSegmentDto,
        provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        let id = if segment.id.is_nil() {
            Uuid::new_v4()
        } else {
            segment.id
        };
        let stored = MediaSegmentDto {
            id,
            ..segment.clone()
        };
        media_segment_repository::replace_rows(
            &self.db,
            stored.item_id,
            &[],
            None,
            provider_id,
            std::slice::from_ref(&stored),
        )
        .await?;
        Ok(stored)
    }
    async fn clear_stored(&self, item: Uuid) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "MediaSegments" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item))
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
    async fn run_provider(
        &self,
        item: Uuid,
        provider: &dyn MediaSegmentProvider,
        overwrite: bool,
    ) -> Result<(), ServiceError> {
        let (previous, (run, revision)) = {
            let mut revisions = self.mutations.lock().await;
            let previous = if overwrite {
                Vec::new()
            } else {
                self.provider_rows(item, provider).await?
            };
            (previous, revisions.watch(item))
        };
        let segments = provider
            .get_segments(&MediaSegmentGenerationRequest {
                item_id: item,
                existing_segments: previous.clone(),
            })
            .await;
        let mut revisions = self.mutations.lock().await;
        let current = revisions.unwatch(item, run);
        let segments = segments?;
        if current != revision {
            tracing::debug!(item_id=%item, provider=provider.name(), "ignoring extraction superseded by a segment mutation");
            return Ok(());
        }
        if segments.len() == previous.len()
            && segments.iter().all(|segment| {
                previous.iter().any(|old| {
                    old.start_ticks == segment.start_ticks
                        && old.end_ticks == segment.end_ticks
                        && old.type_ == segment.type_
                })
            })
        {
            return Ok(());
        }
        Self::changed(&mut revisions, item);
        let segments: Vec<_> = segments
            .into_iter()
            .map(|segment| MediaSegmentDto {
                id: Uuid::new_v4(),
                item_id: item,
                ..segment
            })
            .collect();
        media_segment_repository::replace_rows(
            &self.db,
            item,
            &Self::namespaces(provider),
            None,
            &media_segment_provider_id(provider.name()),
            &segments,
        )
        .await
    }
}
#[async_trait]
impl MediaSegmentManager for FerrofinMediaSegmentManager {
    fn register_segment_provider(
        &self,
        provider: Arc<dyn MediaSegmentProvider>,
    ) -> Arc<dyn MediaSegmentProvider> {
        let mut providers = self
            .providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = media_segment_provider_id(provider.name());
        if let Some(registered) = providers
            .iter()
            .find(|registered| media_segment_provider_id(registered.name()) == id)
        {
            return Arc::clone(registered);
        }
        providers.push(Arc::clone(&provider));
        providers.sort_by_key(|provider| provider.order());
        provider
    }
    fn is_registered_segment_provider(&self, identifier: &str) -> bool {
        self.provider(identifier).is_some()
    }
    async fn adopt_loaded_segment_provider(
        &self,
        candidate: Arc<dyn MediaSegmentProvider>,
    ) -> Result<bool, ServiceError> {
        let _guard = self.mutations.lock().await;
        let mut output: std::collections::HashMap<Uuid, Vec<MediaSegmentDto>> =
            std::collections::HashMap::new();
        for namespace in candidate.legacy_ids() {
            for row in self.selected_rows(None, Some(&namespace)).await? {
                let dto = Self::to_dto(row)?;
                output.entry(dto.item_id).or_default().push(dto);
            }
        }
        if output.is_empty() {
            return Ok(false);
        }
        let provider = match self.loaded_provider(candidate) {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(%error, "could not register loaded legacy segment producer");
                return Ok(false);
            }
        };
        for (item, segments) in output {
            if let Err(error) = provider.initialize_cache(item, &segments).await {
                // Served rows are themselves valid role proof and remain intact on failure.
                tracing::warn!(item_id=%item, provider=provider.name(), %error, "could not adopt legacy producer output");
            }
        }
        self.register_segment_provider(provider);
        Ok(true)
    }
    fn registered_segment_providers(&self) -> Vec<MediaSegmentProviderInfo> {
        self.providers()
            .iter()
            .map(|provider| MediaSegmentProviderInfo {
                name: provider.name().to_owned(),
                id: media_segment_provider_id(provider.name()),
            })
            .collect()
    }
    async fn is_provider_enabled(
        &self,
        item_id: Uuid,
        identifier: &str,
    ) -> Result<bool, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(false);
        };
        let name = self
            .provider(identifier)
            .map_or_else(|| identifier.to_owned(), |p| p.name().to_owned());
        Ok(!self
            .options(&item)
            .await?
            .disabled_media_segment_providers
            .iter()
            .any(|disabled| {
                ferrofin_util::string_extensions::equals_ordinal_ignore_case(disabled, &name)
            }))
    }
    async fn is_type_supported(&self, item_id: Uuid) -> Result<bool, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(false);
        };
        Ok(kind_from_type_name(&item.type_).is_some_and(|kind| is_video(kind) || is_audio(kind)))
    }
    async fn create_segment(
        &self,
        segment: &MediaSegmentDto,
        provider_id: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        let mut revisions = self.mutations.lock().await;
        let created = self
            .create_stored(segment, &self.provider_id(provider_id))
            .await?;
        Self::changed(&mut revisions, segment.item_id);
        Ok(created)
    }
    async fn delete_segment(&self, segment_id: Uuid) -> Result<(), ServiceError> {
        let mut revisions = self.mutations.lock().await;
        if let Some(row) = self.delete_stored(segment_id).await? {
            Self::changed(&mut revisions, Self::to_dto(row)?.item_id);
        }
        Ok(())
    }
    async fn delete_segments(&self, item: Uuid) -> Result<(), ServiceError> {
        let mut revisions = self.mutations.lock().await;
        for provider in self.providers() {
            if let Err(error) = provider.cleanup(item).await {
                tracing::warn!(provider=provider.name(), %error, "media-segment cleanup failed");
            }
        }
        Self::changed(&mut revisions, item);
        self.clear_stored(item).await
    }
    async fn delete_provider_segments(
        &self,
        item: Uuid,
        provider_id: &str,
        types: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        let mut revisions = self.mutations.lock().await;
        Self::changed(&mut revisions, item);
        self.replace_provider_namespaces(item, provider_id, types, &[])
            .await
    }
    async fn delete_all_provider_segments(
        &self,
        provider_id: &str,
        types: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        let mut revisions = self.mutations.lock().await;
        revisions.epoch = revisions.epoch.wrapping_add(1);
        self.clear_all_provider(provider_id, types, &mut revisions)
            .await
    }
    async fn get_producer_segments(
        &self,
        item: Uuid,
        identifier: &str,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        let Some(provider) = self.provider(identifier) else {
            return self
                .rows(item)
                .await?
                .into_iter()
                .filter(|row| row.segment_provider_id == identifier)
                .map(Self::to_dto)
                .collect();
        };
        let existing = self.provider_rows(item, provider.as_ref()).await?;
        provider.cached_segments(item, &existing).await
    }
    async fn replace_producer_segments(
        &self,
        item: Uuid,
        identifier: &str,
        type_filter: Option<MediaSegmentType>,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        if segments
            .iter()
            .any(|segment| segment.end_ticks < segment.start_ticks)
        {
            return Err(ServiceError::invalid_input(
                "segment end precedes its start",
            ));
        }
        let segments: Vec<_> = segments
            .iter()
            .cloned()
            .map(|mut segment| {
                segment.item_id = item;
                if segment.id.is_nil() {
                    segment.id = Uuid::new_v4();
                }
                segment
            })
            .collect();
        let mut revisions = self.mutations.lock().await;
        let source = self
            .publish_source(
                item,
                identifier,
                &MediaSegmentCacheMutation::Replace {
                    type_filter,
                    segments: segments.clone(),
                },
            )
            .await?;
        Self::changed(&mut revisions, item);
        let written = self
            .replace_provider_namespaces(item, identifier, type_filter, &segments)
            .await;
        Self::restore_source(item, source, written).await
    }
    async fn replace_loaded_producer_segments(
        &self,
        item: Uuid,
        candidate: Arc<dyn MediaSegmentProvider>,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        if segments
            .iter()
            .any(|segment| segment.end_ticks < segment.start_ticks)
        {
            return Err(ServiceError::invalid_input(
                "segment end precedes its start",
            ));
        }
        let provider_id = media_segment_provider_id(candidate.name());
        let provider = self.loaded_provider(candidate)?;
        let segments: Vec<_> = segments
            .iter()
            .cloned()
            .map(|mut segment| {
                segment.item_id = item;
                if segment.id.is_nil() {
                    segment.id = Uuid::new_v4();
                }
                segment
            })
            .collect();
        let mut revisions = self.mutations.lock().await;
        let previous = self
            .publish_to(
                item,
                provider.as_ref(),
                &MediaSegmentCacheMutation::Replace {
                    type_filter: None,
                    segments: segments.clone(),
                },
            )
            .await?;
        Self::changed(&mut revisions, item);
        let written = media_segment_repository::replace_rows(
            &self.db,
            item,
            &Self::namespaces(provider.as_ref()),
            None,
            &provider_id,
            &segments,
        )
        .await;
        Self::restore_source(item, Some((provider.clone(), previous)), written).await?;
        self.register_segment_provider(provider);
        Ok(())
    }
    async fn create_producer_segment(
        &self,
        segment: &MediaSegmentDto,
        identifier: &str,
    ) -> Result<MediaSegmentDto, ServiceError> {
        if segment.end_ticks < segment.start_ticks {
            return Err(ServiceError::invalid_input(
                "segment end precedes its start",
            ));
        }
        let mut segment = segment.clone();
        if segment.id.is_nil() {
            segment.id = Uuid::new_v4();
        }
        let mutation = if segment.type_ == MediaSegmentType::Commercial {
            MediaSegmentCacheMutation::Append(segment.clone())
        } else {
            MediaSegmentCacheMutation::Replace {
                type_filter: Some(segment.type_),
                segments: vec![segment.clone()],
            }
        };
        let item = segment.item_id;
        let mut revisions = self.mutations.lock().await;
        let source = self.publish_source(item, identifier, &mutation).await?;
        Self::changed(&mut revisions, item);
        let written = if segment.type_ == MediaSegmentType::Commercial {
            self.create_stored(&segment, &self.provider_id(identifier))
                .await
        } else {
            self.replace_provider_namespaces(
                item,
                identifier,
                Some(segment.type_),
                std::slice::from_ref(&segment),
            )
            .await
            .map(|()| segment)
        };
        Self::restore_source(item, source, written).await
    }
    async fn delete_producer_segment(
        &self,
        item: Uuid,
        identifier: &str,
        segment_id: Uuid,
        type_filter: MediaSegmentType,
    ) -> Result<(), ServiceError> {
        let mut revisions = self.mutations.lock().await;
        let existing = self
            .rows(item)
            .await?
            .into_iter()
            .find(|row| Uuid::parse_str(&row.id).ok() == Some(segment_id));
        let mutation = if let Some(row) = existing {
            let mut segment = Self::to_dto(row)?;
            segment.type_ = type_filter;
            // Intro Skipper Plugin.DeleteTimestampAsync uses a 0.001-second tolerance.
            MediaSegmentCacheMutation::Remove {
                segment,
                tolerance_ticks: 10_000,
            }
        } else {
            MediaSegmentCacheMutation::Replace {
                type_filter: Some(type_filter),
                segments: Vec::new(),
            }
        };
        let source = self.publish_source(item, identifier, &mutation).await?;
        Self::changed(&mut revisions, item);
        let written = self.delete_stored(segment_id).await.map(drop);
        Self::restore_source(item, source, written).await
    }
    async fn delete_all_producer_segments(
        &self,
        identifier: &str,
        type_filter: Option<MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        let mut revisions = self.mutations.lock().await;
        revisions.epoch = revisions.epoch.wrapping_add(1);
        // Rows first, all or none, so a failed clear leaves every row and snapshot. Unlike
        // the publish-first producer paths: putting every item's snapshot back after a
        // failed clear could be large (gl-1401). If the snapshot erase then fails, the next
        // scan serves the snapshots it did not reach again, and a retry completes the
        // erase unless the snapshot walk keeps failing.
        self.clear_all_provider(identifier, type_filter, &mut revisions)
            .await?;
        if let Some(provider) = self.provider(identifier) {
            provider.erase_cached_segments(type_filter).await?;
        }
        Ok(())
    }
    async fn get_segments(
        &self,
        item_id: Uuid,
        types: Option<&[MediaSegmentType]>,
        filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(Vec::new());
        };
        self.get_item_segments(&item, types, filter_by_provider)
            .await
    }
    async fn get_item_segments(
        &self,
        item: &BaseItemEntity,
        types: Option<&[MediaSegmentType]>,
        filter_by_provider: bool,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        let item_id = Uuid::parse_str(&item.id)
            .map_err(|_| ServiceError::invalid_input("the item's id is not a GUID"))?;
        let providers = if filter_by_provider {
            let options = self.options(item).await?;
            self.providers()
                .into_iter()
                .filter(|p| Self::enabled(p.as_ref(), &options))
                .collect()
        } else {
            Vec::new()
        };
        if filter_by_provider && providers.is_empty() {
            return Ok(Vec::new());
        }
        self.rows(item_id)
            .await?
            .into_iter()
            .filter(|row| {
                !filter_by_provider
                    || providers
                        .iter()
                        .any(|p| Self::identifies(p.as_ref(), &row.segment_provider_id))
            })
            .map(Self::to_dto)
            .filter_map(|dto| match dto {
                Ok(dto) if types.is_some_and(|types| !types.contains(&dto.type_)) => None,
                other => Some(other),
            })
            .collect()
    }
    async fn has_segments(&self, item: Uuid) -> Result<bool, ServiceError> {
        let count: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "MediaSegments" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(item))
                .fetch_one(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(count > 0)
    }
    async fn get_supported_providers(
        &self,
        item: Uuid,
    ) -> Result<Vec<MediaSegmentProviderInfo>, ServiceError> {
        Ok(if self.is_type_supported(item).await? {
            self.registered_segment_providers()
        } else {
            Vec::new()
        })
    }
    async fn run_segment_providers(
        &self,
        item_id: Uuid,
        overwrite: bool,
    ) -> Result<usize, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(item_id).await? else {
            return Ok(0);
        };
        let options = self.options(&item).await?;
        let mut providers: Vec<_> = self
            .providers()
            .into_iter()
            .filter(|provider| Self::enabled(provider.as_ref(), &options))
            .collect();
        providers.sort_by_key(|provider| {
            options
                .media_segment_provider_order
                .iter()
                .position(|name| name == provider.name())
                .unwrap_or(usize::MAX)
        });
        if providers.is_empty() {
            return Ok(0);
        }
        if overwrite {
            let mut revisions = self.mutations.lock().await;
            // The clear below erases every namespace, including disabled providers.
            // Adopt every loaded producer's older output before deleting served rows;
            // the library policy controls replay, not retention of producer data.
            let rows = self.rows(item_id).await?;
            for provider in self.providers() {
                let existing = rows
                    .iter()
                    .filter(|row| Self::identifies(provider.as_ref(), &row.segment_provider_id))
                    .cloned()
                    .map(Self::to_dto)
                    .collect::<Result<Vec<_>, _>>()?;
                provider.initialize_cache(item_id, &existing).await?;
            }
            Self::changed(&mut revisions, item_id);
            self.clear_stored(item_id).await?;
        }
        let mut ran = 0;
        for provider in providers {
            if !provider.supports(&item).await? {
                continue;
            }
            ran += 1;
            if let Err(error) = self
                .run_provider(item_id, provider.as_ref(), overwrite)
                .await
            {
                tracing::warn!(provider=provider.name(), %error, "media-segment provider failed");
            }
        }
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use ferrofin_db::entities::playback::MediaSegmentEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use uuid::Uuid;

    use ferrofin_traits::media_segments::MediaSegmentManager;

    use crate::test_support::{library_manager_over, seed_item, test_db};

    use super::FerrofinMediaSegmentManager;

    /// A stored row with the given `Type` discriminant.
    fn stored(type_: i32) -> MediaSegmentEntity {
        MediaSegmentEntity {
            id: Uuid::from_u128(0x52).to_string(),
            end_ticks: 6_000_000,
            item_id: Uuid::from_u128(0x50).to_string(),
            segment_provider_id: "chapter-provider".to_owned(),
            start_ticks: 0,
            type_,
        }
    }

    #[test]
    fn a_stored_row_maps_its_guids_ticks_and_every_type() {
        let expected = [
            MediaSegmentType::Unknown,
            MediaSegmentType::Commercial,
            MediaSegmentType::Preview,
            MediaSegmentType::Recap,
            MediaSegmentType::Outro,
            MediaSegmentType::Intro,
            MediaSegmentType::Unrecognized(99),
        ];
        for (value, want) in [0, 1, 2, 3, 4, 5, 99].into_iter().zip(expected) {
            let dto = FerrofinMediaSegmentManager::to_dto(stored(value)).expect("convert");
            assert_eq!(dto.type_, want, "discriminant {value}");
            assert_eq!(dto.id, Uuid::from_u128(0x52));
            assert_eq!(dto.item_id, Uuid::from_u128(0x50));
            assert_eq!((dto.start_ticks, dto.end_ticks), (0, 6_000_000));
        }
    }

    #[test]
    fn a_stored_row_with_a_malformed_guid_is_an_error() {
        let row = MediaSegmentEntity {
            item_id: "not-a-guid".to_owned(),
            ..stored(5)
        };
        let error = FerrofinMediaSegmentManager::to_dto(row).expect_err("bad guid");
        assert!(
            error.to_string().contains("MediaSegments.ItemId"),
            "{error}"
        );
    }

    fn segment(item: Uuid, kind: MediaSegmentType, start: i64, end: i64) -> MediaSegmentDto {
        MediaSegmentDto {
            id: Uuid::nil(),
            item_id: item,
            type_: kind,
            start_ticks: start,
            end_ticks: end,
        }
    }

    #[tokio::test]
    async fn create_query_and_delete_round_trip() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let mgr = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db.clone()));

        assert!(!mgr.has_segments(item).await.expect("empty"));

        let created = mgr
            .create_segment(&segment(item, MediaSegmentType::Intro, 0, 100), "prov")
            .await
            .expect("create");
        assert!(!created.id.is_nil());

        mgr.create_segment(&segment(item, MediaSegmentType::Outro, 900, 1000), "prov")
            .await
            .expect("create outro");

        assert!(mgr.has_segments(item).await.expect("has"));

        let all = mgr.get_segments(item, None, false).await.expect("all");
        assert_eq!(all.len(), 2);
        // Ordered by start ticks.
        assert_eq!(all[0].type_, MediaSegmentType::Intro);

        // Type filter narrows to the requested kind.
        let outros = mgr
            .get_segments(item, Some(&[MediaSegmentType::Outro]), false)
            .await
            .expect("outros");
        assert_eq!(outros.len(), 1);
        assert_eq!(outros[0].type_, MediaSegmentType::Outro);

        mgr.delete_segment(created.id).await.expect("delete one");
        assert_eq!(
            mgr.get_segments(item, None, false)
                .await
                .expect("after")
                .len(),
            1
        );

        mgr.delete_segments(item).await.expect("delete all");
        assert!(!mgr.has_segments(item).await.expect("empty again"));
    }

    #[tokio::test]
    async fn a_replacement_that_fails_part_way_keeps_the_previous_rows() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let mgr = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db.clone()));
        let intro = mgr
            .create_segment(&segment(item, MediaSegmentType::Intro, 0, 100), "prov")
            .await
            .unwrap();
        // Another provider's row: reusing its id makes the replacement's last INSERT fail.
        let taken = mgr
            .create_segment(&segment(item, MediaSegmentType::Outro, 900, 1000), "other")
            .await
            .unwrap();
        let clash = MediaSegmentDto {
            id: taken.id,
            ..segment(item, MediaSegmentType::Intro, 10, 110)
        };
        let stored = || async {
            let mut ids: Vec<_> = mgr
                .get_segments(item, None, false)
                .await
                .unwrap()
                .into_iter()
                .map(|segment| segment.id)
                .collect();
            ids.sort();
            ids
        };
        let mut before = vec![intro.id, taken.id];
        before.sort();

        let replacement = [
            segment(item, MediaSegmentType::Intro, 5, 105),
            clash.clone(),
        ];
        assert!(
            mgr.replace_producer_segments(item, "prov", None, &replacement)
                .await
                .is_err()
        );
        assert_eq!(stored().await, before, "replace_producer_segments");

        assert!(mgr.create_producer_segment(&clash, "prov").await.is_err());
        assert_eq!(stored().await, before, "create_producer_segment");

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let fake = provider("Fake", 0, 200, &events);
        mgr.register_segment_provider(fake.clone());
        mgr.run_segment_providers(item, false).await.unwrap();
        let ran = stored().await;
        assert_eq!(ran.len(), 3);
        // An invalid segment from a provider run is refused before its old rows go.
        *fake.output.lock().unwrap() = vec![segment(item, MediaSegmentType::Intro, 300, 299)];
        mgr.run_segment_providers(item, false).await.unwrap();
        assert_eq!(stored().await, ran, "run_provider");

        assert!(
            mgr.replace_loaded_producer_segments(item, fake.clone(), &replacement)
                .await
                .is_err()
        );
        assert_eq!(stored().await, ran, "replace_loaded_producer_segments");
    }

    #[tokio::test]
    async fn type_support_and_no_providers() {
        let db = test_db().await;
        let episode = Uuid::new_v4();
        let series = Uuid::new_v4();
        let audio = Uuid::new_v4();
        let audiobook = Uuid::new_v4();
        seed_item(&db, audio, BaseItemKind::Audio).await;
        seed_item(&db, audiobook, BaseItemKind::AudioBook).await;
        seed_item(&db, episode, BaseItemKind::Episode).await;
        seed_item(&db, series, BaseItemKind::Series).await;
        let mgr = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db.clone()));

        assert!(mgr.is_type_supported(episode).await.expect("episode"));
        assert!(!mgr.is_type_supported(series).await.expect("series"));
        assert!(mgr.is_type_supported(audio).await.unwrap());
        assert!(mgr.is_type_supported(audiobook).await.unwrap());
        assert!(!mgr.is_type_supported(Uuid::new_v4()).await.unwrap());
        let supported_providers = mgr
            .get_supported_providers(episode)
            .await
            .expect("providers");
        assert!(supported_providers.is_empty(), "{supported_providers:?}");
        // An install with no registered producer runs none.
        assert_eq!(
            mgr.run_segment_providers(episode, false)
                .await
                .expect("run providers"),
            0
        );
    }
    struct FakeProvider {
        name: &'static str,
        order: i32,
        output: std::sync::Mutex<Vec<MediaSegmentDto>>,
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        requests:
            std::sync::Mutex<Vec<ferrofin_model::media_segments::MediaSegmentGenerationRequest>>,
        supports: bool,
        fail: bool,
        publish_fail: std::sync::atomic::AtomicBool,
        pause: Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    }
    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl ferrofin_traits::media_segments::MediaSegmentProvider for FakeProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn order(&self) -> i32 {
            self.order
        }
        fn legacy_ids(&self) -> Vec<String> {
            vec![format!("legacy:{}", self.name)]
        }
        async fn supports(
            &self,
            _item: &ferrofin_db::entities::base_items::BaseItemEntity,
        ) -> Result<bool, ferrofin_traits::error::ServiceError> {
            Ok(self.supports)
        }
        async fn get_segments(
            &self,
            request: &ferrofin_model::media_segments::MediaSegmentGenerationRequest,
        ) -> Result<Vec<MediaSegmentDto>, ferrofin_traits::error::ServiceError> {
            self.events.lock().unwrap().push(self.name.to_owned());
            self.requests.lock().unwrap().push(request.clone());
            if self.fail {
                return Err(ferrofin_traits::error::ServiceError::backend(
                    "provider failed",
                ));
            }
            let output = self.output.lock().unwrap().clone();
            if let Some(pause) = &self.pause {
                pause.0.notify_one();
                pause.1.notified().await;
            }
            Ok(output)
        }
        async fn cache_segments(
            &self,
            _item: Uuid,
            segments: &[MediaSegmentDto],
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            *self.output.lock().unwrap() = segments.to_vec();
            Ok(())
        }
        async fn cached_segments(
            &self,
            _item: Uuid,
            _existing: &[MediaSegmentDto],
        ) -> Result<Vec<MediaSegmentDto>, ferrofin_traits::error::ServiceError> {
            Ok(self.output.lock().unwrap().clone())
        }
        async fn mutate_cache(
            &self,
            _item: Uuid,
            _existing: &[MediaSegmentDto],
            mutation: &ferrofin_traits::media_segments::MediaSegmentCacheMutation,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            if self.publish_fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ferrofin_traits::error::ServiceError::backend(
                    "snapshot publication failed",
                ));
            }
            let mut output = self.output.lock().unwrap();
            *output = mutation.apply(&output);
            Ok(())
        }
        async fn erase_cached_segments(
            &self,
            type_filter: Option<MediaSegmentType>,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            if self.publish_fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ferrofin_traits::error::ServiceError::backend(
                    "snapshot erase failed",
                ));
            }
            let mut output = self.output.lock().unwrap();
            *output = ferrofin_traits::media_segments::MediaSegmentCacheMutation::Replace {
                type_filter,
                segments: Vec::new(),
            }
            .apply(&output);
            Ok(())
        }
        async fn cleanup(&self, _item: Uuid) -> Result<(), ferrofin_traits::error::ServiceError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("cleanup:{}", self.name));
            Ok(())
        }
    }
    fn provider(
        name: &'static str,
        order: i32,
        start: i64,
        events: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> std::sync::Arc<FakeProvider> {
        std::sync::Arc::new(FakeProvider {
            name,
            order,
            output: std::sync::Mutex::new(vec![segment(
                Uuid::new_v4(),
                MediaSegmentType::Intro,
                start,
                start + 100,
            )]),
            events: events.clone(),
            requests: std::sync::Mutex::new(Vec::new()),
            supports: true,
            fail: false,
            publish_fail: std::sync::atomic::AtomicBool::new(false),
            pause: None,
        })
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn registered_providers_follow_live_owning_library_settings() {
        use ferrofin_model::configuration::LibraryOptions;
        use ferrofin_model::configuration::MediaPathInfo;
        use ferrofin_traits::library::VirtualFolderManager;
        use ferrofin_traits::persistence::ItemPersistenceService;
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let library = library_manager_over(db.clone());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let mut row = library.get_item_by_id(item).await.unwrap().unwrap();
        row.path = Some(inner.join("episode.mkv").to_string_lossy().into_owned());
        row.top_parent_id = Some(Uuid::new_v4().to_string());
        crate::FerrofinItemPersistenceService::new(db.clone())
            .save_items(&[row])
            .await
            .unwrap();
        let folders = Arc::new(crate::FerrofinVirtualFolderManager::new(
            tmp.path().join("views"),
        ));
        let mut options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: inner.to_string_lossy().into_owned(),
            }],
            ..Default::default()
        };
        folders
            .add_virtual_folder(
                "A outer",
                None,
                &LibraryOptions {
                    path_infos: vec![MediaPathInfo {
                        path: root.to_string_lossy().into_owned(),
                    }],
                    disabled_media_segment_providers: vec![
                        "Alpha".to_owned(),
                        "Beta".to_owned(),
                        "Gamma".to_owned(),
                    ],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        folders
            .add_virtual_folder("Z inner", None, &options)
            .await
            .unwrap();
        let mgr =
            FerrofinMediaSegmentManager::new(db, library).with_virtual_folders(folders.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        for p in [
            provider("Alpha", 10, 30, &events),
            provider("Beta", 0, 10, &events),
            provider("Gamma", 0, 20, &events),
        ] {
            mgr.register_segment_provider(p);
        }
        assert_eq!(
            mgr.registered_segment_providers()
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Beta", "Gamma", "Alpha"]
        );
        assert_eq!(mgr.run_segment_providers(item, false).await.unwrap(), 3);
        assert_eq!(*events.lock().unwrap(), ["Beta", "Gamma", "Alpha"]);
        mgr.create_segment(
            &segment(item, MediaSegmentType::Outro, 900, 1000),
            "unregistered",
        )
        .await
        .unwrap();
        assert_eq!(mgr.get_segments(item, None, true).await.unwrap().len(), 3);
        let segments = mgr.get_segments(item, Some(&[]), false).await.unwrap();
        assert!(segments.is_empty(), "{segments:?}");
        options.disabled_media_segment_providers = vec!["bEtA".to_owned()];
        options.media_segment_provider_order = vec!["Alpha".to_owned(), "Gamma".to_owned()];
        folders
            .update_library_options("Z inner", &options)
            .await
            .unwrap();
        events.lock().unwrap().clear();
        assert_eq!(mgr.run_segment_providers(item, false).await.unwrap(), 2);
        assert_eq!(*events.lock().unwrap(), ["Alpha", "Gamma"]);
        assert_eq!(mgr.get_segments(item, None, true).await.unwrap().len(), 2);
        assert_eq!(mgr.get_segments(item, None, false).await.unwrap().len(), 4);
        assert!(!mgr.is_provider_enabled(item, "legacy:Beta").await.unwrap());
        assert_eq!(mgr.get_supported_providers(item).await.unwrap().len(), 3);
        options.disabled_media_segment_providers.clear();
        options.media_segment_provider_order = vec!["alpha".to_owned()];
        folders
            .update_library_options("Z inner", &options)
            .await
            .unwrap();
        events.lock().unwrap().clear();
        mgr.run_segment_providers(item, false).await.unwrap();
        assert_eq!(*events.lock().unwrap(), ["Beta", "Gamma", "Alpha"]);
        options.disabled_media_segment_providers =
            vec!["alpha".to_owned(), "BETA".to_owned(), "gamma".to_owned()];
        folders
            .update_library_options("Z inner", &options)
            .await
            .unwrap();
        assert_eq!(mgr.run_segment_providers(item, true).await.unwrap(), 0);
        assert_eq!(mgr.get_segments(item, None, false).await.unwrap().len(), 4);
        let segments = mgr.get_segments(item, None, true).await.unwrap();
        assert!(segments.is_empty(), "{segments:?}");
    }

    #[tokio::test]
    async fn providers_preserve_unchanged_rows_and_contain_extraction_failures() {
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let mgr = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db));
        let events = Arc::new(Mutex::new(Vec::new()));
        let good = provider("Good", 0, 5, &events);
        let mut failed = provider("Failed", -1, 10, &events);
        Arc::get_mut(&mut failed).unwrap().fail = true;
        let mut unsupported = provider("Unsupported", -2, 20, &events);
        Arc::get_mut(&mut unsupported).unwrap().supports = false;
        mgr.register_segment_provider(good.clone());
        mgr.register_segment_provider(failed);
        mgr.register_segment_provider(unsupported);
        assert_eq!(mgr.run_segment_providers(item, false).await.unwrap(), 2);
        let first = mgr.get_segments(item, None, true).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].item_id, item);
        mgr.run_segment_providers(item, false).await.unwrap();
        assert_eq!(mgr.get_segments(item, None, true).await.unwrap(), first);
        assert_eq!(good.requests.lock().unwrap()[1].existing_segments, first);
        mgr.create_segment(&segment(item, MediaSegmentType::Outro, 999, 1000), "user")
            .await
            .unwrap();
        mgr.run_segment_providers(item, true).await.unwrap();
        let overwritten = mgr.get_segments(item, None, false).await.unwrap();
        assert_eq!(overwritten.len(), 1);
        assert_ne!(overwritten[0].id, first[0].id);
        let existing_segments = good.requests.lock().unwrap()[2].existing_segments.clone();
        assert!(existing_segments.is_empty(), "{existing_segments:?}");
        good.output.lock().unwrap().clear();
        mgr.run_segment_providers(item, false).await.unwrap();
        assert!(!mgr.has_segments(item).await.unwrap());
        assert!(
            mgr.create_segment(&segment(item, MediaSegmentType::Intro, 10, 9), "user")
                .await
                .is_err()
        );
        mgr.delete_segments(item).await.unwrap();
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&"cleanup:Unsupported".to_owned())
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn registered_cache_adopts_legacy_rows_and_survives_overwrite_and_restart() {
        use ferrofin_traits::media_segments::{MediaSegmentProvider, media_segment_provider_id};
        use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
        use std::sync::Arc;
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let library = library_manager_over(db.clone());
        let tmp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let plugins: Arc<dyn PluginManager> = Arc::new(crate::FerrofinPluginManager::new(
            vec![crate::RegisteredPlugin::new(
                PluginDescriptor {
                    id,
                    name: "Intro Skipper".to_owned(),
                    enabled: true,
                    ..Default::default()
                },
                None,
            )],
            tmp.path().join("plugins"),
        ));
        let mgr = FerrofinMediaSegmentManager::new(db.clone(), library.clone());
        let old = mgr
            .create_segment(
                &segment(item, MediaSegmentType::Intro, 0, 100),
                "IntroSkipper",
            )
            .await
            .unwrap();
        mgr.create_segment(
            &segment(item, MediaSegmentType::Outro, 900, 1000),
            "wasm:unloaded",
        )
        .await
        .unwrap();
        let cached = Arc::new(
            crate::CachedMediaSegmentProvider::new(
                "Intro Skipper".to_owned(),
                id,
                plugins.clone(),
                vec![BaseItemKind::Episode],
                tmp.path().join("segments"),
            )
            .with_legacy_ids(vec!["IntroSkipper".to_owned()]),
        );
        mgr.register_segment_provider(cached.clone());
        assert_eq!(mgr.get_segments(item, None, true).await.unwrap(), [old]);
        assert_eq!(mgr.run_segment_providers(item, true).await.unwrap(), 1);
        let first = mgr.get_segments(item, None, true).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].end_ticks, 100);
        assert!(
            mgr.rows(item)
                .await
                .unwrap()
                .iter()
                .all(|row| row.segment_provider_id == media_segment_provider_id("Intro Skipper"))
        );
        let restarted = FerrofinMediaSegmentManager::new(db.clone(), library);
        restarted.register_segment_provider(Arc::new(
            crate::CachedMediaSegmentProvider::new(
                "Intro Skipper".to_owned(),
                id,
                plugins.clone(),
                vec![BaseItemKind::Episode],
                tmp.path().join("segments"),
            )
            .with_legacy_ids(vec!["IntroSkipper".to_owned()]),
        ));
        restarted.run_segment_providers(item, true).await.unwrap();
        assert_eq!(
            restarted.get_segments(item, None, true).await.unwrap()[0].end_ticks,
            100
        );
        let intact = restarted.get_segments(item, None, false).await.unwrap();
        std::fs::write(
            tmp.path().join(format!("segments/{item}.json")),
            b"invalid cache",
        )
        .unwrap();
        assert_eq!(
            restarted.run_segment_providers(item, false).await.unwrap(),
            1
        );
        assert_eq!(
            restarted.get_segments(item, None, false).await.unwrap(),
            intact,
            "a producer read error preserves served rows and is contained"
        );
        cached.cache_segments(item, &intact).await.unwrap();
        restarted
            .delete_segment(restarted.get_segments(item, None, false).await.unwrap()[0].id)
            .await
            .unwrap();
        restarted.run_segment_providers(item, true).await.unwrap();
        let restored = restarted.get_segments(item, None, true).await.unwrap();
        assert_eq!(
            restored.len(),
            1,
            "core row deletion leaves producer output intact"
        );
        restarted
            .delete_producer_segment(
                item,
                "IntroSkipper",
                restored[0].id,
                MediaSegmentType::Intro,
            )
            .await
            .unwrap();
        restarted.run_segment_providers(item, true).await.unwrap();
        assert!(!restarted.has_segments(item).await.unwrap());
        restarted
            .create_producer_segment(
                &segment(item, MediaSegmentType::Intro, 5, 105),
                "IntroSkipper",
            )
            .await
            .unwrap();
        restarted
            .create_producer_segment(
                &segment(item, MediaSegmentType::Outro, 800, 900),
                "IntroSkipper",
            )
            .await
            .unwrap();
        restarted
            .delete_all_producer_segments("IntroSkipper", Some(MediaSegmentType::Intro))
            .await
            .unwrap();
        restarted.run_segment_providers(item, true).await.unwrap();
        assert_eq!(
            restarted.get_segments(item, None, false).await.unwrap()[0].type_,
            MediaSegmentType::Outro
        );
        plugins.disable_plugin(id).await.unwrap();
        assert_eq!(
            restarted.run_segment_providers(item, false).await.unwrap(),
            0
        );
        plugins.enable_plugin(id).await.unwrap();
        restarted.delete_segments(item).await.unwrap();
        let segments = cached
            .get_segments(
                &ferrofin_model::media_segments::MediaSegmentGenerationRequest {
                    item_id: item,
                    existing_segments: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(segments.is_empty(), "{segments:?}");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn overwrite_adopts_disabled_legacy_output_before_another_provider_clears_rows() {
        use ferrofin_model::configuration::LibraryOptions;
        use ferrofin_model::configuration::MediaPathInfo;
        use ferrofin_traits::library::VirtualFolderManager;
        use ferrofin_traits::media_segments::{MediaSegmentProvider, media_segment_provider_id};
        use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
        use std::sync::{Arc, Mutex};

        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        crate::test_support::set_item_path(&db, item, &root.join("episode.mkv").to_string_lossy())
            .await;
        let folders = Arc::new(crate::FerrofinVirtualFolderManager::new(
            tmp.path().join("views"),
        ));
        let mut options = LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: root.to_string_lossy().into_owned(),
            }],
            disabled_media_segment_providers: vec!["iNtRo SkIpPeR".to_owned()],
            ..Default::default()
        };
        folders
            .add_virtual_folder("Library", None, &options)
            .await
            .unwrap();
        let plugin_id = Uuid::new_v4();
        let plugins: Arc<dyn PluginManager> = Arc::new(crate::FerrofinPluginManager::new(
            vec![crate::RegisteredPlugin::new(
                PluginDescriptor {
                    id: plugin_id,
                    name: "Intro Skipper".to_owned(),
                    enabled: true,
                    ..Default::default()
                },
                None,
            )],
            tmp.path().join("plugins"),
        ));
        let library = library_manager_over(db.clone());
        let manager = FerrofinMediaSegmentManager::new(db.clone(), library.clone())
            .with_virtual_folders(folders.clone());
        manager
            .create_segment(
                &segment(item, MediaSegmentType::Intro, 0, 100),
                "IntroSkipper",
            )
            .await
            .unwrap();
        let cached = Arc::new(
            crate::CachedMediaSegmentProvider::new(
                "Intro Skipper".to_owned(),
                plugin_id,
                plugins.clone(),
                vec![BaseItemKind::Episode],
                tmp.path().join("segments"),
            )
            .with_legacy_ids(vec!["IntroSkipper".to_owned()]),
        );
        manager.register_segment_provider(cached.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let active = provider("Éclair", 0, 300, &events);
        manager.register_segment_provider(active.clone());
        assert!(!tmp.path().join(format!("segments/{item}.json")).exists());
        assert_eq!(manager.run_segment_providers(item, true).await.unwrap(), 1);
        assert_eq!(*events.lock().unwrap(), ["Éclair"]);
        let served = manager.get_segments(item, None, true).await.unwrap();
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].start_ticks, 300);
        let retained = cached
            .get_segments(
                &ferrofin_model::media_segments::MediaSegmentGenerationRequest {
                    item_id: item,
                    existing_segments: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].end_ticks, 100);

        options.disabled_media_segment_providers.clear();
        folders
            .update_library_options("Library", &options)
            .await
            .unwrap();
        let restarted = FerrofinMediaSegmentManager::new(db, library).with_virtual_folders(folders);
        restarted.register_segment_provider(Arc::new(
            crate::CachedMediaSegmentProvider::new(
                "Intro Skipper".to_owned(),
                plugin_id,
                plugins,
                vec![BaseItemKind::Episode],
                tmp.path().join("segments"),
            )
            .with_legacy_ids(vec!["IntroSkipper".to_owned()]),
        ));
        restarted.register_segment_provider(active);
        assert_eq!(
            restarted.run_segment_providers(item, false).await.unwrap(),
            2
        );
        let replayed = restarted.get_segments(item, None, true).await.unwrap();
        assert_eq!(replayed.len(), 2);
        assert!(
            replayed
                .iter()
                .any(|row| row.start_ticks == 0 && row.end_ticks == 100)
        );
        assert!(
            restarted
                .rows(item)
                .await
                .unwrap()
                .iter()
                .any(|row| row.segment_provider_id == media_segment_provider_id("Intro Skipper"))
        );
        let folders = restarted.virtual_folders.as_ref().unwrap();
        options.disabled_media_segment_providers = vec!["ÉCLAIR".to_owned()];
        folders
            .update_library_options("Library", &options)
            .await
            .unwrap();
        assert!(!restarted.is_provider_enabled(item, "Éclair").await.unwrap());
        assert_eq!(
            restarted
                .get_segments(item, None, true)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            restarted.run_segment_providers(item, false).await.unwrap(),
            1
        );
        restarted
            .delete_provider_segments(item, "IntroSkipper", None)
            .await
            .unwrap();
        let producer_segments = restarted
            .get_producer_segments(item, "IntroSkipper")
            .await
            .unwrap();
        assert!(!producer_segments.is_empty(), "{producer_segments:?}");
        restarted
            .delete_all_producer_segments("IntroSkipper", None)
            .await
            .unwrap();
        restarted.run_segment_providers(item, false).await.unwrap();
        assert!(
            restarted
                .get_producer_segments(item, "IntroSkipper")
                .await
                .unwrap()
                .is_empty(),
            "bulk producer erase reaches cache-only output after its served rows are gone"
        );
    }

    /// A producer write whose served rows fail after its snapshot was
    /// published puts the previous snapshot back, as a deletion does, so the
    /// producer does not report a write the rows never took (gl-1356).
    #[tokio::test]
    async fn a_producer_write_whose_rows_fail_restores_the_previous_snapshot() {
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db));
        let events = Arc::new(Mutex::new(Vec::new()));
        let producer = provider("Producer", 0, 0, &events);
        manager.register_segment_provider(producer.clone());
        manager
            .create_producer_segment(
                &segment(item, MediaSegmentType::Intro, 0, 100),
                "legacy:Producer",
            )
            .await
            .unwrap();
        // Another provider's row: reusing its id makes the producer's row write fail.
        let taken = manager
            .create_segment(&segment(item, MediaSegmentType::Outro, 900, 1000), "other")
            .await
            .unwrap();
        let clash = MediaSegmentDto {
            id: taken.id,
            ..segment(item, MediaSegmentType::Intro, 10, 110)
        };
        let snapshot = manager
            .get_producer_segments(item, "legacy:Producer")
            .await
            .unwrap();
        let rows = manager.get_segments(item, None, false).await.unwrap();
        let snapshot_now = || async {
            manager
                .get_producer_segments(item, "legacy:Producer")
                .await
                .unwrap()
        };

        assert!(
            manager
                .replace_producer_segments(
                    item,
                    "legacy:Producer",
                    None,
                    std::slice::from_ref(&clash)
                )
                .await
                .is_err()
        );
        assert_eq!(snapshot_now().await, snapshot, "replace_producer_segments");
        assert!(
            manager
                .create_producer_segment(&clash, "legacy:Producer")
                .await
                .is_err()
        );
        assert_eq!(snapshot_now().await, snapshot, "create_producer_segment");
        // A commercial is appended and stored by its own insert.
        let commercial = MediaSegmentDto {
            type_: MediaSegmentType::Commercial,
            ..clash.clone()
        };
        assert!(
            manager
                .create_producer_segment(&commercial, "legacy:Producer")
                .await
                .is_err()
        );
        assert_eq!(
            snapshot_now().await,
            snapshot,
            "create_producer_segment, a commercial"
        );
        assert!(
            manager
                .replace_loaded_producer_segments(
                    item,
                    producer.clone(),
                    std::slice::from_ref(&clash)
                )
                .await
                .is_err()
        );
        assert_eq!(
            snapshot_now().await,
            snapshot,
            "replace_loaded_producer_segments"
        );
        assert_eq!(
            manager.get_segments(item, None, false).await.unwrap(),
            rows,
            "the served rows keep the old set"
        );
    }

    /// A provider-wide producer erase whose row clear fails keeps the
    /// producer's snapshot and every namespace's rows (gl-1401).
    #[tokio::test]
    async fn a_producer_erase_whose_rows_fail_keeps_the_snapshot_and_every_row() {
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager =
            FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db.clone()));
        // Stored under the legacy namespace before its producer registers.
        manager
            .create_segment(
                &segment(item, MediaSegmentType::Outro, 900, 1000),
                "legacy:Producer",
            )
            .await
            .unwrap();
        manager.register_segment_provider(provider(
            "Producer",
            0,
            0,
            &Arc::new(Mutex::new(Vec::new())),
        ));
        manager
            .create_producer_segment(
                &segment(item, MediaSegmentType::Intro, 0, 100),
                "legacy:Producer",
            )
            .await
            .unwrap();
        // The legacy namespace is cleared after the producer's own id: make it fail.
        sqlx::query(
            r#"CREATE TRIGGER "FailLegacyDelete" BEFORE DELETE ON "MediaSegments"
               WHEN OLD."SegmentProviderId" = 'legacy:Producer'
               BEGIN SELECT RAISE(ABORT, 'injected'); END"#,
        )
        .execute(db.writer())
        .await
        .unwrap();
        let snapshot = manager
            .get_producer_segments(item, "legacy:Producer")
            .await
            .unwrap();
        let rows = manager.get_segments(item, None, false).await.unwrap();
        assert_eq!((snapshot.len(), rows.len()), (1, 2));

        assert!(
            manager
                .delete_all_producer_segments("legacy:Producer", None)
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .get_producer_segments(item, "legacy:Producer")
                .await
                .unwrap(),
            snapshot
        );
        assert_eq!(manager.get_segments(item, None, false).await.unwrap(), rows);
    }

    /// A producer erase whose snapshot erase fails after its rows were cleared
    /// keeps the snapshot, which the next scan serves again; a retry
    /// completes the erase (gl-1401).
    #[tokio::test]
    async fn a_producer_erase_whose_snapshot_fails_is_served_again_and_a_retry_completes_it() {
        use std::sync::{Arc, Mutex, atomic::Ordering};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db));
        let producer = provider("Producer", 0, 0, &Arc::new(Mutex::new(Vec::new())));
        manager.register_segment_provider(producer.clone());
        let written = manager
            .create_producer_segment(
                &segment(item, MediaSegmentType::Intro, 0, 100),
                "legacy:Producer",
            )
            .await
            .unwrap();
        let snapshot_now = || async {
            manager
                .get_producer_segments(item, "legacy:Producer")
                .await
                .unwrap()
        };
        let rows_now = || async { manager.get_segments(item, None, false).await.unwrap() };

        producer.publish_fail.store(true, Ordering::SeqCst);
        assert!(
            manager
                .delete_all_producer_segments("legacy:Producer", None)
                .await
                .is_err()
        );
        assert_eq!(rows_now().await, []);
        assert_eq!(snapshot_now().await, [written]);
        manager.run_segment_providers(item, false).await.unwrap();
        let served = rows_now().await;
        assert_eq!((served.len(), served[0].end_ticks), (1, 100));

        producer.publish_fail.store(false, Ordering::SeqCst);
        manager
            .delete_all_producer_segments("legacy:Producer", None)
            .await
            .unwrap();
        assert_eq!(snapshot_now().await, []);
        assert_eq!(rows_now().await, []);
    }

    #[tokio::test]
    async fn producer_publication_failure_preserves_rows_and_core_crud_remains_independent() {
        use std::sync::{Arc, Mutex, atomic::Ordering};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db));
        let events = Arc::new(Mutex::new(Vec::new()));
        let producer = provider("Producer", 0, 0, &events);
        manager.register_segment_provider(producer.clone());
        let original = manager
            .create_producer_segment(
                &segment(item, MediaSegmentType::Intro, 0, 100),
                "legacy:Producer",
            )
            .await
            .unwrap();
        producer.publish_fail.store(true, Ordering::SeqCst);
        assert!(
            manager
                .replace_producer_segments(
                    item,
                    "legacy:Producer",
                    None,
                    &[segment(item, MediaSegmentType::Intro, 200, 300)]
                )
                .await
                .is_err()
        );
        assert_eq!(
            manager.get_segments(item, None, false).await.unwrap(),
            std::slice::from_ref(&original)
        );
        assert_eq!(
            manager
                .get_producer_segments(item, "legacy:Producer")
                .await
                .unwrap(),
            std::slice::from_ref(&original)
        );
        let core_only = manager
            .create_segment(
                &segment(item, MediaSegmentType::Outro, 400, 500),
                "legacy:Producer",
            )
            .await
            .unwrap();
        manager.delete_segment(original.id).await.unwrap();
        assert_eq!(
            manager.get_segments(item, None, false).await.unwrap(),
            [core_only]
        );
        assert_eq!(
            manager
                .get_producer_segments(item, "legacy:Producer")
                .await
                .unwrap(),
            [original]
        );
        producer.publish_fail.store(false, Ordering::SeqCst);
        manager.run_segment_providers(item, false).await.unwrap();
        let replayed = manager.get_segments(item, None, true).await.unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].end_ticks, 100);
    }

    #[tokio::test]
    async fn extraction_callbacks_run_outside_publication_gate_and_stale_results_are_ignored() {
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = Arc::new(FerrofinMediaSegmentManager::new(
            db.clone(),
            library_manager_over(db),
        ));
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut producer = provider("Producer", 0, 0, &events);
        let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        Arc::get_mut(&mut producer).unwrap().pause = Some(pause.clone());
        manager.register_segment_provider(producer);
        let running = manager.clone();
        let task = tokio::spawn(async move { running.run_segment_providers(item, false).await });
        pause.0.notified().await;
        let updated = manager
            .create_producer_segment(
                &segment(item, MediaSegmentType::Intro, 200, 300),
                "legacy:Producer",
            )
            .await
            .unwrap();
        pause.1.notify_one();
        assert_eq!(task.await.unwrap().unwrap(), 1);
        assert_eq!(
            manager.get_segments(item, None, true).await.unwrap(),
            [updated],
            "a pre-mutation extraction cannot overwrite a newer producer edit"
        );
        manager
            .delete_provider_segments(item, "legacy:Producer", None)
            .await
            .unwrap();
        let running = manager.clone();
        let task = tokio::spawn(async move { running.run_segment_providers(item, false).await });
        pause.0.notified().await;
        manager
            .delete_all_producer_segments("legacy:Producer", None)
            .await
            .unwrap();
        pause.1.notify_one();
        task.await.unwrap().unwrap();
        assert!(
            !manager.has_segments(item).await.unwrap(),
            "bulk erasing cache-only output invalidates an extraction with no served rows"
        );
    }

    async fn watched_items(manager: &FerrofinMediaSegmentManager) -> usize {
        manager.mutations.lock().await.revisions.len()
    }

    #[tokio::test]
    async fn revisions_are_kept_only_while_an_extraction_watches_the_item() {
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = Arc::new(FerrofinMediaSegmentManager::new(
            db.clone(),
            library_manager_over(db.clone()),
        ));
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut producer = provider("Producer", 0, 0, &events);
        let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        Arc::get_mut(&mut producer).unwrap().pause = Some(pause.clone());
        manager.register_segment_provider(producer);
        manager
            .create_segment(&segment(item, MediaSegmentType::Outro, 500, 600), "manual")
            .await
            .unwrap();
        assert_eq!(watched_items(&manager).await, 0, "a write no run watches");

        let running = manager.clone();
        let task = tokio::spawn(async move { running.run_segment_providers(item, false).await });
        pause.0.notified().await;
        assert_eq!(watched_items(&manager).await, 1, "a run watches its item");
        pause.1.notify_one();
        assert_eq!(task.await.unwrap().unwrap(), 1);
        assert_eq!(watched_items(&manager).await, 0, "a finished run");

        for clear_by_write in [false, true] {
            let running = manager.clone();
            let task =
                tokio::spawn(async move { running.run_segment_providers(item, false).await });
            pause.0.notified().await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(watched_items(&manager).await, 1, "an aborted run's entry");
            if clear_by_write {
                manager
                    .create_segment(&segment(item, MediaSegmentType::Recap, 700, 800), "manual")
                    .await
                    .unwrap();
            } else {
                let running = manager.clone();
                let task =
                    tokio::spawn(async move { running.run_segment_providers(item, false).await });
                pause.0.notified().await;
                pause.1.notify_one();
                task.await.unwrap().unwrap();
            }
            assert_eq!(
                watched_items(&manager).await,
                0,
                "the item's next write or run clears an aborted run's entry"
            );
        }
        assert_eq!(
            manager.get_segments(item, None, false).await.unwrap().len(),
            3
        );

        let failing = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db));
        let mut broken = provider("Broken", 0, 0, &events);
        Arc::get_mut(&mut broken).unwrap().fail = true;
        failing.register_segment_provider(broken);
        assert_eq!(failing.run_segment_providers(item, false).await.unwrap(), 1);
        assert_eq!(watched_items(&failing).await, 0, "a failed extraction");
    }

    #[tokio::test]
    async fn concurrent_extractions_of_one_item_share_its_entry_until_both_finish() {
        use std::sync::{Arc, Mutex};
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = Arc::new(FerrofinMediaSegmentManager::new(
            db.clone(),
            library_manager_over(db),
        ));
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut producer = provider("Producer", 0, 0, &events);
        let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        Arc::get_mut(&mut producer).unwrap().pause = Some(pause.clone());
        manager.register_segment_provider(producer);
        let mut runs = Vec::new();
        for _ in 0..2 {
            let running = manager.clone();
            runs.push(tokio::spawn(async move {
                running.run_segment_providers(item, false).await
            }));
            // `Notify` keeps one permit: take each run's before starting the next.
            pause.0.notified().await;
        }
        assert_eq!(
            watched_items(&manager).await,
            1,
            "both runs share one entry"
        );
        let edited = manager
            .create_segment(&segment(item, MediaSegmentType::Outro, 500, 600), "manual")
            .await
            .unwrap();
        pause.1.notify_one();
        while !runs.iter().any(tokio::task::JoinHandle::is_finished) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            watched_items(&manager).await,
            1,
            "the other run still watches"
        );
        pause.1.notify_one();
        for run in runs {
            assert_eq!(run.await.unwrap().unwrap(), 1);
        }
        assert_eq!(watched_items(&manager).await, 0, "both runs finished");
        assert_eq!(
            manager.get_segments(item, None, false).await.unwrap(),
            [edited],
            "the write superseded both extractions"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn concurrent_producer_edits_use_retained_adapter_and_complete_snapshots() {
        use ferrofin_traits::media_segments::MediaSegmentProvider;
        use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
        use std::sync::Arc;
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = Arc::new(FerrofinMediaSegmentManager::new(
            db.clone(),
            library_manager_over(db),
        ));
        let tmp = tempfile::tempdir().unwrap();
        let plugin_id = Uuid::new_v4();
        let plugins: Arc<dyn PluginManager> = Arc::new(crate::FerrofinPluginManager::new(
            vec![crate::RegisteredPlugin::new(
                PluginDescriptor {
                    id: plugin_id,
                    name: "Producer".to_owned(),
                    enabled: true,
                    ..Default::default()
                },
                None,
            )],
            tmp.path().join("plugins"),
        ));
        let cached: Arc<dyn MediaSegmentProvider> =
            Arc::new(crate::CachedMediaSegmentProvider::new(
                "Producer".to_owned(),
                plugin_id,
                plugins.clone(),
                vec![BaseItemKind::Episode],
                tmp.path().join("segments"),
            ));
        let retained = manager.register_segment_provider(cached.clone());
        let duplicate: Arc<dyn MediaSegmentProvider> =
            Arc::new(crate::CachedMediaSegmentProvider::new(
                "Producer".to_owned(),
                plugin_id,
                plugins,
                vec![BaseItemKind::Episode],
                tmp.path().join("unused"),
            ));
        assert!(Arc::ptr_eq(
            &retained,
            &manager.register_segment_provider(duplicate.clone())
        ));
        let first = segment(item, MediaSegmentType::Commercial, 0, 100);
        let second = segment(item, MediaSegmentType::Commercial, 200, 300);
        let provider_id = ferrofin_traits::media_segments::media_segment_provider_id("Producer");
        let (one, two) = tokio::join!(
            manager.create_producer_segment(&first, &provider_id),
            manager.create_producer_segment(&second, &provider_id)
        );
        one.unwrap();
        two.unwrap();
        assert_eq!(
            manager.get_segments(item, None, true).await.unwrap().len(),
            2
        );
        let snapshot = cached.cached_segments(item, &[]).await.unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().any(|segment| segment.start_ticks == 200));
        manager
            .replace_loaded_producer_segments(item, duplicate, &[first])
            .await
            .unwrap();
        assert!(!tmp.path().join("unused").exists());
        assert_eq!(cached.cached_segments(item, &[]).await.unwrap().len(), 1);
        manager.run_segment_providers(item, true).await.unwrap();
        assert_eq!(
            manager.get_segments(item, None, true).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn loaded_legacy_writers_are_adopted_without_registering_unloaded_or_nonwriter_namespaces()
     {
        use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
        use std::sync::Arc;
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Episode).await;
        let manager = FerrofinMediaSegmentManager::new(db.clone(), library_manager_over(db));
        let tmp = tempfile::tempdir().unwrap();
        let loaded = Uuid::new_v4();
        let unknown = Uuid::new_v4();
        let plugins: Arc<dyn PluginManager> = Arc::new(crate::FerrofinPluginManager::new(
            vec![crate::RegisteredPlugin::new(
                PluginDescriptor {
                    id: loaded,
                    name: "Loaded writer".to_owned(),
                    enabled: true,
                    ..Default::default()
                },
                None,
            )],
            tmp.path().join("plugins"),
        ));
        let legacy = format!("wasm:{loaded}");
        let original = manager
            .create_segment(&segment(item, MediaSegmentType::Intro, 0, 100), &legacy)
            .await
            .unwrap();
        manager
            .create_segment(
                &segment(item, MediaSegmentType::Outro, 300, 400),
                &format!("wasm:{unknown}"),
            )
            .await
            .unwrap();
        let provider = Arc::new(
            crate::CachedMediaSegmentProvider::new(
                "Loaded writer".to_owned(),
                loaded,
                plugins.clone(),
                vec![BaseItemKind::Episode],
                tmp.path().join("loaded"),
            )
            .with_legacy_ids(vec![legacy.clone()]),
        );
        assert!(
            manager
                .adopt_loaded_segment_provider(provider)
                .await
                .unwrap()
        );
        assert!(manager.is_registered_segment_provider(&legacy));
        assert!(!manager.is_registered_segment_provider(&format!("wasm:{unknown}")));
        assert_eq!(
            manager.get_segments(item, None, true).await.unwrap(),
            [original]
        );
        assert!(tmp.path().join(format!("loaded/{item}.json")).is_file());
        let nonwriter = Uuid::new_v4();
        assert!(
            !manager
                .adopt_loaded_segment_provider(Arc::new(
                    crate::CachedMediaSegmentProvider::new(
                        "State-only analyzer".to_owned(),
                        nonwriter,
                        plugins,
                        vec![BaseItemKind::Episode],
                        tmp.path().join("nonwriter")
                    )
                    .with_legacy_ids(vec![format!("wasm:{nonwriter}")])
                ))
                .await
                .unwrap()
        );
        assert!(!tmp.path().join("nonwriter").exists());
        assert_eq!(manager.registered_segment_providers().len(), 1);
        manager.run_segment_providers(item, true).await.unwrap();
        assert_eq!(
            manager.get_segments(item, None, true).await.unwrap()[0].end_ticks,
            100
        );
    }

    #[test]
    fn producer_timestamp_removal_matches_the_plugins_millisecond_tolerance() {
        use ferrofin_traits::media_segments::MediaSegmentCacheMutation;
        let item = Uuid::new_v4();
        let target = segment(item, MediaSegmentType::Intro, 50_000, 100_000);
        let near = segment(item, MediaSegmentType::Intro, 60_000, 110_000);
        let outside = segment(item, MediaSegmentType::Intro, 60_001, 110_000);
        assert_eq!(
            MediaSegmentCacheMutation::Remove {
                segment: target,
                tolerance_ticks: 10_000
            }
            .apply(&[near, outside.clone()]),
            [outside]
        );
    }

    #[test]
    fn provider_identity_uses_dotnet_simple_invariant_lowercase() {
        assert_eq!(
            ferrofin_traits::media_segments::media_segment_provider_id("İ"),
            ferrofin_common::extensions::get_md5("İ")
                .simple()
                .to_string()
        );
        assert_eq!(
            ferrofin_traits::media_segments::media_segment_provider_id("Éclair"),
            ferrofin_traits::media_segments::media_segment_provider_id("éCLAIR")
        );
    }

    #[test]
    fn concurrent_registration_keeps_a_single_loaded_identity() {
        use std::sync::{Arc, Mutex};
        // Registry ordering is synchronous and shared by all manager clones.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(test_db());
        let manager = Arc::new(FerrofinMediaSegmentManager::new(
            db.clone(),
            library_manager_over(db),
        ));
        let events = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let manager = manager.clone();
                let provider = provider("Loaded", 0, 0, &events);
                scope.spawn(move || manager.register_segment_provider(provider));
            }
        });
        assert_eq!(manager.registered_segment_providers().len(), 1);
    }
}
