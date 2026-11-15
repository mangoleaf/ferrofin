//! Scheduled-task manager trait — the DI seam for `ScheduledTasksController`.
//!
//! Port of the *read/run* slice of `MediaBrowser.Model.Tasks.ITaskManager`
//! (plus the `ScheduledTaskHelpers.GetTaskInfo` projection the controller calls
//! per task). A handler holds an `Arc<dyn TaskManager>` in
//! [`AppState`](../../ferrofin_api/state) and never names the concrete
//! `ferrofin-core` registry.
//!
//! The trait covers the whole controller: enumerate tasks (filtered as
//! `GetTasks` filters), fetch one, run one, cancel a running one
//! (`ITaskManager.Cancel` → `DELETE /ScheduledTasks/Running/{taskId}`) and
//! replace a task's triggers (`IConfigurableScheduledTask.Triggers = …` →
//! `POST /ScheduledTasks/{taskId}/Triggers`). Both of those are implemented, not
//! stubs, and the `ITaskTrigger` timers behind them run in the `ferrofin-core`
//! registry's own scheduler.
//!
//! Every `taskId` on this trait is the wire id — `ScheduledTaskWorker.Id`,
//! reproduced by [`task_id_for_key`] — not the task key, matching the C#
//! controller's `FirstOrDefault(i => string.Equals(i.Id, taskId, …))` lookup.
//!
//! Reuses the `ferrofin-model` wire DTO [`TaskInfo`] verbatim.
//!
//! It also holds the task contract itself, [`ScheduledTask`] and the
//! [`TaskProgress`] each run gets, which core tasks, compiled-in extensions
//! and WASM plugins all implement, so none of them needs `ferrofin-core`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use ferrofin_model::tasks::{TaskInfo, TaskTriggerInfo};

use crate::error::ServiceError;

/// An initial task snapshot and all transitions captured after that snapshot.
/// Construct both under the registry lock so subscribing cannot lose a fast run.
pub struct TaskSubscription {
    /// State at the instant this subscription was established.
    pub snapshot: Vec<TaskInfo>,
    /// Subsequent lifecycle transitions in registry order.
    pub updates: tokio::sync::broadcast::Receiver<Vec<TaskInfo>>,
    /// One `()` each time a running task reported progress (upstream's
    /// `IScheduledTaskWorker.TaskProgress` event); the progress itself is read
    /// through [`TaskManager::get_tasks`].
    pub progress: tokio::sync::broadcast::Receiver<()>,
}

/// Enumerates the server's scheduled tasks and runs one on demand.
///
/// Port of `ITaskManager` (read + manual-run slice). Object-safe so it can be
/// held as `Arc<dyn TaskManager>`.
#[async_trait]
pub trait TaskManager: Send + Sync {
    /// Subscribes to captured task-start and terminal snapshots in transition
    /// order, and to the running tasks' progress reports. Implementations
    /// without live transitions return `None`: the `ScheduledTasksInfo`
    /// stream then sends only its subscribe snapshot, read from `get_tasks`.
    fn subscribe_task_updates(&self) -> Option<TaskSubscription> {
        None
    }

    /// Re-resolves the names, descriptions and categories of a captured
    /// transition snapshot in the ambient UI culture, and re-orders it by name.
    ///
    /// A transition snapshot is built in the culture of the run that
    /// published it and shared by every subscriber, where upstream builds each
    /// connection's payload in that connection's own culture
    /// (`BasePeriodicWebSocketListener` calls `ApplyRequestCulture` before
    /// `GetDataToSend`). The default returns the snapshot unchanged.
    fn localize_tasks(&self, tasks: Vec<TaskInfo>) -> Vec<TaskInfo> {
        tasks
    }

    /// Lists every registered task as a wire [`TaskInfo`].
    ///
    /// The unfiltered listing, in the C# wire order
    /// (`_taskManager.ScheduledTasks.OrderBy(o => o.Name)`). The query filters
    /// live on [`get_tasks_filtered`](TaskManager::get_tasks_filtered).
    async fn get_tasks(&self) -> Result<Vec<TaskInfo>, ServiceError>;

