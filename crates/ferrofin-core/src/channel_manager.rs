//! [`FerrofinChannelManager`] — the [`ChannelManager`] the server runs with.
//!
//! Upstream's `ChannelManager` (`Jellyfin.LiveTv.Channels`) fans each query
//! out to its registered `IChannel` backends. Ferrofin has no `IChannel` seam, so
//! nothing can register a channel and every query resolves to an empty result.
//! TODO(parity, gl-298): open work item: give channels an extension seam.
//!
//! There is no per-backend strategy fan-out and no persistence here — the
//! methods return empty collections rather than touching the database.

use async_trait::async_trait;
use ferrofin_model::channels::{ChannelFeatures, ChannelQuery};
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::stubs::ChannelManager;

/// The channels manager with no channel backends (see the module docs).
///
/// Every method returns an empty result: no channels are advertised and no
/// channel items exist.
#[derive(Debug, Clone, Copy, Default)]
pub struct FerrofinChannelManager;

impl FerrofinChannelManager {
    /// Creates the channels manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelManager for FerrofinChannelManager {
    async fn get_channel_features(
        &self,
        _id: Option<Uuid>,
    ) -> Result<Vec<ChannelFeatures>, ServiceError> {
        // No channel backends can be registered (see the module docs).
        Ok(Vec::new())
    }

    async fn get_channels(
        &self,
        _query: &ChannelQuery,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        Ok(QueryResult::default())
    }

    async fn get_channel_items(
        &self,
        _query: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        Ok(QueryResult::default())
    }
}

#[cfg(test)]
mod tests {
    use ferrofin_model::channels::ChannelQuery;
    use ferrofin_traits::options::InternalItemsQuery;
    use ferrofin_traits::stubs::ChannelManager;

    use super::FerrofinChannelManager;

    #[tokio::test]
    async fn everything_is_empty() {
        let mgr = FerrofinChannelManager::new();
        let channel_features = mgr.get_channel_features(None).await.expect("features");
        assert!(channel_features.is_empty(), "{channel_features:?}");
        assert_eq!(
            mgr.get_channels(&ChannelQuery::default())
                .await
                .expect("channels")
                .total_record_count,
            0
        );
        assert_eq!(
            mgr.get_channel_items(&InternalItemsQuery::default())
                .await
                .expect("items")
                .total_record_count,
            0
        );
    }
}
