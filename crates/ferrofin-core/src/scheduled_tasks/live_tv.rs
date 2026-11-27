//! The Live-TV-category scheduled tasks.
//!
//! Port of `src/Jellyfin.LiveTv/Guide/RefreshGuideScheduledTask.cs` over the
//! [`LiveTvManager`] seam: the 24-hour guide refresh Jellyfin registers for
//! every server, hidden from the dashboard until a tuner host exists (a stock
//! server has exactly one Live TV service, so `IsHidden` reduces to "no tuner
//! hosts configured").

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::tasks::TaskTriggerInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::LiveTvManager;

use super::{ScheduledTask, TaskProgress, interval_hours};

/// The Live TV category display string. Unlike the Library/Maintenance
/// categories (which upstream localizes via `TasksLibraryCategory` /
/// `TasksMaintenanceCategory`), `RefreshGuideScheduledTask.Category` returns
/// this bare literal.
const LIVE_TV: &str = "Live TV";

/// "Refresh Guide" — re-fetches every tuner host (M3U) and listing provider
/// (XMLTV) and rewrites the channel/guide cache. Port of
/// `RefreshGuideScheduledTask`, whose `ExecuteAsync` is
/// `GuideManager.RefreshGuide`; Ferrofin's equivalent is
/// [`LiveTvManager::refresh_guide_until`], handed the run's progress and token.
pub struct RefreshGuideTask {
    live_tv: Arc<dyn LiveTvManager>,
}

impl RefreshGuideTask {
    /// Builds the task over the Live TV manager seam.
    #[must_use]
    pub fn new(live_tv: Arc<dyn LiveTvManager>) -> Self {
        Self { live_tv }
    }
}

