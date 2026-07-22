//! The [`ProviderManager`] trait implementation — port of the
//! `MediaBrowser.Providers.Manager.ProviderManager` surface.
//!
//! Scope note (First-Light): the full C# `ProviderManager` couples the metadata
//! *refresh orchestration* to the library item store, the image-saving pipeline,
//! and the (deferred, feature-gated) remote provider plugins — none of which are
//! available in this wave. The high-value, test-backed deliverable in this crate
//! is the XbmcMetadata NFO parser subsystem ([`crate::xbmc`]).
//!
//! This type therefore implements the [`hermit_traits::providers::ProviderManager`]
//! trait as a thin, dependency-free shell: read-only descriptor queries return
//! empty/default results, and the operations that require the (not-yet-ported)
//! library store or network I/O return [`ServiceError::Backend`] describing the
//! deferral rather than silently succeeding. The external-id descriptor set —
//! which the NFO parsers consume — is fully wired.

use async_trait::async_trait;
use hermit_model::configuration::{MetadataOptions, MetadataPluginSummary};
use hermit_model::entities::ImageType;
use hermit_model::providers::{
    ExternalIdInfo, ExternalUrl, ImageProviderInfo, RemoteImageInfo, RemoteImageQuery,
};
use hermit_traits::error::ServiceError;
use hermit_traits::providers::{
    ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use uuid::Uuid;

/// A First-Light [`ProviderManager`] providing the descriptor surface and NFO
/// external-id set, with refresh/image orchestration deferred.
///
/// Construct via [`LocalProviderManager::new`]. The external-id descriptors it
/// advertises can be supplied at construction so downstream NFO parsing sees the
/// same provider set the manager would.
#[derive(Debug, Default, Clone)]
pub struct LocalProviderManager {
    external_id_infos: Vec<ExternalIdInfo>,
}

impl LocalProviderManager {
    /// Creates a manager advertising `external_id_infos` for every item.
    #[must_use]
    pub fn new(external_id_infos: Vec<ExternalIdInfo>) -> Self {
        Self { external_id_infos }
    }

    /// Builds the "operation deferred" error for the orchestration methods that
    /// need the (not-yet-ported) library store or network I/O.
    fn deferred(op: &str) -> ServiceError {
        ServiceError::backend(format!(
            "{op} is deferred until the library-item store / image pipeline lands"
        ))
    }
}

#[async_trait]
impl ProviderManager for LocalProviderManager {
    async fn queue_refresh(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        Err(Self::deferred("queue_refresh"))
    }

    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        Err(Self::deferred("refresh_full_item"))
    }

    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        Err(Self::deferred("refresh_single_item"))
    }

    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Err(Self::deferred("save_image_from_url"))
    }

    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Err(Self::deferred("save_image"))
    }

    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError> {
        // No remote image providers are wired in First-Light.
        Ok(Vec::new())
    }

    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }

    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        Err(Self::deferred("save_metadata"))
    }

    async fn get_external_urls(&self, _item_id: Uuid) -> Result<Vec<ExternalUrl>, ServiceError> {
        Ok(Vec::new())
    }

    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError> {
        Ok(self.external_id_infos.clone())
    }

    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError> {
        Ok(Vec::new())
    }

    async fn get_metadata_options(&self, _item_id: Uuid) -> Result<MetadataOptions, ServiceError> {
        Ok(MetadataOptions::default())
    }

    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::LocalProviderManager;
    use hermit_model::entities::ImageType;
    use hermit_model::providers::{ExternalIdInfo, RemoteImageQuery};
    use hermit_traits::providers::{
        ItemUpdateType, MetadataRefreshOptions, ProviderManager, RefreshPriority,
    };
    use uuid::Uuid;

    #[tokio::test]
    async fn external_id_infos_are_advertised() {
        let mgr = LocalProviderManager::default();
        let infos = mgr
            .get_external_id_infos(Uuid::nil())
            .await
            .expect("descriptor query succeeds");
        assert!(infos.is_empty());
    }

    #[tokio::test]
    async fn new_advertises_supplied_external_ids_for_every_item() {
        let seed = vec![ExternalIdInfo::new("Tmdb".into(), "tmdb".into(), None)];
        let mgr = LocalProviderManager::new(seed.clone());
        // The manager clones the same descriptor set for any item id.
        let a = mgr.get_external_id_infos(Uuid::nil()).await.unwrap();
        let b = mgr.get_external_id_infos(Uuid::new_v4()).await.unwrap();
        assert_eq!(a, seed);
        assert_eq!(b, seed);
    }

    #[tokio::test]
    async fn refresh_is_deferred() {
        let mgr = LocalProviderManager::default();
        let err = mgr
            .refresh_single_item(Uuid::nil(), &MetadataRefreshOptions::default())
            .await
            .expect_err("refresh is deferred");
        assert!(err.to_string().contains("deferred"));
    }

    /// Every orchestration method that needs the not-yet-ported library store /
    /// image pipeline must return a `deferred` backend error naming the op.
    #[tokio::test]
    async fn all_orchestration_methods_are_deferred_with_op_name() {
        let mgr = LocalProviderManager::default();
        let id = Uuid::nil();
        let opts = MetadataRefreshOptions::default();

        let queue = mgr
            .queue_refresh(id, &opts, RefreshPriority::Normal)
            .await
            .expect_err("queue_refresh deferred");
        assert!(queue.to_string().contains("queue_refresh"));
        assert!(queue.to_string().contains("deferred"));

        let full = mgr
            .refresh_full_item(id, &opts)
            .await
            .expect_err("refresh_full_item deferred");
        assert!(full.to_string().contains("refresh_full_item"));

        let save_url = mgr
            .save_image_from_url(id, "http://x/y.jpg", ImageType::Primary, Some(0))
            .await
            .expect_err("save_image_from_url deferred");
        assert!(save_url.to_string().contains("save_image_from_url"));

        let save_bytes = mgr
            .save_image(id, b"data", "image/jpeg", ImageType::Backdrop, None)
            .await
            .expect_err("save_image deferred");
        assert!(save_bytes.to_string().contains("save_image"));

        let save_meta = mgr
            .save_metadata(id, ItemUpdateType::MetadataDownload)
            .await
            .expect_err("save_metadata deferred");
        assert!(save_meta.to_string().contains("save_metadata"));
    }

    /// The read-only descriptor queries return empty/default results (no store,
    /// no remote providers wired in First-Light) — never an error.
    #[tokio::test]
    async fn read_only_queries_return_empty_defaults() {
        let mgr = LocalProviderManager::default();
        let id = Uuid::new_v4();
        let query = RemoteImageQuery::new("Tmdb".into());

        assert!(
            mgr.get_available_remote_images(id, &query)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mgr.get_remote_image_provider_info(id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(mgr.get_external_urls(id).await.unwrap().is_empty());
        assert!(mgr.get_all_metadata_plugins().await.unwrap().is_empty());
        assert!(mgr.get_refresh_queue().await.unwrap().is_empty());

        // Metadata options fall back to the type default.
        let opts = mgr.get_metadata_options(id).await.unwrap();
        assert_eq!(
            opts,
            hermit_model::configuration::MetadataOptions::default()
        );
    }
}