    /// Lists the tasks the `isHidden`/`isEnabled` query filters keep.
    ///
    /// Ports the loop in `ScheduledTasksController.GetTasks`, where the two
    /// filters are applied **only** inside
    /// `if (task.ScheduledTask is IConfigurableScheduledTask scheduledTask)` — a
    /// task that does not implement the interface is yielded whatever the caller
    /// asked for. Configurability is a property of the concrete task, so it is
    /// resolved here rather than by the handler.
    ///
    /// The default implementation is the answer for a registry whose tasks are
    /// all non-configurable: both filters are skipped and every task is listed.
    ///
    /// # Errors
    ///
    /// Whatever [`get_tasks`](TaskManager::get_tasks) returns.
    async fn get_tasks_filtered(
        &self,
        _is_hidden: Option<bool>,
        _is_enabled: Option<bool>,
    ) -> Result<Vec<TaskInfo>, ServiceError> {
        self.get_tasks().await
    }

    /// Fetches a single task by its id (the C# `IScheduledTaskWorker.Id`), or
    /// `None` when no task has that id.
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskInfo>, ServiceError>;

    /// Queues the named task to run now, recording the outcome when it ends.
    ///
    /// Ports `ScheduledTasksController.StartTask` → `ITaskManager.Execute`: the
    /// call returns as soon as the task is `Running`, and the dashboard tracks
    /// it from there.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id;
    /// [`ServiceError::InvalidInput`] when it is already running; or whatever
    /// error the task body itself returns.
    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError>;

    /// Cancels a running task.
    ///
    /// Ports `ScheduledTasksController.StopTask` → `ITaskManager.Cancel`: a
    /// running task is asked to stop and shows `Cancelling` until its run
    /// returns and records its result; cancelling an idle task is a no-op. A
    /// missing task is [`ServiceError::NotFound`], otherwise it succeeds.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id.
    async fn cancel_task(&self, task_id: &str) -> Result<(), ServiceError>;

    /// Cancels the task if it runs and queues a new run, which starts once
    /// the cancelled one has ended: `ITaskManager.CancelIfRunningAndQueue`,
    /// what `POST /Library/Refresh` does to the library-scan task.
    ///
    /// The default cancels, then starts, for a registry whose cancel ends a
    /// run at once.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id, or whatever
    /// [`start_task`](TaskManager::start_task) returns.
    async fn cancel_if_running_and_queue(&self, task_id: &str) -> Result<(), ServiceError> {
        self.cancel_task(task_id).await?;
        self.start_task(task_id).await
    }

    /// Replaces a task's configured triggers.
    ///
    /// Ports `ScheduledTasksController.UpdateTask` → `task.Triggers = triggerInfos`.
    /// The stored triggers are surfaced in the task's [`TaskInfo`], persisted, and
    /// armed on the registry's scheduler, exactly as the C# assignment reloads
    /// the configurable task's trigger events.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id.
    async fn update_triggers(
        &self,
        task_id: &str,
        triggers: &[TaskTriggerInfo],
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_task_manager(_: &dyn TaskManager) {}

/// Stops a running scan cooperatively, as upstream's `CancellationToken`
/// does. The scan checks it between two items, around each stored-row
/// window it reads and plans, between its closing passes and inside the
/// long ones. It races it against every wait of the item in progress: the
/// ones before anything is written (its probe, its metadata providers) end
/// the item unwritten, and a cancel inside its artwork pass (image
/// providers, cover extraction) stops the fetches where they stand and
/// saves the item with the artwork already on disk. A cancel after the
/// item's row was saved leaves the row without its refresh stamp, so an item
/// is written whole, not at all, or as far as it got for the next scan to
/// redo; and a hung ffprobe or provider cannot keep a cancelled scan alive.
///
/// It is also each scheduled task run's token ([`TaskProgress::cancel_token`]).
#[derive(Debug, Clone)]
pub struct ScanCancel(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for ScanCancel {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(false)))
    }
}

impl ScanCancel {
    /// A token nobody has cancelled yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the scan to stop.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether the scan was asked to stop.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the scan is asked to stop.
    pub async fn cancelled(&self) {
        let mut asked = self.0.subscribe();
        // The sender lives as long as `self`, so this never errors.
        let _ = asked.wait_for(|cancelled| *cancelled).await;
    }

