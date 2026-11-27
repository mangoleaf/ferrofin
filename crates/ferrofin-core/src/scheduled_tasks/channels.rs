//! The Internet-Channels-category scheduled task.
//!
//! Faithful port of upstream's `src/Jellyfin.LiveTv/Channels/RefreshChannelsScheduledTask.cs`
//! (key, the localization keys of its name, description and category, and its
//! default trigger). Up to 10.11.8 upstream asked for the missing
//! `TasksRefreshChannels…` keys and served them raw;
//! jellyfin/jellyfin#16488 (v12.0-rc1) moved to the shipped
//! `TaskRefreshChannels…` keys, so Jellyfin 12 serves "Refresh Channels".

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::tasks::TaskTriggerInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::options::{DeleteOptions, InternalItemsQuery};
use ferrofin_traits::stubs::ChannelManager;
use uuid::Uuid;

use super::{ScheduledTask, TaskProgress, interval_hours};

/// The upstream Internet Channels category's localization key.
const CHANNELS: &str = "TasksChannelsCategory";

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
/// hidden rule. There is no per-channel refresh entry point to call and this
/// task does not pretend otherwise; what is missing is the channel backend
/// itself, not a call site.
///
/// The run then cleans the database as upstream's `ChannelPostScanTask` does:
/// every stored `Channel` item that is not one of the set, with every item of
/// it, is deleted with its metadata folders, its media files left alone. With
/// the set empty that is every stored channel, such as those an adopted
/// Jellyfin database kept from a channel plugin.
///
/// Like upstream (`IsHidden => Channels.Length == 0`) the task hides itself
/// while that set is empty, which is what the Jellyfin oracle reports too, so
/// the dashboard matches.
pub struct RefreshChannelsTask {
    channels: Arc<dyn ChannelManager>,
    /// Lists and deletes the stored channels no channel backend has.
    library: Arc<dyn LibraryManager>,
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
    pub async fn new(channels: Arc<dyn ChannelManager>, library: Arc<dyn LibraryManager>) -> Self {
        let count = match channels.get_channel_features(None).await {
            Ok(features) => features.len(),
            Err(e) => {
                tracing::warn!(error = %e, "could not seed the channel count");
                0
            }
        };
        Self {
            channels,
            library,
            channel_count: AtomicUsize::new(count),
        }
    }

