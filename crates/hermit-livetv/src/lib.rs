//! Live TV for Hermit — **deferred subsystem** (see `brain/DEFERRED.md`).
//!
//! Provides `DisabledLiveTvManager`, the no-op implementation of the
//! `hermit-traits` `LiveTvManager` trait: no tuners, no guide, routes report
//! disabled/empty. The real tuner/EPG port is future work. Filled by the Wave 5
//! PortJob. See `brain/PLAN_HERMIT_PORT.md`.

pub mod m3u;
pub mod xmltv;

use async_trait::async_trait;
use hermit_model::dto::BaseItemDto;
use hermit_model::live_tv::LiveTvInfo;
use hermit_model::querying::QueryResult;
use hermit_traits::error::ServiceError;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::stubs::LiveTvManager;

/// A [`LiveTvManager`] that reports Live TV as disabled.
///
/// Live TV is a deferred Hermit subsystem: there are no tuners, no guide data
/// and no recordings. Every method returns the empty/disabled state
/// ([`LiveTvInfo::default`] with `is_enabled = false`, an empty
/// [`QueryResult`], and a no-op tuner reset) and never errors, mirroring
/// upstream Jellyfin's behaviour when no Live TV services are configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledLiveTvManager;

#[async_trait]
impl LiveTvManager for DisabledLiveTvManager {
    /// Returns disabled Live TV info: `is_enabled = false`, no services and no
    /// enabled users.
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        Ok(LiveTvInfo::default())
    }

    /// Returns an empty program listing; `query` and `options` are ignored
    /// because there is no guide data.
    async fn get_programs(
        &self,
        _query: &InternalItemsQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        Ok(QueryResult::default())
    }

    /// No-op: there are no tuners to reset, so `id` is ignored and this always
    /// succeeds.
    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use super::*;

    /// Minimal single-poll executor: the manager's futures are ready without
    /// ever yielding, so no async runtime dependency is needed to test them.
    fn block_on<F: Future>(future: F) -> F::Output {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("DisabledLiveTvManager future was not immediately ready"),
        }
    }

    #[test]
    fn info_is_disabled() {
        let info = block_on(DisabledLiveTvManager.get_live_tv_info()).unwrap();
        assert!(!info.is_enabled);
        assert!(info.services.is_empty());
        assert!(info.enabled_users.is_empty());
    }

    #[test]
    fn programs_is_empty() {
        let result = block_on(
            DisabledLiveTvManager
                .get_programs(&InternalItemsQuery::default(), &DtoOptions::default()),
        )
        .unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total_record_count, 0);
    }

    #[test]
    fn reset_tuner_is_noop_ok() {
        assert!(block_on(DisabledLiveTvManager.reset_tuner("tuner-1")).is_ok());
    }

    #[test]
    fn coerces_to_dyn_manager() {
        let manager: Arc<dyn LiveTvManager> = Arc::new(DisabledLiveTvManager);
        assert!(!block_on(manager.get_live_tv_info()).unwrap().is_enabled);
    }
}