    /// Runs `work` unless the scan is asked to stop first; `None` when it
    /// was (and `work` is dropped where it stood).
    pub async fn unless_cancelled<T>(
        &self,
        work: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        tokio::select! {
            biased;
            () = self.cancelled() => None,
            done = work => Some(done),
        }
    }
}

/// A live handle a running task uses to report its completion percentage.
///
/// Port of the C# `IProgress<double>` each `IScheduledTask.ExecuteAsync`
/// receives; the registry surfaces the last reported value as
/// [`TaskInfo::current_progress_percentage`] while the task is running, and
/// nothing before its first report (upstream's `CurrentProgress` stays null
/// until `ScheduledTaskWorker.OnProgressChanged` sets it).
#[derive(Clone, Debug, Default)]
pub struct TaskProgress {
    /// The current percentage, stored as `f64` bits.
    percent: Arc<AtomicU64>,
    /// Set by the first [`report`](Self::report).
    reported: Arc<AtomicBool>,
    /// Set by [`stopped`](Self::stopped).
    stopped: Arc<AtomicBool>,
    /// The run's cancellation token (upstream's `CurrentCancellationTokenSource`),
    /// cancelled by Stop, the run's max runtime and shutdown.
    cancel: ScanCancel,
    /// Why the run started (`api`, `schedule`, `startup`); empty for a
    /// test's foreground `FerrofinTaskManager::run_now`.
    trigger: &'static str,
    /// The registry's progress signal, raised by [`report`](Self::report)
    /// (upstream's `TaskProgress` event); `None` outside a registry run.
    progressed: Option<tokio::sync::broadcast::Sender<()>>,
}

impl TaskProgress {
    /// A fresh handle for one registry run, whose reports raise `progressed`
    /// (for the task registry; a task body only uses the handle it is given).
    #[must_use]
    pub fn for_run(progressed: tokio::sync::broadcast::Sender<()>) -> Self {
        Self {
            progressed: Some(progressed),
            ..Self::default()
        }
    }

    /// This handle, recording why its run started (see
    /// [`trigger`](Self::trigger)).
    #[must_use]
    pub fn with_trigger(self, trigger: &'static str) -> Self {
        Self { trigger, ..self }
    }

    /// Whether `other` is a clone of this handle, so both belong to the same
    /// run: the registry gives each run a fresh handle and checks it with this.
    #[must_use]
    pub fn is_same_run(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.stopped, &other.stopped)
    }

    /// Why this run started: the registry's trigger tag (`api` for the
    /// dashboard or `StartTask`, `schedule` for a configured trigger,
    /// `startup` for a startup trigger), or `""` when it has none (a test's
    /// foreground `FerrofinTaskManager::run_now`).
    #[must_use]
    pub fn trigger(&self) -> &'static str {
        self.trigger
    }

    /// Marks the run as stopped before its work finished, so it is recorded
    /// [`Cancelled`](ferrofin_model::tasks::TaskCompletionStatus::Cancelled) rather than
    /// `Completed` — upstream's `ExecuteAsync` throwing
    /// `OperationCanceledException` (`ScheduledTaskWorker.ExecuteInternal`).
    pub fn stopped(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Whether the run was marked [`stopped`](Self::stopped), which the
    /// registry records as `Cancelled`.
    #[must_use]
    pub fn was_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// The run's cancellation token, for the work a body hands it to (the
    /// chapter-image extractor, the subtitle downloader): upstream's
    /// `CancellationToken` argument of `ExecuteAsync`.
    #[must_use]
    pub fn cancel_token(&self) -> &ScanCancel {
        &self.cancel
    }

    /// Whether the run was asked to stop (Stop, its max runtime, shutdown).
    /// Once it says so the run counts as [`stopped`](Self::stopped), so a
    /// body that then returns `Ok` is recorded `Cancelled`: upstream's
    /// `cancellationToken.ThrowIfCancellationRequested()`.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        let requested = self.cancel.is_cancelled();
        if requested {
            self.stopped();
        }
        requested
    }

    /// Reports the task's completion percentage, capped at 100 with no lower
    /// bound, and raises the registry's progress signal, as upstream's
    /// `ScheduledTaskWorker.OnProgressChanged` caps it (`Math.Min(e, 100)`,
    /// which keeps a NaN) and raises `TaskProgress` on every report.
    pub fn report(&self, percent: f64) {
        // Not `f64::min`, which turns a NaN into 100.
        let capped = if percent > 100.0 { 100.0 } else { percent };
        self.percent.store(capped.to_bits(), Ordering::Relaxed);
        // Release: whoever sees the flag sees this report's percentage.
        self.reported.store(true, Ordering::Release);
        if let Some(progressed) = &self.progressed {
            // Nobody subscribed is not an error.
            let _ = progressed.send(());
        }
    }

    /// Reads the last reported percentage.
    #[must_use]
    pub fn current(&self) -> f64 {
        f64::from_bits(self.percent.load(Ordering::Relaxed))
    }

    /// The last reported percentage, or `None` before the first report.
    #[must_use]
    pub fn reported(&self) -> Option<f64> {
        self.reported
            .load(Ordering::Acquire)
            .then(|| self.current())
    }
}

