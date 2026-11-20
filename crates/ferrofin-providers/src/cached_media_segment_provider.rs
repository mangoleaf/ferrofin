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
        if !BaseItemKind::from_stored_type_name(&item.type_)
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

#[cfg(test)]
mod tests {
    use ferrofin_model::media_segments::MediaSegmentType;
    use ferrofin_traits::plugins::DisabledPluginManager;

    use super::*;

    fn segment(item_id: Uuid, type_: MediaSegmentType, start_ticks: i64) -> MediaSegmentDto {
        MediaSegmentDto {
            id: Uuid::new_v4(),
            item_id,
            type_,
            start_ticks,
            end_ticks: start_ticks + 10,
        }
    }

    fn provider(dir: &std::path::Path) -> CachedMediaSegmentProvider {
        CachedMediaSegmentProvider::new(
            "Producer".to_owned(),
            Uuid::new_v4(),
            Arc::new(DisabledPluginManager),
            vec![BaseItemKind::Episode],
            dir.join("cache"),
        )
    }

    /// The cache replays what a producer stored, falls back to the existing
    /// rows for an item it never stored, and forgets an item on cleanup.
    #[tokio::test]
    async fn the_cache_replays_stored_output_and_forgets_on_cleanup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = provider(dir.path());
        let (item, other) = (Uuid::new_v4(), Uuid::new_v4());
        let intro = segment(item, MediaSegmentType::Intro, 0);
        let existing = vec![segment(other, MediaSegmentType::Outro, 50)];

        cache
            .cache_segments(item, std::slice::from_ref(&intro))
            .await
            .expect("cache");
        let request = |item_id| MediaSegmentGenerationRequest {
            item_id,
            existing_segments: existing.clone(),
        };
        assert_eq!(
            cache.get_segments(&request(item)).await.expect("get"),
            std::slice::from_ref(&intro)
        );
        assert_eq!(
            cache.get_segments(&request(other)).await.expect("get"),
            existing
        );
        assert!(dir.path().join("cache/producer.json").is_file());

        let outro = segment(item, MediaSegmentType::Outro, 90);
        cache
            .mutate_cache(item, &[], &MediaSegmentCacheMutation::Append(outro.clone()))
            .await
            .expect("mutate");
        assert_eq!(
            cache.cached_segments(item, &[]).await.expect("read"),
            [intro, outro.clone()]
        );

        cache
            .erase_cached_segments(Some(MediaSegmentType::Intro))
            .await
            .expect("erase");
        assert_eq!(
            cache.cached_segments(item, &[]).await.expect("read"),
            [outro]
        );

        cache.cleanup(item).await.expect("cleanup");
        cache
            .cleanup(item)
            .await
            .expect("a second cleanup finds nothing to remove");
        assert_eq!(
            cache.cached_segments(item, &existing).await.expect("read"),
            existing
        );
    }

    /// Stored output is never overwritten by the first-run seed, and an empty
    /// seed writes nothing.
    #[tokio::test]
    async fn initialize_seeds_only_an_item_with_no_stored_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = provider(dir.path());
        let item = Uuid::new_v4();
        cache.initialize_cache(item, &[]).await.expect("empty seed");
        assert!(!dir.path().join(format!("cache/{item}.json")).exists());

        let first = segment(item, MediaSegmentType::Intro, 0);
        cache
            .initialize_cache(item, std::slice::from_ref(&first))
            .await
            .expect("seed");
        let later = segment(item, MediaSegmentType::Intro, 40);
        cache
            .initialize_cache(item, &[later])
            .await
            .expect("reseed");
        assert_eq!(
            cache.cached_segments(item, &[]).await.expect("read"),
            [first]
        );
    }

    /// A producer serves only the kinds it handles, and only while the plugin
    /// manager reports its plugin enabled: an unknown plugin serves nothing.
    #[tokio::test]
    async fn supports_needs_a_handled_kind_and_an_enabled_plugin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_id = Uuid::new_v4();
        let cache = |plugins: Arc<dyn PluginManager>| {
            CachedMediaSegmentProvider::new(
                "Producer".to_owned(),
                plugin_id,
                plugins,
                vec![BaseItemKind::Episode],
                dir.path().join("cache"),
            )
            .with_legacy_ids(vec!["wasm:legacy".to_owned()])
        };
        let row = |kind: BaseItemKind| BaseItemEntity {
            type_: kind.stored_type_name().expect("stored type").to_owned(),
            ..BaseItemEntity::default()
        };
        let enabled = cache(crate::plugin_config::tests_support::enabled_plugin(
            plugin_id,
        ));
        assert_eq!(enabled.name(), "Producer");
        assert_eq!(enabled.legacy_ids(), ["wasm:legacy"]);
        assert!(
            enabled
                .supports(&row(BaseItemKind::Episode))
                .await
                .expect("episode")
        );
        assert!(
            !enabled
                .supports(&row(BaseItemKind::Movie))
                .await
                .expect("movie")
        );

        let unknown = cache(Arc::new(DisabledPluginManager));
        assert!(
            !unknown
                .supports(&row(BaseItemKind::Episode))
                .await
                .expect("episode")
        );
    }
}
