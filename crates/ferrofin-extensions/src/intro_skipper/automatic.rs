//! Automatic analysis — port of the plugin's `Entrypoint` (intro-skipper
//! `db09359`): seasons whose items are added or updated are queued, and
//! analysed once the library has been quiet for a while; a library scan's end
//! starts the same wait. A removed item's cache is deleted. Gated by
//! `AutoDetectIntros`.
//!
//! The events arrive through the server's event bus (owner decision D8): the
//! library-change notifier's in-process `ItemsChanged`, the task manager's
//! `TaskCompleted` and the plugin manager's `PluginConfigurationChanged`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrofin_core::TaskProgress;
use ferrofin_model::data::BaseItemKind;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{DetectSegmentsTask, EXTENSION_ID, ReleaseOnDrop, queue};

/// The wait after an item is added (`OnItemChanged`, `UpdateReason == 0`).
const ADDED_DELAY: Duration = Duration::from_secs(120);
/// The wait after an item is updated, a library scan ends, or a run ended with
/// more queued (`StartTimer`'s default).
const UPDATED_DELAY: Duration = Duration::from_secs(60);
/// How often a run waiting for the one-pass latch looks again: the wait is
/// a whole analysis pass (minutes), so a second's lag is noise.
// TODO: wake waiters through a `tokio::sync::Notify` on release instead of
// polling, if latch waits ever matter (many queued runs, or tests).
const LATCH_POLL: Duration = Duration::from_secs(1);

/// The Intro Skipper's run state, shared by every handle on it (the plugin's
/// statics and `Plugin.Instance` flags).
#[derive(Debug, Default)]
pub struct Runtime {
    /// `ScheduledTaskSemaphore`: one analysis at a time.
    pub(super) running: Arc<AtomicBool>,
    /// `Plugin.AnalyzeAgain`: the settings changed since the last pass, so
    /// the next one analyses again what earlier ones recorded.
    pub(super) analyze_again: AtomicBool,
    automatic: Mutex<Automatic>,
    /// `WarningManager`'s flags (`PluginWarning`).
    warnings: AtomicU8,
    /// `TotalQueued` / `TotalSeasons`: the last queue's analysable entries
    /// and seasons.
    pub(super) queued: AtomicUsize,
    pub(super) seasons: AtomicUsize,
    /// The parsed configuration (`Plugin.Configuration`), read once and again
    /// after each save. A tokio lock held across the read, so a save landing
    /// mid-read still ends with the cache empty, never stale.
    pub(super) config: tokio::sync::Mutex<Option<Arc<super::IntroSkipperConfig>>>,
}

/// `PluginWarning.InvalidChromaprintFingerprint`: a fingerprint failed.
pub(super) const INVALID_CHROMAPRINT_FINGERPRINT: u8 = 2;
/// `PluginWarning.IncompatibleFFmpegBuild`: ffmpeg cannot fingerprint.
pub(super) const INCOMPATIBLE_FFMPEG_BUILD: u8 = 4;

impl Runtime {
    /// `WarningManager.SetFlag`.
    pub(super) fn warn(&self, warning: u8) {
        self.warnings.fetch_or(warning, Ordering::Relaxed);
    }

    /// `WarningManager.GetWarnings`: the `[Flags]` enum's `ToString()`.
    pub(super) fn warnings(&self) -> String {
        let flags = self.warnings.load(Ordering::Relaxed);
        let names: Vec<&str> = [
            (
                INVALID_CHROMAPRINT_FINGERPRINT,
                "InvalidChromaprintFingerprint",
            ),
            (INCOMPATIBLE_FFMPEG_BUILD, "IncompatibleFFmpegBuild"),
        ]
        .into_iter()
        .filter(|(flag, _)| flags & flag != 0)
        .map(|(_, name)| name)
        .collect();
        if names.is_empty() {
            "None".to_owned()
        } else {
            names.join(", ")
        }
    }
}

/// `Entrypoint`'s state.
#[derive(Debug, Default)]
struct Automatic {
    /// `_seasonsToAnalyze`.
    seasons: HashSet<Uuid>,
    /// `_queueTimer`.
    timer: Option<JoinHandle<()>>,
    /// The run (`_cancellationTokenSource`): `Running` until it finishes.
    run: Option<JoinHandle<()>>,
    /// `_analyzeAgain`: more was queued while a run was going.
    again: bool,
}