/// A unit of background work the registry can enumerate, schedule and run.
///
/// Port of `MediaBrowser.Model.Tasks.IScheduledTask`. A task advertises stable
/// metadata, default triggers and an idempotent
/// [`execute`](ScheduledTask::execute) body; the registry — not the task — owns
/// run state, history and scheduling.
#[async_trait]
pub trait ScheduledTask: Send + Sync {
    /// A stable, unique key identifying the task (the C# `ScheduledTask.Key`).
    ///
    /// Used as the registry map key and echoed into [`TaskInfo::key`]/`id`.
    fn key(&self) -> &str;

    /// The task's human-readable display name, or its localization key when
    /// [`is_localized`](ScheduledTask::is_localized).
    fn name(&self) -> &str;

    /// A one-line description of what the task does, or its localization key
    /// when [`is_localized`](ScheduledTask::is_localized).
    fn description(&self) -> &str;

    /// The category the task is grouped under in the UI, or its localization
    /// key when [`is_localized`](ScheduledTask::is_localized).
    fn category(&self) -> &str;

    /// Whether [`name`](ScheduledTask::name),
    /// [`description`](ScheduledTask::description) and
    /// [`category`](ScheduledTask::category) are keys into the core
    /// localization dictionaries rather than display text.
    ///
    /// The registry then shows each in the ambient UI culture, as upstream's
    /// core tasks return `_localization.GetLocalizedString("<key>")`. Defaults
    /// to `false`: extension and WASM tasks show their own strings, as
    /// upstream plugins localize themselves, and so do the two core tasks
    /// upstream leaves as literals (`RefreshGuide`,
    /// `TmdbRefreshUpcomingEpisodes`).
    fn is_localized(&self) -> bool {
        false
    }

    /// The value formatted into a localized description's `{0}`, as
    /// upstream's `string.Format(GetLocalizedString(key), value)`. Only
    /// `CleanLogFiles` has one.
    fn description_argument(&self) -> Option<String> {
        None
    }

    /// Whether the task is hidden from the normal task listing.
    ///
    /// Defaults to `false`; hidden tasks are still runnable by key.
    fn is_hidden(&self) -> bool {
        false
    }

    /// Whether this task implements C# `IConfigurableScheduledTask`.
    ///
    /// Load-bearing for `GET /ScheduledTasks`: `ScheduledTasksController.GetTasks`
    /// applies the `isHidden`/`isEnabled` filters **only** inside
    /// `if (task.ScheduledTask is IConfigurableScheduledTask scheduledTask)`, so
    /// a task that does not implement the interface is yielded whatever the
    /// caller asked for. Exactly nine of Jellyfin 10.11.8's twenty tasks do
    /// (`CleanActivityLog`, `DeleteCacheFiles`, `CleanLogFiles`,
    /// `DeleteTranscodeFiles`, `OptimizeDatabaseTask`, `RefreshPeople`,
    /// `PluginUpdates`, `RefreshGuide`, `RefreshInternetChannels`), so this
    /// defaults to `false`.
    fn is_configurable(&self) -> bool {
        false
    }

