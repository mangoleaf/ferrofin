//! The channels manager trait.
//!
//! Port of a slice of `MediaBrowser.Controller.Channels.IChannelManager`. The
//! per-backend `IChannel` strategy interface and the rest of the item-query
//! surface are not ported, so no channel can be registered and `ferrofin-core`'s
//! implementation answers every query empty. TODO(parity, gl-298): open work
//! item: give channels an extension seam.
//!
//! Port rules applied: channel/item results reuse `ferrofin-model` DTOs
//! ([`ChannelFeatures`], `QueryResult<BaseItemDto>`); the `ChannelQuery` param
//! is reused from `ferrofin-model`; identity args are [`uuid::Uuid`]; `Task<T>` →
//! `async fn -> Result<T, ServiceError>`.

use async_trait::async_trait;
use ferrofin_model::channels::{ChannelFeatures, ChannelQuery};
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;

use crate::error::ServiceError;
use crate::options::InternalItemsQuery;

/// The channels manager.
///
/// Port of a slice of `IChannelManager` (see the module docs).
#[async_trait]
pub trait ChannelManager: Send + Sync {
    /// Gets the features advertised by a channel (or all channels when `None`).
    async fn get_channel_features(
        &self,
        id: Option<uuid::Uuid>,
    ) -> Result<Vec<ChannelFeatures>, ServiceError>;

    /// Lists the available channels as item DTOs.
    async fn get_channels(
        &self,
        query: &ChannelQuery,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Lists the items inside a channel.
    async fn get_channel_items(
        &self,
        query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;
}

fn _assert_object_safe_channel_manager(_: &dyn ChannelManager) {}
