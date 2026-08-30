//! The Internet-Channels-category scheduled task.
//!
//! Faithful port of upstream's `src/Jellyfin.LiveTv/Channels/RefreshChannelsScheduledTask.cs`
//! (key, name, description, category and default trigger), including its
//! untranslated display strings — upstream asks the localizer for
//! `TasksRefreshChannels`/`TasksRefreshChannelsDescription`, which are *not* in
//! `Core/en-US.json` (the shipped keys are `TaskRefreshChannels…`), and
//! `LocalizationManager.GetLocalizedString` returns the key itself when it is
//! missing. A live Jellyfin 10.11.8 therefore serves `"Name":
//! "TasksRefreshChannels"` for this task, and so does Ferrofin — parity beats
//! prettiness. If upstream ever adds the missing strings, this task's name and
//! description follow them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_model::tasks::TaskTriggerInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::ChannelManager;

use super::{ScheduledTask, TaskProgress, interval_hours};

/// The upstream Internet Channels category display string
/// (`TasksChannelsCategory`).
const CHANNELS: &str = "Internet Channels";

/// "Refresh Channels" — refreshes the internet channels' item listings. Port of
/// `RefreshChannelsScheduledTask`.
///
/// **What this does in Ferrofin, and what it cannot.** Upstream refreshes the
/// `IChannel` implementations contributed by .NET plugins. Ferrofin registers
/// no channel backends at all, and neither plugin tier can supply one:
/// compiled-in extensions have no channel seam, and sandboxed WASM plugins
/// expose no channel capability (`docs/EXTENSIONS.md` has the tiers; .NET
/// assembly loading is never coming). This task reads the channel set through
/// [`crate::FerrofinChannelManager`], the stub that answers every channel query
/// empty — note the `/Channels` HTTP surface does not go through that stub at
/// all, it returns empty results directly, so "no channels exist" is true on
/// both paths for the same reason. The run records the set's size for the
/// hidden rule and — with nothing in it — finishes. There is no per-channel
/// refresh entry point to call and this task does not pretend otherwise; what
/// is missing is the channel backend itself, not a call site.
///
/// Like upstream (`IsHidden => Channels.Length == 0`) the task hides itself
/// while that set is empty, which is what the Jellyfin oracle reports too, so
/// the dashboard matches.
pub struct RefreshChannelsTask {
    channels: Arc<dyn ChannelManager>,
    /// The channel count seen by the last run, backing the sync
    /// [`is_hidden`](ScheduledTask::is_hidden) rule (upstream reads
    /// `ChannelManager.Channels.Length`, a plain field; the seam here is
    /// async, so the count is cached).
    channel_count: AtomicUsize,
}

impl RefreshChannelsTask {
    /// Builds the task over the channel-manager seam, reading the channel set
    /// once so [`is_hidden`](ScheduledTask::is_hidden) is right from the first
    /// dashboard paint (upstream reads `Channels.Length` eagerly; the seam here
    /// is async, hence the constructor). Each run re-reads it.
    ///
    /// A failed read leaves the count at zero — the task hides rather than
    /// advertising channels it could not confirm.
    pub async fn new(channels: Arc<dyn ChannelManager>) -> Self {
        let count = match channels.get_channel_features(None).await {
            Ok(features) => features.len(),
            Err(e) => {
                tracing::warn!(error = %e, "could not seed the channel count");
                0
            }
        };
        Self {
            channels,
            channel_count: AtomicUsize::new(count),
        }
    }
}