    /// C# `IConfigurableScheduledTask.IsEnabled`.
    ///
    /// Only consulted when [`is_configurable`](ScheduledTask::is_configurable)
    /// is `true`, and only by the `isEnabled` query filter — `TaskInfo` has no
    /// `IsEnabled` field, so this never reaches the wire. Every configurable
    /// task in 10.11.8 returns `true`.
    fn is_enabled(&self) -> bool {
        true
    }

    /// The task's default triggers, fired by the scheduler unless overridden
    /// via [`TaskManager::update_triggers`].
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        Vec::new()
    }

    /// Runs the task to completion, reporting progress through `progress`.
    ///
    /// Errors surface as a [`Failed`](ferrofin_model::tasks::TaskCompletionStatus::Failed) result recorded by
    /// the registry; the returned `Result` also propagates to the caller of
    /// the test-only `FerrofinTaskManager::run_now`. A panic in the body is recorded the
    /// same way, as a failure whose error names the panic's message (upstream
    /// catches every exception the body throws as `Failed`).
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError>;
}

fn _assert_object_safe_scheduled_task(_: &dyn ScheduledTask) {}

// ---------------------------------------------------------------------------
// Task id derivation (`ScheduledTaskWorker.Id`)
// ---------------------------------------------------------------------------

/// The C# type `FullName` behind each task key Ferrofin registers.
///
/// This table is the input to [`task_id_for_key`], which reproduces
/// `ScheduledTaskWorker.Id` — `ScheduledTask.GetType().FullName.GetMD5()`
/// (`Emby.Server.Implementations/ScheduledTasks/ScheduledTaskWorker.cs:219` at
/// `v10.11.8`). Ferrofin has no .NET type to reflect over, so the type name a
/// task *would* have upstream is recorded here instead; the id is otherwise
/// derived exactly as upstream derives it, which is what makes a task id stable
/// across the two servers.
///
/// The first nineteen rows are Jellyfin 10.11.8's own task classes (namespace +
/// class name read from the `v10.11.8` tag), less
/// `CleanupCollectionAndPlaylistPathsTask`, which Jellyfin 12 deleted and
/// Ferrofin no longer registers. The last four are the tasks of the two
/// third-party plugins Ferrofin ports as Tier-1a extensions, at the upstream
/// commits pinned in `docs/PLUGINS_UPSTREAM.md`, so a dashboard link made
/// against a Jellyfin install carrying those plugins keeps working.
///
/// A key that is not listed (a WASM plugin's task, a test task) has no upstream
/// type at all; [`task_id_for_key`] hashes a `Ferrofin.ScheduledTasks.`-prefixed
/// name for it, so its id has the same 32-hex shape and is equally stable,
/// without pretending to be an upstream id.
const CSHARP_TASK_TYPE_NAMES: &[(&str, &str)] = &[
    // Emby.Server.Implementations/ScheduledTasks/Tasks/*.cs
    (
        "AudioNormalization",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.AudioNormalizationTask",
    ),
    (
        "CleanActivityLog",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.CleanActivityLogTask",
    ),
    (
        "DeleteCacheFiles",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.DeleteCacheFileTask",
    ),
    (
        "CleanLogFiles",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.DeleteLogFileTask",
    ),
    (
        "DeleteTranscodeFiles",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.DeleteTranscodeFileTask",
    ),
    (
        "RefreshChapterImages",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.ChapterImagesTask",
    ),
    (
        "TaskExtractMediaSegments",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.MediaSegmentExtractionTask",
    ),
    (
        "OptimizeDatabaseTask",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.OptimizeDatabaseTask",
    ),
    (
        "RefreshPeople",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.PeopleValidationTask",
    ),
    (
        "RefreshLibrary",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.RefreshMediaLibraryTask",
    ),
    (
        "PluginUpdates",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.PluginUpdateTask",
    ),
    (
        "CleanupUserDataTask",
        "Emby.Server.Implementations.ScheduledTasks.Tasks.CleanupUserDataTask",
    ),
    // MediaBrowser.Providers/**
    (
        "DownloadLyrics",
        "MediaBrowser.Providers.Lyric.LyricScheduledTask",
    ),
    (
        "DownloadSubtitles",
        "MediaBrowser.Providers.MediaInfo.SubtitleScheduledTask",
    ),
    (
        "RefreshTrickplayImages",
        "MediaBrowser.Providers.Trickplay.TrickplayImagesTask",
    ),
    (
        "MoveTrickplayImages",
        "MediaBrowser.Providers.Trickplay.TrickplayMoveImagesTask",
    ),
    (
        "TmdbRefreshUpcomingEpisodes",
        "MediaBrowser.Providers.Plugins.Tmdb.TV.TmdbUpcomingEpisodesTask",
    ),
    // src/Jellyfin.*/**
    (
        "KeyframeExtraction",
        "Jellyfin.MediaEncoding.Hls.ScheduledTasks.KeyframeExtractionScheduledTask",
    ),
    (
        "RefreshGuide",
        "Jellyfin.LiveTv.Guide.RefreshGuideScheduledTask",
    ),
    (
        "RefreshInternetChannels",
        "Jellyfin.LiveTv.Channels.RefreshChannelsScheduledTask",
    ),
    // Tier-1a extensions: the upstream plugins' own task classes.
    // intro-skipper @ db09359 — IntroSkipper/ScheduledTasks/DetectSegmentsTask.cs.
    (
        "IntroSkipperDetectSegmentsTask",
        "IntroSkipper.ScheduledTasks.DetectSegmentsTask",
    ),
    // IntroSkipper/ScheduledTasks/CleanCacheTask.cs (upstream's own key).
    (
        "CPBIntroSkipperCleanCache",
        "IntroSkipper.ScheduledTasks.CleanCacheTask",
    ),
    // jellyfin-plugin-mergeversions @ e6f58d6 —
    // Jellyfin.Plugin.MergeVersions/ScheduledTasks/RefreshLibraryTask.cs.
    (
        "MergeMoviesTask",
        "Jellyfin.Plugin.MergeVersions.ScheduledTasks.MergeMoviesTask",
    ),
    (
        "MergeEpisodesTask",
        "Jellyfin.Plugin.MergeVersions.ScheduledTasks.MergeEpisodesTask",
    ),
    // jellyfin-plugin-ldapauth @ d11c86a — LDAP-Auth/LdapProfileImageSyncTask.cs.
    (
        "LdapProfileImageSync",
        "Jellyfin.Plugin.LDAP_Auth.LdapProfileImageSyncTask",
    ),
];

