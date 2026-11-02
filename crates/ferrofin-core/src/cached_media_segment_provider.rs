//! Persistent producer output replayed by the registered media-segment task.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentGenerationRequest};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::media_segments::{MediaSegmentCacheMutation, MediaSegmentProvider};
use ferrofin_traits::plugins::PluginManager;
use ferrofin_util::directory_path::DirectoryPath;
use uuid::Uuid;

/// Adapts season/analyzer producers whose own output outlives served segment rows.
/// It reads persisted output; it never reruns a season detector for every episode.
pub struct CachedMediaSegmentProvider {
    name: String,
    plugin_id: Uuid,
    plugins: Arc<dyn PluginManager>,
    kinds: Vec<BaseItemKind>,
    legacy_ids: Vec<String>,
    cache_dir: DirectoryPath,
    writes: tokio::sync::Mutex<()>,
}

impl CachedMediaSegmentProvider {
    /// Creates a provider in the producer's existing cache/state directory.
    #[must_use]
    pub fn new(
        name: String,
        plugin_id: Uuid,
        plugins: Arc<dyn PluginManager>,
        kinds: Vec<BaseItemKind>,
        cache_dir: impl Into<DirectoryPath>,
    ) -> Self {
        Self {
            name,
            plugin_id,
            plugins,
            kinds,
            legacy_ids: Vec::new(),
            cache_dir: cache_dir.into(),
            writes: tokio::sync::Mutex::new(()),
        }
    }
    /// Associate known pre-registry namespaces with this actual loaded producer.
    #[must_use]
    pub fn with_legacy_ids(mut self, ids: Vec<String>) -> Self {
        self.legacy_ids = ids;
        self
    }
    async fn read(&self, item: Uuid) -> Result<Option<Vec<MediaSegmentDto>>, ServiceError> {
        match tokio::fs::read(self.cache_dir.join(format!("{item}.json"))).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| ServiceError::backend(format!("media-segment cache: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ServiceError::backend(format!(
                "media-segment cache: {error}"
            ))),
        }
    }
    async fn write(&self, item: Uuid, segments: &[MediaSegmentDto]) -> Result<(), ServiceError> {
        let path = self.cache_dir.join(format!("{item}.json"));
        let bytes = serde_json::to_vec(segments)
            .map_err(|error| ServiceError::backend(error.to_string()))?;
        tokio::task::spawn_blocking(move || {
            ferrofin_util::file_helper::atomic_write(&path, &bytes)
        })
        .await
        .map_err(|error| ServiceError::backend(error.to_string()))?
        .map_err(|error| ServiceError::backend(format!("media-segment cache: {error}")))?;
        let identity = self.cache_dir.join("producer.json");
        let plugin_id = self.plugin_id;
        if !identity.is_file() {
            // Output is already committed; legacy snapshot proof also works if this marker fails.
            if let Err(error) = tokio::task::spawn_blocking(move || {
                ferrofin_util::file_helper::atomic_write(
                    &identity,
                    format!("\"{plugin_id}\"").as_bytes(),
                )
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()))
            {
                tracing::debug!(%plugin_id, %error, "could not persist media-segment producer identity");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MediaSegmentProvider for CachedMediaSegmentProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn legacy_ids(&self) -> Vec<String> {
        self.legacy_ids.clone()
    }
    async fn supports(&self, item: &BaseItemEntity) -> Result<bool, ServiceError> {
        if !crate::item_type_lookup::kind_from_type_name(&item.type_)
            .is_some_and(|kind| self.kinds.contains(&kind))
        {
            return Ok(false);
        }
        Ok(self
            .plugins
            .get_plugin(self.plugin_id)
            .await?
            .is_some_and(|plugin| plugin.enabled))
    }
    async fn get_segments(
        &self,
        request: &MediaSegmentGenerationRequest,
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        Ok(self
            .read(request.item_id)
            .await?
            .unwrap_or_else(|| request.existing_segments.clone()))
    }
    async fn cleanup(&self, item: Uuid) -> Result<(), ServiceError> {
        let _guard = self.writes.lock().await;
        match tokio::fs::remove_file(self.cache_dir.join(format!("{item}.json"))).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ServiceError::backend(error.to_string())),
        }
    }
    async fn cache_segments(
        &self,
        item: Uuid,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        let _guard = self.writes.lock().await;
        self.write(item, segments).await
    }
    async fn cached_segments(
        &self,
        item: Uuid,
        existing: &[MediaSegmentDto],
    ) -> Result<Vec<MediaSegmentDto>, ServiceError> {
        let _guard = self.writes.lock().await;
        Ok(self.read(item).await?.unwrap_or_else(|| existing.to_vec()))
    }
    async fn mutate_cache(
        &self,
        item: Uuid,
        existing: &[MediaSegmentDto],
        mutation: &MediaSegmentCacheMutation,
    ) -> Result<(), ServiceError> {
        let _guard = self.writes.lock().await;
        let current = self.read(item).await?.unwrap_or_else(|| existing.to_vec());
        self.write(item, &mutation.apply(&current)).await
    }
    async fn erase_cached_segments(
        &self,
        type_filter: Option<ferrofin_model::media_segments::MediaSegmentType>,
    ) -> Result<(), ServiceError> {
        let _guard = self.writes.lock().await;
        let mut entries = match tokio::fs::read_dir(self.cache_dir.resolve()).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ServiceError::backend(error.to_string())),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| ServiceError::backend(error.to_string()))?
        {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(item) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            let current = self.read(item).await?.unwrap_or_default();
            self.write(
                item,
                &MediaSegmentCacheMutation::Replace {
                    type_filter,
                    segments: Vec::new(),
                }
                .apply(&current),
            )
            .await?;
        }
        Ok(())
    }
    async fn initialize_cache(
        &self,
        item: Uuid,
        segments: &[MediaSegmentDto],
    ) -> Result<(), ServiceError> {
        let _guard = self.writes.lock().await;
        if self.read(item).await?.is_none() && !segments.is_empty() {
            self.write(item, segments).await?;
        }
        Ok(())
    }
}