    /// `ChannelPostScanTask.CleanChannel`: deletes every item of `channel`,
    /// then the channel, leaving their files. `false` when the run was stopped
    /// before it finished.
    async fn clean_channel(
        &self,
        channel: Uuid,
        progress: &TaskProgress,
    ) -> Result<bool, ServiceError> {
        tracing::info!(item_id = %channel, "cleaning channel from database");
        let items = self
            .library
            .get_item_ids(&InternalItemsQuery {
                channel_ids: vec![channel],
                ..InternalItemsQuery::default()
            })
            .await?;
        let keep_files = DeleteOptions {
            delete_file_location: false,
            ..DeleteOptions::default()
        };
        for item in items {
            if progress.stop_requested() {
                return Ok(false);
            }
            self.library.delete_item(item, &keep_files).await?;
        }
        self.library.delete_item(channel, &keep_files).await?;
        Ok(true)
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
        "TaskRefreshChannels"
    }
    fn description(&self) -> &str {
        "TaskRefreshChannelsDescription"
    }
    fn category(&self) -> &str {
        CHANNELS
    }
    fn is_localized(&self) -> bool {
        true
    }
    fn is_hidden(&self) -> bool {
        self.channel_count.load(Ordering::Relaxed) == 0
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![interval_hours(24)]
    }
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
        // No report before the refresh: upstream's `RefreshChannels` gets its
        // own `Progress<double>`, so the run shows none until the end.
        let channels = self.channels.get_channel_features(None).await?;
        // Feeds `is_hidden` — an empty set keeps the task off the dashboard.
        self.channel_count.store(channels.len(), Ordering::Relaxed);
        if !channels.is_empty() {
            // Unreachable while Ferrofin registers no channel backends. If one
            // ever appears, this is where the refresh has to be implemented —
            // saying so is more use than a loop that walks the set and calls
            // nothing. That refresh checks the stop token before each channel,
            // as upstream's `RefreshChannels` does; with none registered today
            // there is nothing to stop.
            tracing::warn!(
                channels = channels.len(),
                "channels are registered but Ferrofin has no channel refresh backend"
            );
        }
        // `ChannelPostScanTask.CleanDatabase` (gl-2017): the stored channels
        // no installed channel is, each checked for a stop first.
        let uninstalled = self
            .library
            .get_item_ids(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Channel],
                exclude_item_ids: channels.iter().map(|channel| channel.id).collect(),
                ..InternalItemsQuery::default()
            })
            .await?;
        // A nil id would list every item of no channel (the nil `ChannelId`
        // ordinary rows store): a corrupt row upstream's MD5 ids never make.
        for channel in uninstalled.into_iter().filter(|id| !id.is_nil()) {
            if progress.stop_requested() || !self.clean_channel(channel, progress).await? {
                return Ok(());
            }
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
        let library =
            crate::test_support::library_manager_over(crate::test_support::test_db().await);
        RefreshChannelsTask::new(Arc::new(FakeChannels(channels)), library).await
    }

    #[tokio::test]
    async fn metadata_matches_upstream() {
        let task = task(Vec::new()).await;
        assert_eq!(task.key(), "RefreshInternetChannels");
        // Upstream's localization keys since jellyfin/jellyfin#16488 (v12.0-rc1).
        assert!(task.is_localized());
        assert_eq!(task.name(), "TaskRefreshChannels");
        assert_eq!(task.description(), "TaskRefreshChannelsDescription");
        assert_eq!(task.category(), "TasksChannelsCategory");
        let triggers = task.default_triggers();
        assert_eq!(triggers.len(), 1);
        assert_eq!(
            triggers[0].type_,
            ferrofin_model::tasks::TaskTriggerInfoType::IntervalTrigger
        );
        // 24 hours, matching the oracle's `IntervalTicks`.
        assert_eq!(triggers[0].interval_ticks, Some(864_000_000_000));
    }

    /// The registry lists the task in the ambient UI culture, as Jellyfin 12
    /// serves it: "Refresh Channels" in English, never the raw key.
    #[tokio::test]
    async fn the_registry_lists_it_in_the_request_culture() {
        let registry = super::super::FerrofinTaskManager::new();
        registry.register(Arc::new(task(Vec::new()).await));
        let english = registry.get("RefreshInternetChannels").expect("registered");
        assert_eq!(english.name.as_deref(), Some("Refresh Channels"));
        assert_eq!(
            english.description.as_deref(),
            Some("Refreshes internet channel information.")
        );
        assert_eq!(english.category.as_deref(), Some("Internet Channels"));

        let german = ferrofin_util::current_culture::scope_with_ui(
            "de".to_owned(),
            Some("de".to_owned()),
            async { registry.get("RefreshInternetChannels").expect("registered") },
        )
        .await;
        assert_eq!(german.name.as_deref(), Some("Kanäle aktualisieren"));
        assert_eq!(
            german.description.as_deref(),
            Some("Aktualisiert Internet-Kanal-Informationen.")
        );
        assert_eq!(german.category.as_deref(), Some("Internet-Kanäle"));
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

    /// `ChannelPostScanTask.CleanDatabase` (gl-2017): a stored channel no
    /// backend has is deleted with its items, files left alone; a channel the
    /// set has, and an item of no channel, stay.
    #[tokio::test]
    async fn a_run_deletes_the_stored_channels_no_backend_has() {
        use crate::test_support::{seed_channel, seed_channel_item, seed_item, test_db};
        use ferrofin_model::data::BaseItemKind;

        let db = test_db().await;
        let (gone, kept) = (Uuid::from_u128(0xC1), Uuid::from_u128(0xC2));
        let (gone_item, gone_second, kept_item, movie) = (
            Uuid::from_u128(0xC11),
            Uuid::from_u128(0xC12),
            Uuid::from_u128(0xC21),
            Uuid::from_u128(0xF1),
        );
        let (tuner_channel, programme) = (Uuid::from_u128(0x71), Uuid::from_u128(0x711));
        // A corrupt channel row with the nil id is never cleaned: it would
        // list every item of no channel.
        seed_item(&db, Uuid::nil(), BaseItemKind::Channel).await;
        // As upstream stores a channel: its own id as its `ChannelId`, so it
        // is among its own items, and deleting it first is harmless.
        seed_channel(&db, gone).await;
        seed_channel(&db, kept).await;
        seed_channel_item(&db, gone_item, BaseItemKind::Folder, gone).await;
        seed_channel_item(&db, gone_second, BaseItemKind::Video, gone).await;
        seed_channel_item(&db, kept_item, BaseItemKind::Video, kept).await;
        // Stored as an ordinary row is: a nil `ChannelId`, not NULL.
        seed_channel_item(&db, movie, BaseItemKind::Movie, Uuid::nil()).await;
        // Live TV rows carry their tuner channel's id in `ChannelId` too.
        seed_item(&db, tuner_channel, BaseItemKind::LiveTvChannel).await;
        seed_channel_item(&db, programme, BaseItemKind::LiveTvProgram, tuner_channel).await;

        let library = crate::test_support::library_manager_over(db.clone());
        let task = RefreshChannelsTask::new(
            Arc::new(FakeChannels(vec![ChannelFeatures {
                id: kept,
                ..ChannelFeatures::default()
            }])),
            library.clone(),
        )
        .await;

        let progress = TaskProgress::default();
        task.execute(&progress).await.expect("run");

        for (id, stays) in [
            (gone, false),
            (gone_item, false),
            (gone_second, false),
            (kept, true),
            (kept_item, true),
            (movie, true),
            (tuner_channel, true),
            (programme, true),
        ] {
            assert_eq!(
                library.get_item_by_id(id).await.expect("read").is_some(),
                stays,
                "{id}"
            );
        }
        assert!((progress.current() - 100.0).abs() < f64::EPSILON);
    }

    /// A stop before a channel is cleaned leaves it, and the run is Cancelled
    /// (`ThrowIfCancellationRequested` before each channel). The channel has
    /// no items, so only the check before each channel can keep it.
    #[tokio::test]
    async fn a_stopped_run_deletes_no_channel() {
        use crate::test_support::{seed_channel, test_db};

        let db = test_db().await;
        let channel = Uuid::from_u128(0xC1);
        seed_channel(&db, channel).await;
        let library = crate::test_support::library_manager_over(db.clone());
        let task =
            RefreshChannelsTask::new(Arc::new(FakeChannels(Vec::new())), library.clone()).await;

        let progress = TaskProgress::default();
        progress.cancel_token().cancel();
        task.execute(&progress).await.expect("run");

        let kept = library.get_item_by_id(channel).await.expect("read");
        assert!(kept.is_some(), "the channel stays");
        assert!(progress.was_stopped(), "recorded Cancelled");
    }

    /// The check before each item (`ThrowIfCancellationRequested` in
    /// `CleanChannel`): a stopped run deletes none of a channel's items, nor
    /// the channel.
    #[tokio::test]
    async fn a_stop_inside_a_channel_deletes_none_of_its_items() {
        use crate::test_support::{seed_channel, seed_channel_item, test_db};
        use ferrofin_model::data::BaseItemKind;

        let db = test_db().await;
        let (channel, first, second) = (
            Uuid::from_u128(0xC1),
            Uuid::from_u128(0xC11),
            Uuid::from_u128(0xC12),
        );
        seed_channel(&db, channel).await;
        seed_channel_item(&db, first, BaseItemKind::Video, channel).await;
        seed_channel_item(&db, second, BaseItemKind::Video, channel).await;
        let library = crate::test_support::library_manager_over(db.clone());
        let task =
            RefreshChannelsTask::new(Arc::new(FakeChannels(Vec::new())), library.clone()).await;

        let progress = TaskProgress::default();
        progress.cancel_token().cancel();
        let finished = task.clean_channel(channel, &progress).await.expect("clean");

        assert!(!finished, "stopped");
        for id in [channel, first, second] {
            let kept = library.get_item_by_id(id).await.expect("read");
            assert!(kept.is_some(), "{id}");
        }
    }

    #[tokio::test]
    async fn a_failed_read_hides_the_task_and_fails_the_run() {
        // Constructing over a broken seam must not advertise channels it could
        // not confirm…
        let library =
            crate::test_support::library_manager_over(crate::test_support::test_db().await);
        let task = RefreshChannelsTask::new(Arc::new(BrokenChannels), library).await;
        assert!(task.is_hidden());
        // …and the run itself surfaces the error rather than reporting success.
        let progress = TaskProgress::default();
        assert!(task.execute(&progress).await.is_err());
        // Nor any progress: upstream's `RefreshChannels` gets its own
        // `Progress<double>`, and only `ChannelPostScanTask` reports, at its end.
        assert_eq!(progress.reported(), None);
    }
}