/// The wire id of the task registered under `key` — the port of
/// `IScheduledTaskWorker.Id`.
///
/// Upstream is `ScheduledTask.GetType().FullName.GetMD5().ToString("N")`:
/// MD5 over the UTF-16LE bytes of the .NET type name, wrapped in a `Guid` and
/// rendered as 32 lowercase hex digits. It is a *portable* value — two Jellyfin
/// installs agree on it — so it is what a stored dashboard link, a bookmark and
/// `GET|POST|DELETE /ScheduledTasks/{taskId}` all address. Emitting the task key
/// there instead made Ferrofin's ids mutually incompatible with Jellyfin's.
///
/// `TaskInfo.Key` still carries the key, exactly as
/// `ScheduledTaskHelpers.GetTaskInfo` sets both fields.
#[must_use]
pub fn task_id_for_key(key: &str) -> String {
    let type_name = CSHARP_TASK_TYPE_NAMES
        .iter()
        .find(|(k, _)| *k == key)
        .map_or_else(
            || format!("Ferrofin.ScheduledTasks.{key}"),
            |(_, name)| (*name).to_owned(),
        );
    ferrofin_common::extensions::get_md5(&type_name)
        .simple()
        .to_string()
}

#[cfg(test)]
mod task_id_tests {
    use super::{CSHARP_TASK_TYPE_NAMES, task_id_for_key};

