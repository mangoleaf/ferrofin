//! The Live-TV-category scheduled tasks.
//!
//! Port of `src/Jellyfin.LiveTv/Guide/RefreshGuideScheduledTask.cs` over the
//! [`LiveTvManager`] seam: the 24-hour guide refresh Jellyfin registers for
//! every server, hidden from the dashboard until a tuner host exists (a stock
//! server has exactly one Live TV service, so `IsHidden` reduces to "no tuner
//! hosts configured").

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::LiveTvManager;

use super::{ScheduledTask, TaskProgress};

/// 100-nanosecond ticks per second (the `TaskTriggerInfo` time unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// The Live TV category display string. Unlike the Library/Maintenance
/// categories (which upstream localizes via `TasksLibraryCategory` /
/// `TasksMaintenanceCategory`), `RefreshGuideScheduledTask.Category` returns
/// this bare literal.
const LIVE_TV: &str = "Live TV";

/// "Refresh Guide" — re-fetches every tuner host (M3U) and listing provider
/// (XMLTV) and rewrites the channel/guide cache. Port of
/// `RefreshGuideScheduledTask`, whose `ExecuteAsync` is
/// `GuideManager.RefreshGuide`; Ferrofin's equivalent is
/// [`LiveTvManager::refresh_guide`].
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
        vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(24 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        }]
    }

    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // Divergence: upstream's `GuideManager.RefreshGuide(progress, ct)`
        // reports across its tuner and listing passes, so the dashboard shows a
        // moving bar; Ferrofin's `refresh_guide` takes no progress handle, so
        // the run reads 0% until it finishes. Closing it means widening the
        // manager seam, not faking intermediate percentages here.
        self.live_tv.refresh_guide().await?;
        progress.report(100.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// A [`LiveTvManager`] fake recording `refresh_guide` calls; every other
    /// method is unreachable in these tests.
    #[derive(Default)]
    struct FakeLiveTv {
        refreshes: AtomicU32,
        tuners: AtomicBool,
    }

    #[async_trait]
    impl LiveTvManager for FakeLiveTv {
        fn has_tuner_hosts(&self) -> bool {
            self.tuners.load(Ordering::Relaxed)
        }
        async fn refresh_guide(&self) -> Result<(), ServiceError> {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            Ok(())
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
        ) -> Result<ferrofin_model::live_tv::TunerChannelMapping, ServiceError> {
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
        async fn get_channel_stream_url(
            &self,
            _id: uuid::Uuid,
        ) -> Result<Option<String>, ServiceError> {
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
        async fn get_recording_path(
            &self,
            _id: uuid::Uuid,
        ) -> Result<Option<String>, ServiceError> {
            unimplemented!()
        }
        async fn delete_recording(&self, _id: uuid::Uuid) -> Result<(), ServiceError> {
            unimplemented!()
        }
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
