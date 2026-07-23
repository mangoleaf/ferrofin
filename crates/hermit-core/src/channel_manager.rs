//! [`HermitChannelManager`] — a **stub** [`ChannelManager`] for the deferred
//! channels subsystem.
//!
//! Port of the disabled/null shape of
//! `Emby.Server.Implementations.Channels.ChannelManager`. Channels are not part
//! of the Hermit v1 feature set (per the port plan's deferred-subsystem list),
//! so no `IChannel` backends are registered and every query resolves to an
//! empty result. The seam exists only so the DI graph and `AppState` can name an
//! `Arc<dyn ChannelManager>`; a real implementation is a future wave.
//!
//! There is no per-backend strategy fan-out and no persistence here — the
//! methods return empty collections rather than touching the database.

use async_trait::async_trait;
use hermit_model::channels::{ChannelFeatures, ChannelQuery};
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::InternalItemsQuery;
use hermit_traits::stubs::ChannelManager;

/// The stub channels manager for the deferred channels subsystem.
///
/// Every method returns an empty result: no channels are advertised and no
/// channel items exist.
#[derive(Debug, Clone, Copy, Default)]
pub struct HermitChannelManager;

impl HermitChannelManager {
    /// Creates the stub channels manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelManager for HermitChannelManager {
    async fn get_channel_features(
        &self,
        _id: Option<Uuid>,
    ) -> Result<Vec<ChannelFeatures>, ServiceError> {
        // Channels are deferred: no backends are registered.
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
    use hermit_model::channels::ChannelQuery;
    use hermit_traits::options::InternalItemsQuery;
    use hermit_traits::stubs::ChannelManager;

    use super::HermitChannelManager;

    #[tokio::test]
    async fn everything_is_empty() {
        let mgr = HermitChannelManager::new();
        assert!(
            mgr.get_channel_features(None)
                .await
                .expect("features")
                .is_empty()
        );
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