#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for RefreshChannelsTask {
    fn key(&self) -> &str {
        "RefreshInternetChannels"
    }
    /// C# `RefreshChannelsScheduledTask` implements `IConfigurableScheduledTask`, so the
    /// `GET /ScheduledTasks` `isHidden`/`isEnabled` filters apply to it.
    fn is_configurable(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        // Upstream's untranslated localization key — see the module docs.
        "TasksRefreshChannels"
    }
    fn description(&self) -> &str {
        "TasksRefreshChannelsDescription"
    }
    fn category(&self) -> &str {
        CHANNELS
    }
    fn is_hidden(&self) -> bool {
        self.channel_count.load(Ordering::Relaxed) == 0
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        progress.report(0.0);
        let channels = self.channels.get_channel_features(None).await?;
        // Feeds `is_hidden` — an empty set keeps the task off the dashboard.
        self.channel_count.store(channels.len(), Ordering::Relaxed);
        if !channels.is_empty() {
            // Unreachable while Ferrofin registers no channel backends. If one
            // ever appears, this is where the refresh has to be implemented —
            // saying so is more use than a loop that walks the set and calls
            // nothing.
            tracing::warn!(
                channels = channels.len(),
                "channels are registered but Ferrofin has no channel refresh backend"
            );
        }
        progress.report(100.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_model::channels::{ChannelFeatures, ChannelQuery};
    use ferrofin_model::dto::BaseItemDto;
    use ferrofin_model::querying::QueryResult;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::options::InternalItemsQuery;
    use ferrofin_traits::stubs::ChannelManager;
    use uuid::Uuid;

    use super::{RefreshChannelsTask, ScheduledTask, TaskProgress};

    /// A channel manager advertising a fixed set of channels.
    struct FakeChannels(Vec<ChannelFeatures>);

    /// A channel manager whose reads fail.
    struct BrokenChannels;

    #[async_trait::async_trait]
    impl ChannelManager for BrokenChannels {
        async fn get_channel_features(
            &self,
            _id: Option<Uuid>,
        ) -> Result<Vec<ChannelFeatures>, ServiceError> {
            Err(ServiceError::backend("channels unavailable"))
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

    #[async_trait::async_trait]
    impl ChannelManager for FakeChannels {
        async fn get_channel_features(
            &self,
            _id: Option<Uuid>,
        ) -> Result<Vec<ChannelFeatures>, ServiceError> {
            Ok(self.0.clone())
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

    async fn task(channels: Vec<ChannelFeatures>) -> RefreshChannelsTask {
        RefreshChannelsTask::new(Arc::new(FakeChannels(channels))).await
    }

    #[tokio::test]
    async fn metadata_matches_upstream() {
        let task = task(Vec::new()).await;
        assert_eq!(task.key(), "RefreshInternetChannels");
        // Upstream serves the untranslated localization keys here.
        assert_eq!(task.name(), "TasksRefreshChannels");
        assert_eq!(task.description(), "TasksRefreshChannelsDescription");
        assert_eq!(task.category(), "Internet Channels");
        let triggers = task.default_triggers();
        assert_eq!(triggers.len(), 1);
        assert_eq!(
            triggers[0].type_,
            ferrofin_model::tasks::TaskTriggerInfoType::IntervalTrigger
        );
        // 24 hours, matching the oracle's `IntervalTicks`.
        assert_eq!(triggers[0].interval_ticks, Some(864_000_000_000));
    }

    #[tokio::test]
    async fn hidden_while_no_channel_is_registered() {
        let task = task(Vec::new()).await;
        assert!(task.is_hidden(), "hidden before the first run");
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert!(task.is_hidden(), "still hidden: no channels exist");
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn a_registered_channel_un_hides_the_task() {
        let task = task(vec![ChannelFeatures {
            name: "Example".to_owned(),
            id: Uuid::from_u128(7),
            ..ChannelFeatures::default()
        }])
        .await;
        // Visible from construction, not only after the first run — a hidden
        // task has no dashboard button to press, so it would never un-hide.
        assert!(
            !task.is_hidden(),
            "a channel exists, so the task is visible"
        );
        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");
        assert!(!task.is_hidden());
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn a_failed_read_hides_the_task_and_fails_the_run() {
        // Constructing over a broken seam must not advertise channels it could
        // not confirm…
        let task = RefreshChannelsTask::new(Arc::new(BrokenChannels)).await;
        assert!(task.is_hidden());
        // …and the run itself surfaces the error rather than reporting success.
        assert!(task.execute(&TaskProgress::default()).await.is_err());
    }
}