    /// The ids a real Jellyfin 10.11.8 server returns from `GET /ScheduledTasks`
    /// for its twenty built-in tasks, captured from the parity lab pair on
    /// 2026-08-30, less `CleanCollectionsAndPlaylists` (deleted in Jellyfin 12,
    /// not registered here). This is the oracle: the derivation is only correct
    /// if it reproduces these byte for byte.
    const JELLYFIN_10_11_8_TASK_IDS: &[(&str, &str)] = &[
        ("AudioNormalization", "ec2f221fd8e7706b3d3afd2c4591b4d7"),
        ("CleanActivityLog", "b461ef918ab28520928183e794350e3c"),
        ("DeleteCacheFiles", "241d4fcb19a1d557ee62428e411da609"),
        ("CleanLogFiles", "1c8ede62c521bea0bf851344f5b8ca40"),
        ("DeleteTranscodeFiles", "7d8088c10902f1bf4072ded42437bcfb"),
        ("DownloadLyrics", "26649fe0aad57557245351f220da916c"),
        ("DownloadSubtitles", "2c66a88bca43e565d7f8099f825478f1"),
        ("RefreshChapterImages", "4e6637c832ed644d1af3370a2506e80a"),
        ("RefreshTrickplayImages", "64f5f44cd30dc273cb9890205473bbcc"),
        ("KeyframeExtraction", "f302d80f31bcacf76f979d277d448581"),
        (
            "TaskExtractMediaSegments",
            "f861734dd71b37f9482b52a820e39013",
        ),
        ("MoveTrickplayImages", "ebb6e58c4e9a8f5d1f77ab6ddefb3143"),
        ("OptimizeDatabaseTask", "31de9ce83b9223d338c77b1a635e144b"),
        ("RefreshGuide", "bea9b218c97bbf98c5dc1303bdb9a0ca"),
        ("RefreshPeople", "866456ed0d44e15468124ce33d85961e"),
        ("RefreshLibrary", "7738148ffcd07979c7ceb148e06b3aed"),
        (
            "RefreshInternetChannels",
            "0c9ee3a88fc15547c6852205480da1fd",
        ),
        ("PluginUpdates", "f9b057c054e9e6daee4a88ffd146a403"),
        ("CleanupUserDataTask", "8240e309407dbbf43c99d30a6cbd239e"),
    ];

    #[test]
    fn every_jellyfin_task_id_is_reproduced_exactly() {
        for (key, expected) in JELLYFIN_10_11_8_TASK_IDS {
            assert_eq!(&task_id_for_key(key), expected, "task id for {key}");
        }
    }

    /// Jellyfin 12's TMDb task: the MD5 of its type's full name, as every
    /// task id above is.
    #[test]
    fn the_tmdb_upcoming_episodes_task_has_jellyfins_id() {
        assert_eq!(
            task_id_for_key("TmdbRefreshUpcomingEpisodes"),
            "1ee23a5dd39fb4225715375c91bc2990"
        );
    }

    /// LDAP-Auth's profile-image task: the MD5 of
    /// `Jellyfin.Plugin.LDAP_Auth.LdapProfileImageSyncTask` as a .NET `Guid`,
    /// computed outside the code under test.
    #[test]
    fn the_ldap_profile_image_task_has_the_plugins_id() {
        assert_eq!(
            task_id_for_key("LdapProfileImageSync"),
            "76cba34af0fabda24dffccae78f7aaf8"
        );
    }

    #[test]
    fn an_id_is_32_lowercase_hex_digits() {
        // C# `Guid.ToString("N")`: no dashes, no braces, lowercase.
        for (key, _) in CSHARP_TASK_TYPE_NAMES {
            let id = task_id_for_key(key);
            assert_eq!(id.len(), 32, "{key} -> {id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{key} -> {id}"
            );
        }
    }

    #[test]
    fn an_unmapped_key_gets_a_stable_ferrofin_namespaced_id() {
        // A WASM plugin's task has no upstream .NET type; it still gets a
        // guid-shaped id, and the same one every boot.
        let id = task_id_for_key("wasm:acme.hello:sweep");
        assert_eq!(id.len(), 32);
        assert_eq!(id, task_id_for_key("wasm:acme.hello:sweep"));
        assert_ne!(id, task_id_for_key("wasm:acme.hello:other"));
        // And it is NOT the key itself — the bug this derivation replaced.
        assert_ne!(id, "wasm:acme.hello:sweep");
    }

    #[test]
    fn the_type_name_table_has_no_duplicate_keys() {
        let mut keys: Vec<&str> = CSHARP_TASK_TYPE_NAMES.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before);
    }
}