// The metadata accessors return string literals, as every sibling task's do
// (the trait's `&str` return is shared with tasks whose strings are owned).
#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for RefreshGuideTask {
    fn key(&self) -> &str {
        "RefreshGuide"
    }
    /// C# `RefreshGuideScheduledTask` implements `IConfigurableScheduledTask`, so the
    /// `GET /ScheduledTasks` `isHidden`/`isEnabled` filters apply to it.
    fn is_configurable(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "Refresh Guide"
    }

    fn description(&self) -> &str {
        "Downloads channel information from live tv services."
    }

    fn category(&self) -> &str {
        LIVE_TV
    }

    fn is_hidden(&self) -> bool {
        // C# `IsHidden => Services.Count == 1 && TunerHosts.Length == 0`:
        // Ferrofin always has the one default service, so the task hides
        // exactly while no tuner host is configured.
        !self.live_tv.has_tuner_hosts()
    }

    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        // `IntervalTrigger` at `TimeSpan.FromHours(24).Ticks`.
        vec![interval_hours(24)]
    }

    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // `RefreshGuide(progress, cancellationToken)` throws for a run already
        // stopped: the check before its first service does (gl-2004).
        if progress.stop_requested() {
            return Ok(());
        }
        // `GuideManager.RefreshGuide(progress, cancellationToken)`: the
        // refresh reports through the run's handle and checks its token where
        // upstream's does, and says when a stop ended it: recorded Cancelled,
        // with no closing 100 %.
        if !self.live_tv.refresh_guide_until(progress).await? {
            progress.stopped();
            return Ok(());
        }
        // `RefreshGuide`'s closing `progress.Report(100)` (:167).
        progress.report(100.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::tasks::TaskTriggerInfoType;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// A [`LiveTvManager`] fake recording `refresh_guide` calls; every other
    /// method is unreachable in these tests.
    #[derive(Default)]
    struct FakeLiveTv {
        refreshes: AtomicU32,
        tuners: AtomicBool,
        /// A Stop arrives while the refresh runs.
        stopped_inside: AtomicBool,
    }

    /// What [`FakeLiveTv`]'s refresh reports through the run it is handed.
    const REFRESH_REPORT: f64 = 57.5;

    #[async_trait]
    impl LiveTvManager for FakeLiveTv {
        async fn get_guide_info(&self) -> Result<ferrofin_model::live_tv::GuideInfo, ServiceError> {
            unimplemented!("this fake is never asked for the guide window")
        }
        async fn get_recommended_programs(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            unimplemented!("this fake is never asked for recommended programs")
        }
        fn has_tuner_hosts(&self) -> bool {
            self.tuners.load(Ordering::Relaxed)
        }
        async fn refresh_guide(&self) -> Result<(), ServiceError> {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        async fn refresh_guide_until(&self, run: &TaskProgress) -> Result<bool, ServiceError> {
            self.refresh_guide().await?;
            run.report(REFRESH_REPORT);
            if self.stopped_inside.load(Ordering::Relaxed) {
                run.cancel_token().cancel();
            }
            Ok(!run.cancel_token().is_cancelled())
        }
        async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
            unreachable!("the guide-refresh task never asks for listings countries")
        }
        async fn get_live_tv_info(
            &self,
        ) -> Result<ferrofin_model::live_tv::LiveTvInfo, ServiceError> {
            unimplemented!()
        }
        async fn get_tuner_hosts(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::TunerHostInfo>, ServiceError> {
            unimplemented!()
        }
        async fn save_tuner_host(
            &self,
            _info: ferrofin_model::live_tv::TunerHostInfo,
        ) -> Result<ferrofin_model::live_tv::TunerHostInfo, ServiceError> {
            unimplemented!()
        }
        fn tuner_host_types(&self) -> Vec<ferrofin_model::dto::NameIdPair> {
            unimplemented!()
        }
        async fn discover_tuners(
            &self,
            _discovery_duration_ms: u64,
            _new_devices_only: bool,
        ) -> Result<Vec<ferrofin_model::live_tv::TunerHostInfo>, ServiceError> {
            unimplemented!()
        }
        async fn delete_tuner_host(&self, _id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_listing_providers(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::ListingsProviderInfo>, ServiceError> {
            unimplemented!()
        }
        async fn save_listing_provider(
            &self,
            _info: ferrofin_model::live_tv::ListingsProviderInfo,
            _validate_login: bool,
            _validate_listings: bool,
        ) -> Result<ferrofin_model::live_tv::ListingsProviderInfo, ServiceError> {
            unimplemented!()
        }
        async fn get_lineups(
            &self,
            _provider_id: Option<&str>,
            _provider_type: Option<&str>,
            _country: Option<&str>,
            _location: Option<&str>,
        ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
            unimplemented!()
        }
        async fn get_channel_mapping_options(
            &self,
            _provider_id: &str,
        ) -> Result<ferrofin_model::live_tv::ChannelMappingOptionsDto, ServiceError> {
            unimplemented!()
        }
        async fn set_channel_mapping(
            &self,
            _provider_id: &str,
            _tuner_channel_id: &str,
            _provider_channel_id: &str,
        ) -> Result<Option<ferrofin_model::live_tv::TunerChannelMapping>, ServiceError> {
            unimplemented!()
        }
        async fn delete_listing_provider(&self, _id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_channels(
            &self,
            _query: &ferrofin_traits::stubs::LiveTvChannelQuery,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_channel(
            &self,
            _id: uuid::Uuid,
            _user: Option<&ferrofin_db::entities::users::UserEntity>,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
            unimplemented!()
        }
        async fn get_programs(
            &self,
            _query: &ferrofin_traits::options::InternalItemsQuery,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_program(
            &self,
            _id: uuid::Uuid,
            _user: Option<&ferrofin_db::entities::users::UserEntity>,
            _options: &ferrofin_traits::options::DtoOptions,
        ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
            unimplemented!()
        }
        async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_timers(
            &self,
        ) -> Result<Vec<ferrofin_model::live_tv::TimerInfoDto>, ServiceError> {
            unimplemented!()
        }
        async fn get_timer(
            &self,
            _id: &str,
        ) -> Result<Option<ferrofin_model::live_tv::TimerInfoDto>, ServiceError> {
            unimplemented!()
        }
        async fn create_timer(
            &self,
            _timer: ferrofin_model::live_tv::TimerInfoDto,
        ) -> Result<String, ServiceError> {
            unimplemented!()
        }
        async fn update_timer(
            &self,
            _id: &str,
            _timer: ferrofin_model::live_tv::TimerInfoDto,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn cancel_timer(&self, _id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_series_timers(
            &self,
            _query: &ferrofin_model::live_tv::SeriesTimerQuery,
        ) -> Result<Vec<ferrofin_model::live_tv::SeriesTimerInfoDto>, ServiceError> {
            unimplemented!()
        }
        async fn get_series_timer(
            &self,
            _id: &str,
        ) -> Result<Option<ferrofin_model::live_tv::SeriesTimerInfoDto>, ServiceError> {
            unimplemented!()
        }
        async fn create_series_timer(
            &self,
            _timer: ferrofin_model::live_tv::SeriesTimerInfoDto,
        ) -> Result<String, ServiceError> {
            unimplemented!()
        }
        async fn update_series_timer(
            &self,
            _id: &str,
            _timer: ferrofin_model::live_tv::SeriesTimerInfoDto,
        ) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn cancel_series_timer(&self, _id: &str) -> Result<(), ServiceError> {
            unimplemented!()
        }
        async fn get_recordings(
            &self,
        ) -> Result<
            ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>,
            ServiceError,
        > {
            unimplemented!()
        }
        async fn get_recording(
            &self,
            _id: uuid::Uuid,
        ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
            unimplemented!()
        }
        async fn delete_recording(&self, _id: uuid::Uuid) -> Result<(), ServiceError> {
            unimplemented!()
        }
    }

    /// gl-2004: a run stopped before it starts refreshes no guide, and is
    /// recorded Cancelled.
    #[tokio::test]
    async fn a_stopped_run_refreshes_no_guide() {
        let fake = Arc::new(FakeLiveTv::default());
        let task = RefreshGuideTask::new(fake.clone());
        let progress = TaskProgress::default();
        progress.cancel_token().cancel();
        task.execute(&progress).await.expect("run");
        assert_eq!(fake.refreshes.load(Ordering::Relaxed), 0);
        assert!(progress.was_stopped(), "recorded Cancelled");
    }

    /// The refresh is handed the run itself
    /// (`RefreshGuide(progress, cancellationToken)`): what it reports shows on
    /// the run, and a stop inside it reaches it through the run's own token.
    /// It ends early and the run is recorded Cancelled, with no closing 100 %.
    #[tokio::test]
    async fn a_stop_inside_the_refresh_cancels_the_run() {
        let fake = Arc::new(FakeLiveTv::default());
        fake.stopped_inside.store(true, Ordering::Relaxed);
        let task = RefreshGuideTask::new(fake.clone());
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert_eq!(fake.refreshes.load(Ordering::Relaxed), 1);
        assert!(
            progress.cancel_token().is_cancelled(),
            "the refresh was handed the run's token"
        );
        assert!(progress.was_stopped(), "recorded Cancelled");
        assert_eq!(
            progress.reported(),
            Some(REFRESH_REPORT),
            "the refresh's own report, and no 100 % for a stopped refresh"
        );
    }

    #[tokio::test]
    async fn refresh_guide_task_matches_upstream_metadata_and_runs_the_refresh() {
        let fake = Arc::new(FakeLiveTv::default());
        let task = RefreshGuideTask::new(fake.clone());

        assert_eq!(task.key(), "RefreshGuide");
        assert_eq!(task.name(), "Refresh Guide");
        assert_eq!(
            task.description(),
            "Downloads channel information from live tv services."
        );
        assert_eq!(task.category(), "Live TV");

        // Hidden until a tuner host exists (one service on a stock server).
        assert!(task.is_hidden());
        fake.tuners.store(true, Ordering::Relaxed);
        assert!(!task.is_hidden());

        // The default trigger is the upstream 24 h interval.
        let triggers = task.default_triggers();
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].type_, TaskTriggerInfoType::IntervalTrigger);
        assert_eq!(triggers[0].interval_ticks, Some(864_000_000_000));

        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("execute");
        assert_eq!(fake.refreshes.load(Ordering::Relaxed), 1);
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }
}