impl Automatic {
    /// `AutomaticTaskState == Running`.
    fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| !run.is_finished())
    }
}

impl DetectSegmentsTask {
    /// Waits for the one-pass latch (`ScheduledTaskSemaphore.AcquireAsync`);
    /// held until the returned guard drops.
    pub(super) async fn acquire_latch(&self) -> ReleaseOnDrop {
        while self.running.swap(true, Ordering::SeqCst) {
            tokio::time::sleep(LATCH_POLL).await;
        }
        ReleaseOnDrop(Arc::clone(&self.running))
    }

    /// `CancelAutomaticTaskAsync`: stops the automatic run, if one is going,
    /// and waits for it to let go of the latch.
    pub(super) async fn cancel_automatic(&self) {
        let run = self
            .runtime
            .automatic
            .lock()
            .ok()
            .and_then(|mut auto| auto.run.take());
        if let Some(run) = run.filter(|run| !run.is_finished()) {
            tracing::info!(
                "intro skipper: cancelling the automatic analysis for the scheduled one"
            );
            run.abort();
            let _ = run.await;
        }
    }

    /// `OnItemChanged` / `OnItemRemoved`, for one change set.
    pub(super) async fn items_changed(&self, added: &[Uuid], updated: &[Uuid], removed: &[Uuid]) {
        if !self.enabled().await || !self.load_config().await.auto_detect_intros {
            return;
        }
        if !removed.is_empty()
            && let Err(err) = self.erase_cache_files(Some(removed), None).await
        {
            tracing::warn!(%err, "intro skipper: removed items' cache not deleted");
        }
        // An added item waits longer (its metadata is still arriving); one
        // change set counts as added when any of it is.
        let delay = if added.is_empty() {
            UPDATED_DELAY
        } else {
            ADDED_DELAY
        };
        let ids: Vec<Uuid> = added.iter().chain(updated).copied().collect();
        let mut seasons = HashSet::new();
        // One lookup per chunk, not per item: a first import announces a
        // whole library at once.
        for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
            let query = ferrofin_traits::options::InternalItemsQuery {
                item_ids: chunk.to_vec(),
                include_item_types: vec![BaseItemKind::Episode, BaseItemKind::Movie],
                is_virtual_item: Some(false),
                ..ferrofin_traits::options::InternalItemsQuery::default()
            };
            let items = match self.library.get_item_list(&query).await {
                Ok(items) => items,
                Err(err) => {
                    tracing::warn!(%err, "intro skipper: changed items unreadable; they wait for the next pass");
                    continue;
                }
            };
            seasons.extend(items.iter().filter_map(|item| {
                match queue::kind(item) {
                    Some(BaseItemKind::Episode) => item
                        .season_id
                        .as_deref()
                        .and_then(|s| Uuid::parse_str(s).ok()),
                    Some(BaseItemKind::Movie) => Uuid::parse_str(&item.id).ok(),
                    _ => None,
                }
            }));
        }
        if seasons.is_empty() {
            return;
        }
        if let Ok(mut auto) = self.runtime.automatic.lock() {
            auto.seasons.extend(seasons);
        }
        self.start_timer(delay);
    }

    /// `OnLibraryRefresh`: a completed library scan starts the wait.
    pub(super) async fn task_completed(&self, key: &str, completed: bool) {
        if key == "RefreshLibrary"
            && completed
            && self.enabled().await
            && self.load_config().await.auto_detect_intros
            && !self
                .runtime
                .automatic
                .lock()
                .is_ok_and(|auto| auto.running())
        {
            self.start_timer(UPDATED_DELAY);
        }
    }

    /// `OnSettingsChanged`: the next pass analyses everything again.
    pub(super) async fn plugin_configuration_changed(&self, plugin_id: Uuid) {
        if plugin_id == EXTENSION_ID {
            *self.runtime.config.lock().await = None;
            tracing::debug!(
                "intro skipper: settings saved; the next pass analyses everything again"
            );
            self.runtime.analyze_again.store(true, Ordering::SeqCst);
        }
    }

    /// `StartTimer`: (re)starts the wait, unless a run is going — then it
    /// runs again when done.
    fn start_timer(&self, delay: Duration) {
        let Ok(mut auto) = self.runtime.automatic.lock() else {
            return;
        };
        if auto.running() {
            auto.again = true;
            return;
        }
        tracing::debug!(
            ?delay,
            "intro skipper: library changed; automatic analysis queued"
        );
        self.schedule(&mut auto, delay);
    }

    /// `_queueTimer.Change(delay)`: the run starts after `delay`, replacing a
    /// pending start.
    fn schedule(&self, auto: &mut Automatic, delay: Duration) {
        if let Some(timer) = auto.timer.take() {
            timer.abort();
        }
        let task = self.clone();
        auto.timer = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            task.start_run();
        }));
    }

    /// `OnTimerCallback`.
    fn start_run(&self) {
        let Ok(mut auto) = self.runtime.automatic.lock() else {
            return;
        };
        if auto.running() {
            return;
        }
        let task = self.clone();
        auto.run = Some(tokio::spawn(async move { task.run_automatic().await }));
    }

    /// `PerformAnalysisAsync`: analyse the queued seasons under the latch,
    /// and again later when more was queued meanwhile.
    async fn run_automatic(&self) {
        let _latch = self.acquire_latch().await;
        // The plugin may have been turned off during the wait.
        if !self.enabled().await {
            return;
        }
        let seasons = match self.runtime.automatic.lock() {
            Ok(mut auto) => {
                auto.again = false;
                std::mem::take(&mut auto.seasons)
            }
            Err(_) => return,
        };
        tracing::info!(
            seasons = seasons.len(),
            "intro skipper: automatic analysis started"
        );
        // `AnalyzeItemsAsync(seasonsToAnalyze)`: an empty set is nothing to do.
        if !seasons.is_empty() {
            let config = self.load_config().await;
            match self.queue(&config).await {
                Ok(queue) => {
                    let queue: queue::Queue = queue
                        .into_iter()
                        .filter(|(season, _)| seasons.contains(season))
                        .collect();
                    self.analyze_queue(&queue, &config, &TaskProgress::default())
                        .await;
                }
                Err(err) => {
                    tracing::warn!(%err, "intro skipper: could not build the analysis queue");
                }
            }
        }
        if let Ok(mut auto) = self.runtime.automatic.lock()
            && auto.again
        {
            tracing::info!(
                "intro skipper: more changed during the automatic analysis; it runs again"
            );
            self.schedule(&mut auto, UPDATED_DELAY);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use ferrofin_core::ScheduledTask as _;
    use ferrofin_traits::intro_skipper::IntroSkipperAnalysis;

    use super::*;
    use crate::intro_skipper::tests::{
        EP_A, SEASON, TWO_EPISODES, cache_files, harness, intros_of,
    };

    fn queued(task: &DetectSegmentsTask) -> (HashSet<Uuid>, bool) {
        let auto = task.runtime.automatic.lock().expect("lock");
        (auto.seasons.clone(), auto.timer.is_some())
    }

    /// `OnItemChanged`: an added or updated episode queues its season and
    /// starts the wait; `OnItemRemoved` deletes the item's cache. Nothing
    /// without `AutoDetectIntros`.
    #[tokio::test]
    async fn library_changes_queue_their_seasons() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("cache");
        assert!(!cache_files(&h, EP_A).is_empty());
        h.task.items_changed(&[], &[EP_A], &[]).await;
        assert_eq!(queued(&h.task), (HashSet::from([SEASON]), true));
        IntroSkipperAnalysis::items_changed(&h.task, &[], &[], &[EP_A]).await;
        assert!(cache_files(&h, EP_A).is_empty(), "the removed item's cache");

        let off = harness(&TWO_EPISODES, true, r#"{"AutoDetectIntros":false}"#, true).await;
        off.task.items_changed(&[EP_A], &[], &[]).await;
        assert_eq!(queued(&off.task), (HashSet::new(), false));
    }

    /// `OnLibraryRefresh`: only a completed `RefreshLibrary` starts the wait.
    #[tokio::test]
    async fn a_finished_library_scan_starts_the_wait() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task.task_completed("RefreshLibrary", false).await;
        h.task
            .task_completed("IntroSkipperDetectSegmentsTask", true)
            .await;
        assert!(!queued(&h.task).1);
        h.task.task_completed("RefreshLibrary", true).await;
        assert!(queued(&h.task).1);
    }

    /// `PerformAnalysisAsync`: the queued seasons are analysed (and taken);
    /// none queued is nothing to do.
    #[tokio::test]
    async fn the_automatic_run_analyses_the_queued_seasons() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task.run_automatic().await;
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
        h.task.items_changed(&[EP_A], &[], &[]).await;
        h.task.run_automatic().await;
        assert!(h.calls.load(Ordering::SeqCst) > 0);
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
        assert!(queued(&h.task).0.is_empty());
        assert!(!h.task.running.load(Ordering::SeqCst));
    }

    /// `OnSettingsChanged` → `AnalyzeAgain`: the next pass analyses what the
    /// last one recorded even though no hashed setting changed, and consumes
    /// the flag. Another plugin's save is not this one's.
    #[tokio::test]
    async fn a_settings_save_analyses_everything_again() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("first");
        let erase = || std::fs::remove_dir_all(&h.task.cache_dir).expect("drop the cache");
        erase();
        let first = h.calls.load(Ordering::SeqCst);
        h.task
            .plugin_configuration_changed(Uuid::from_u128(1))
            .await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("second");
        assert_eq!(h.calls.load(Ordering::SeqCst), first, "already analysed");

        h.task.plugin_configuration_changed(EXTENSION_ID).await;
        h.task
            .execute(&TaskProgress::default())
            .await
            .expect("again");
        assert!(h.calls.load(Ordering::SeqCst) > first, "analysed again");
        assert!(!h.task.runtime.analyze_again.load(Ordering::SeqCst));
    }

    /// A scheduled pass cancels the automatic one (`CancelAutomaticTaskAsync`)
    /// rather than waiting behind it.
    #[tokio::test]
    async fn a_scheduled_pass_cancels_the_automatic_one() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let stuck = {
            let task = h.task.clone();
            tokio::spawn(async move {
                let _latch = task.acquire_latch().await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.task.runtime.automatic.lock().expect("lock").run = Some(stuck);
        tokio::time::timeout(
            Duration::from_secs(30),
            h.task.execute(&TaskProgress::default()),
        )
        .await
        .expect("not stuck behind the automatic run")
        .expect("run");
        assert!(h.calls.load(Ordering::SeqCst) > 0);
    }

    /// `StartTimer` while a run is going: no new wait, the run goes again
    /// when done.
    #[tokio::test]
    async fn a_change_during_a_run_marks_it_to_run_again() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        let run = tokio::spawn(tokio::time::sleep(Duration::from_secs(3600)));
        h.task.runtime.automatic.lock().expect("lock").run = Some(run);
        h.task.start_timer(UPDATED_DELAY);
        let auto = h.task.runtime.automatic.lock().expect("lock");
        assert!(auto.again && auto.timer.is_none());
    }

    /// `_queueTimer.Change`: a later change replaces the pending wait, and the
    /// wait's end runs the analysis.
    #[tokio::test]
    async fn the_wait_restarts_and_then_runs() {
        let h = harness(&TWO_EPISODES, true, "{}", true).await;
        h.task.items_changed(&[EP_A], &[], &[]).await;
        {
            let mut auto = h.task.runtime.automatic.lock().expect("lock");
            h.task.schedule(&mut auto, Duration::from_millis(50));
            h.task.schedule(&mut auto, Duration::from_secs(3600));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            h.task.runtime.automatic.lock().expect("lock").run.is_none(),
            "replaced"
        );
        {
            let mut auto = h.task.runtime.automatic.lock().expect("lock");
            h.task.schedule(&mut auto, Duration::from_millis(50));
        }
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let done = h
                .task
                .runtime
                .automatic
                .lock()
                .expect("lock")
                .run
                .as_ref()
                .is_some_and(JoinHandle::is_finished);
            if done {
                break;
            }
        }
        assert_eq!(intros_of(&h.segments, EP_A).await.len(), 1);
    }

    /// A run after the plugin was turned off does nothing.
    #[tokio::test]
    async fn a_disabled_plugin_runs_nothing() {
        let h = harness(&TWO_EPISODES, false, "{}", true).await;
        h.task
            .runtime
            .automatic
            .lock()
            .expect("lock")
            .seasons
            .insert(SEASON);
        h.task.run_automatic().await;
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    }
}
