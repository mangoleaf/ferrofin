//! Metadata/image provider manager trait — refresh orchestration.
//!
//! Port of `MediaBrowser.Controller.Providers.IProviderManager` (the manager
//! only; the per-strategy `I*MetadataProvider` / `I*ImageProvider` / `IExternalId`
//! interfaces are intentionally **not** ported — they become match-on-item-kind
//! logic in `hermit-core`).
//!
//! Port rules applied:
//! - The C# `BaseItem` receivers become [`uuid::Uuid`] identity arguments; the
//!   image bytes overloads of `SaveImage` collapse to one `save_image` taking a
//!   MIME type plus owned bytes (a `Stream` cannot cross an object-safe async
//!   boundary cheaply).
//! - `AddParts` (registers the strategy interfaces that are not ported) and the
//!   `.NET` refresh events are dropped.
//! - Remote-image lookups reuse [`RemoteImageInfo`]/[`RemoteImageQuery`]/
//!   [`ImageProviderInfo`]; external-link lookups reuse
//!   [`ExternalUrl`]/[`ExternalIdInfo`]; metadata options reuse
//!   [`MetadataOptions`]/[`MetadataPluginSummary`] from `hermit-model`.
//! - The refresh-request/priority/update value types
//!   ([`MetadataRefreshOptions`], [`RefreshPriority`], [`ItemUpdateType`]) live
//!   under `MediaBrowser.Controller`/`.Model.Entities` and are ported here as
//!   local service-layer types. `ItemUpdateType` is a wire enum echoed in API
//!   responses and is flagged in the port report as a candidate for
//!   `hermit-model`.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` /
//!   `IProgress` are dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use hermit_model::configuration::{MetadataOptions, MetadataPluginSummary};
use hermit_model::entities::ImageType;
use hermit_model::providers::{
    ExternalIdInfo, ExternalUrl, ImageProviderInfo, RemoteImageInfo, RemoteImageQuery,
};
use uuid::Uuid;

use crate::error::ServiceError;

/// The priority a queued refresh runs at.
///
/// Port of `MediaBrowser.Controller.Providers.RefreshPriority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshPriority {
    /// A background/idle refresh.
    #[default]
    Low,
    /// A user-visible but non-blocking refresh.
    Normal,
    /// A refresh the caller is waiting on.
    High,
}

/// How aggressively a refresh should re-fetch and overwrite metadata.
///
/// Port of the `MetadataRefreshMode` enum carried by C#
/// `MetadataRefreshOptions`; only the mode field is retained (the directory
/// service / per-provider replace flags are impl-internal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataRefreshMode {
    /// Do not refresh.
    #[default]
    None,
    /// Validate only what is already present.
    ValidationOnly,
    /// Fetch missing metadata only.
    Default,
    /// Fetch all metadata (respecting cache).
    FullRefresh,
}

/// The options driving a metadata/image refresh.
///
/// Port of `MediaBrowser.Controller.Providers.MetadataRefreshOptions`, reduced
/// to the fields the manager surface actually needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetadataRefreshOptions {
    /// How metadata should be (re)fetched.
    pub metadata_refresh_mode: MetadataRefreshMode,
    /// How images should be (re)fetched.
    pub image_refresh_mode: MetadataRefreshMode,
    /// Replace all existing metadata rather than filling gaps.
    pub replace_all_metadata: bool,
    /// Replace all existing images rather than filling gaps.
    pub replace_all_images: bool,
}

/// Which parts of an item a refresh changed.
///
/// Port of `MediaBrowser.Model.Entities.ItemUpdateType` (a `[Flags]` enum). It
/// is echoed in API responses, so it is a candidate for `hermit-model` — see
/// the port report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemUpdateType {
    /// Nothing changed.
    #[default]
    None,
    /// Only metadata was downloaded/updated.
    MetadataDownload,
    /// Metadata was edited (locally or imported).
    MetadataEdit,
    /// Images changed.
    ImageUpdate,
}

/// Orchestrates metadata and image refreshing for library items.
///
/// Port of `IProviderManager` (manager surface only).
#[async_trait]
pub trait ProviderManager: Send + Sync {
    /// Queues an asynchronous refresh of an item at the given priority.
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
        priority: RefreshPriority,
    ) -> Result<(), ServiceError>;

    /// Refreshes an item and all of its children.
    async fn refresh_full_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError>;

    /// Refreshes a single item, returning what changed.
    async fn refresh_single_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError>;

    /// Downloads and stores an image for an item from a URL.
    async fn save_image_from_url(
        &self,
        item_id: Uuid,
        url: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError>;

    /// Stores caller-supplied image bytes for an item.
    async fn save_image(
        &self,
        item_id: Uuid,
        content: &[u8],
        mime_type: &str,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError>;

    /// Deletes an item's image of `image_type` at `image_index` (default `0`).
    ///
    /// Port of `BaseItem.DeleteImageAsync(imageType, index)` (the fan-in target of
    /// `ImageController.DeleteItemImage`/`DeleteItemImageByIndex`): removes the
    /// on-disk file and the stored image row. The default implementation reports
    /// the image pipeline as deferred, mirroring [`save_image`](Self::save_image)
    /// in the shell manager; a host with the ported image store overrides it.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] while the image store is deferred, or whatever
    /// error the concrete deletion surfaces.
    async fn delete_image(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        let _ = (item_id, image_type, image_index);
        Err(ServiceError::backend(
            "delete_image is deferred until the image pipeline lands",
        ))
    }

    /// Gets the remote images available for an item.
    async fn get_available_remote_images(
        &self,
        item_id: Uuid,
        query: &RemoteImageQuery,
    ) -> Result<Vec<RemoteImageInfo>, ServiceError>;

    /// Lists the remote image providers usable for an item.
    async fn get_remote_image_provider_info(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ImageProviderInfo>, ServiceError>;

    /// Persists an item's metadata, recording the update type.
    async fn save_metadata(
        &self,
        item_id: Uuid,
        update_type: ItemUpdateType,
    ) -> Result<(), ServiceError>;

    /// Gets the external links (IMDb, TMDb, …) for an item.
    async fn get_external_urls(&self, item_id: Uuid) -> Result<Vec<ExternalUrl>, ServiceError>;

    /// Gets the external-id descriptors applicable to an item.
    async fn get_external_id_infos(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<ExternalIdInfo>, ServiceError>;

    /// Gets a summary of every registered metadata plugin.
    async fn get_all_metadata_plugins(&self) -> Result<Vec<MetadataPluginSummary>, ServiceError>;

    /// Gets the configured metadata options for an item.
    async fn get_metadata_options(&self, item_id: Uuid) -> Result<MetadataOptions, ServiceError>;

    /// Gets the ids of items currently queued for refresh.
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError>;
}

fn _assert_object_safe_provider_manager(_: &dyn ProviderManager) {}

#[cfg(test)]
mod tests {
    use super::{ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, RefreshPriority};

    #[test]
    fn enums_default_to_the_no_op_variant() {
        assert_eq!(RefreshPriority::default(), RefreshPriority::Low);
        assert_eq!(MetadataRefreshMode::default(), MetadataRefreshMode::None);
        assert_eq!(ItemUpdateType::default(), ItemUpdateType::None);
    }

    #[test]
    fn refresh_options_default_replaces_nothing() {
        let o = MetadataRefreshOptions::default();
        assert!(!o.replace_all_metadata);
        assert!(!o.replace_all_images);
        assert_eq!(o.metadata_refresh_mode, MetadataRefreshMode::None);
    }
}
