//! [`HermitLiveTvManager`] — a **placeholder** [`LiveTvManager`] stub.
//!
//! Unlike the other deferred subsystems in this unit, Live TV *does* have a real
//! implementation planned in the separate `hermit-livetv` crate, injected as an
//! `Arc<dyn LiveTvManager>` at the Wave 8 composition root. `hermit-core` must
//! **not** depend on `hermit-livetv`, so this placeholder lets the DI graph and
//! tests name the seam before that crate lands (and stands in when Live TV is
//! disabled).
//!
//! It reports an empty [`LiveTvInfo`] (no services, disabled), an empty program
//! query result, and treats tuner resets as a no-op.

use async_trait::async_trait;
use hermit_model::dto::BaseItemDto;
use hermit_model::live_tv::LiveTvInfo;
use hermit_model::querying::QueryResult;

use hermit_traits::error::ServiceError;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::stubs::LiveTvManager;

/// The placeholder Live TV manager.
///
/// Stands in for the real `hermit-livetv` implementation (injected at Wave 8) so
/// the seam is nameable early; reports Live TV as disabled with no programs.
#[derive(Debug, Clone, Copy, Default)]
pub struct HermitLiveTvManager;

impl HermitLiveTvManager {
    /// Creates the placeholder Live TV manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LiveTvManager for HermitLiveTvManager {
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        // No services registered → Live TV reports itself disabled.
        Ok(LiveTvInfo::default())
    }

    async fn get_programs(
        &self,
        _query: &InternalItemsQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        Ok(QueryResult::default())
    }

    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        // No tuners exist; resetting is a successful no-op.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use hermit_traits::options::{DtoOptions, InternalItemsQuery};
    use hermit_traits::stubs::LiveTvManager;

    use super::HermitLiveTvManager;

    #[tokio::test]
    async fn reports_disabled_and_empty() {
        let mgr = HermitLiveTvManager::new();
        let info = mgr.get_live_tv_info().await.expect("info");
        assert!(!info.is_enabled);
        assert!(info.services.is_empty());
        assert_eq!(
            mgr.get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
                .await
                .expect("programs")
                .total_record_count,
            0
        );
        mgr.reset_tuner("chan-1").await.expect("reset is a no-op");
    }
}
