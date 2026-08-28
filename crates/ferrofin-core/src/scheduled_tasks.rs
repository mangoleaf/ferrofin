//! [`FerrofinTaskManager`] — the scheduled-task registry and trigger scheduler.
//!
//! Port of `Emby.Server.Implementations.ScheduledTasks.TaskManager` +
//! `ScheduledTaskWorker` + the `ITaskTrigger` implementations
//! (`DailyTrigger`/`WeeklyTrigger`/`IntervalTrigger`/`StartupTrigger`). The
//! registry enumerates tasks, runs one now (foreground or queued to a spawned
//! tokio task), cancels a queued run by aborting its tokio task, tracks live
//! progress, and — once [`FerrofinTaskManager::start_scheduler`] is called —
//! fires each task's configured triggers on its own. Trigger overrides set via
//! [`FerrofinTaskManager::set_triggers`] persist to a JSON file (the C# stores a
//! `<key>.js` per task; one file for the whole map is equivalent and simpler).
//!
//! Reused verbatim from `ferrofin-model`: [`TaskInfo`], [`TaskResult`],
//! [`TaskState`], [`TaskCompletionStatus`], [`TaskTriggerInfo`] — no new DTOs
//! are defined here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use ferrofin_model::dto::DayOfWeek;
use ferrofin_model::tasks::{
    TaskCompletionStatus, TaskInfo, TaskResult, TaskState, TaskTriggerInfo, TaskTriggerInfoType,
};

use ferrofin_traits::error::ServiceError;
use tracing::Instrument as _;

pub mod application;
pub mod channels;
pub mod library;
pub mod live_tv;
pub mod maintenance;

/// 100-nanosecond ticks per second (the `TaskTriggerInfo` time unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// How often the scheduler evaluates triggers.
const SCHEDULER_PERIOD: Duration = Duration::from_secs(30);

/// An interval trigger firing every `hours` hours (the shape most upstream
/// tasks declare).
pub(crate) fn interval_hours(hours: i64) -> TaskTriggerInfo {
    TaskTriggerInfo {
        type_: TaskTriggerInfoType::IntervalTrigger,
        interval_ticks: Some(hours * 3600 * TICKS_PER_SECOND),
        ..TaskTriggerInfo::default()
    }
}

/// How long a `StartupTrigger` waits before firing — upstream's
/// `StartupTrigger.DelayMs` (3000), ported verbatim: the grace period keeps a
/// task's first run off the boot critical path.
const STARTUP_TRIGGER_DELAY: Duration = Duration::from_secs(3);

/// How long after startup an interval task that has **never completed** first
/// becomes due — upstream's `IntervalTrigger.Start` arms `now.AddHours(1)` when
/// `lastResult is null`, rather than waiting a whole interval.
const FIRST_RUN_DELAY: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// How far past [`FIRST_RUN_DELAY`] the never-run tasks are spread.
///
/// On a fresh install — and on the first boot of an **adopted Jellyfin
/// database**, which has no Ferrofin task-result file — every interval task
/// takes the never-run branch at once. That is thirteen of them, including the
/// library scan, media-segment scan, audio normalization and both
/// merge-versions passes, and `queue_with_trigger` spawns each without any
/// global serialisation. Upstream never sees this because its timers are armed
/// per task from a running process; here they all become due on one sweep.
const FIRST_RUN_SPREAD_MINUTES: i64 = 55;

/// When a never-run interval task first becomes due, offset deterministically
/// by task key so the whole set does not start together.
///
/// Deterministic rather than random: a restart must not re-roll the offset, or
/// a task whose slot keeps moving could be starved by repeated reboots.
fn first_run_delay(task_key: &str) -> chrono::TimeDelta {
    let hash = task_key.bytes().fold(0u64, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u64::from(b))
    });
    let offset =
        i64::try_from(hash % u64::try_from(FIRST_RUN_SPREAD_MINUTES).unwrap_or(1)).unwrap_or(0);
    FIRST_RUN_DELAY + chrono::TimeDelta::minutes(offset)
}

/// A live handle a running task uses to report its completion percentage.
///
/// Port of the C# `IProgress<double>` each `IScheduledTask.ExecuteAsync`
/// receives; the registry surfaces the reported value as
/// [`TaskInfo::current_progress_percentage`] while the task is running.
#[derive(Clone, Debug, Default)]
pub struct TaskProgress {
    /// The current percentage, stored as `f64` bits.
    percent: Arc<AtomicU64>,
}

impl TaskProgress {
    /// Reports the task's completion percentage (clamped to `0.0..=100.0`).
    pub fn report(&self, percent: f64) {
        self.percent
            .store(percent.clamp(0.0, 100.0).to_bits(), Ordering::Relaxed);
    }

    /// Reads the last reported percentage.
    #[must_use]
    pub fn current(&self) -> f64 {
        f64::from_bits(self.percent.load(Ordering::Relaxed))
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

    /// The task's human-readable display name.
    fn name(&self) -> &str;

    /// A one-line description of what the task does.
    fn description(&self) -> &str;

    /// The category the task is grouped under in the UI.
    fn category(&self) -> &str;

    /// Whether the task is hidden from the normal task listing.
    ///
    /// Defaults to `false`; hidden tasks are still runnable by key.
    fn is_hidden(&self) -> bool {
        false
    }

    /// The task's default triggers, fired by the scheduler unless overridden
    /// via [`FerrofinTaskManager::set_triggers`].
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        Vec::new()
    }

    /// Runs the task to completion, reporting progress through `progress`.
    ///
    /// Errors surface as a [`TaskCompletionStatus::Failed`] result recorded by
    /// the registry; the returned `Result` also propagates to the caller of
    /// [`FerrofinTaskManager::run_now`].
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError>;
}

/// One registered task plus the registry-owned run state and last result.
struct Registration {
    task: Arc<dyn ScheduledTask>,
    state: TaskState,
    last_result: Option<TaskResult>,
    /// Triggers set via [`FerrofinTaskManager::set_triggers`], overriding the
    /// task's [`default_triggers`](ScheduledTask::default_triggers). `None`
    /// until a client configures them.
    triggers_override: Option<Vec<TaskTriggerInfo>>,
    /// The running task's live progress handle (meaningful while `Running`).
    progress: TaskProgress,
    /// Abort handle for a queued (spawned) run, so `cancel` can stop it.
    abort: Option<tokio::task::AbortHandle>,
    /// When the current run was claimed (meaningful while `Running`).
    started_at: Option<DateTime<Utc>>,
    /// Per-trigger-index last fire time, so a daily/weekly trigger fires once
    /// per scheduled occurrence and an interval trigger keeps its cadence.
    trigger_fires: HashMap<usize, DateTime<Utc>>,
    /// When this task's trigger set was last reconfigured via
    /// [`FerrofinTaskManager::set_triggers`], `None` while it still runs its
    /// defaults.
    ///
    /// Load-bearing for daily/weekly triggers: `set_triggers` clears
    /// [`trigger_fires`](Self::trigger_fires), so without this the scheduler
    /// forgets that today's occurrence already ran and re-fires it on the next
    /// sweep — saving the schedule of "Extract Chapter Images" at 20:00 kicked
    /// off its 4-hour ffmpeg pass 30 seconds later. Upstream's
    /// `DailyTrigger.Start` always arms the *next* occurrence, never a past
    /// one; this timestamp is that floor. Interval triggers deliberately keep
    /// using the scheduler start (upstream's `IntervalTrigger.Start` does
    /// re-arm a due interval shortly after a reload).
    triggers_since: Option<DateTime<Utc>>,
    /// Whether this key's `StartupTrigger` has already fired in this process —
    /// set under the `tasks` lock by whichever path fires it (the scheduler's
    /// startup sweep or a late [`register`](FerrofinTaskManager::register)),
    /// and carried across a re-registration of the same key so a task cannot
    /// be started twice at boot.
    startup_fired: bool,
}

impl Registration {
    /// Whether this registration's effective triggers include a
    /// `StartupTrigger`.
    fn has_startup_trigger(&self) -> bool {
        self.effective_triggers()
            .iter()
            .any(|t| t.type_ == TaskTriggerInfoType::StartupTrigger)
    }

    /// The triggers the scheduler acts on: the configured override, else the
    /// task's defaults.
    fn effective_triggers(&self) -> Vec<TaskTriggerInfo> {
        self.triggers_override
            .clone()
            .unwrap_or_else(|| self.task.default_triggers())
    }

    /// Projects the registration onto the `ferrofin-model` wire DTO.
    fn to_info(&self) -> TaskInfo {
        let key = self.task.key().to_string();
        TaskInfo {
            name: Some(self.task.name().to_string()),
            state: self.state,
            current_progress_percentage: (self.state == TaskState::Running)
                .then(|| self.progress.current()),
            id: Some(key.clone()),
            last_execution_result: self.last_result.clone(),
            triggers: self.effective_triggers(),
            description: Some(self.task.description().to_string()),
            category: Some(self.task.category().to_string()),
            is_hidden: self.task.is_hidden(),
            key: Some(key),
        }
    }
}

/// The scheduled-task registry and scheduler.
///
/// Register tasks up front, enumerate them as [`TaskInfo`], run one now by key,
/// and call [`start_scheduler`](Self::start_scheduler) once to have the
/// configured triggers fire on their own.
#[derive(Clone, Default)]
pub struct FerrofinTaskManager {
    // Keyed by `ScheduledTask::key`, mirroring the C# lookup-by-type. Guarded by
    // a std mutex — critical sections are trivial map edits, never held across an
    // `.await`; task execution happens on a clone taken *outside* the lock.
    tasks: Arc<Mutex<HashMap<String, Registration>>>,
    /// Trigger overrides loaded from / persisted to `store_path`, applied to a
    /// task when it registers.
    stored_overrides: Arc<Mutex<HashMap<String, Vec<TaskTriggerInfo>>>>,
    /// Where trigger overrides persist (`None` = in-memory only).
    store_path: Arc<Mutex<Option<PathBuf>>>,
    /// Last run outcomes loaded from / persisted to `result_store_path`,
    /// applied to a task when it registers so the dashboard's "Last ran"
    /// column survives a restart (upstream keeps one history file per task).
    stored_results: Arc<Mutex<HashMap<String, TaskResult>>>,
    /// Where run outcomes persist (`None` = in-memory only).
    result_store_path: Arc<Mutex<Option<PathBuf>>>,
    /// Whether [`start_scheduler`](Self::start_scheduler) has run. A task
    /// registered *after* that moment fires its own `StartupTrigger` on
    /// registration (upstream arms each task's triggers when its worker is
    /// constructed, so registration order cannot cost a task its startup run).
    ///
    /// Written and read **while holding the `tasks` lock**, so a task either
    /// lands in the scheduler's startup snapshot or fires its own trigger —
    /// never both, never neither.
    scheduler_started: Arc<AtomicBool>,
    /// Optional domain-event seam: every recorded run outcome is published as
    /// a `TaskCompleted` event (the composition root forwards it to admin
    /// sessions as the `ScheduledTaskEnded` WebSocket push the dashboard
    /// listens for). Interior-mutable so it can be wired after construction;
    /// clones share it.
    events: Arc<Mutex<Option<Arc<dyn ferrofin_traits::events::EventManager>>>>,
    /// Optional activity-log seam: a run that ends
    /// [`Failed`](TaskCompletionStatus::Failed) is recorded as a `TaskFailed`
    /// dashboard Alert (port of upstream's `TaskCompletedLogger`).
    activity: Arc<Mutex<Option<Arc<dyn ferrofin_traits::activity::ActivityManager>>>>,
}

impl std::fmt::Debug for FerrofinTaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = lock(&self.tasks).len();
        // ponytail: task count only; the registration map isn't Debug.
        f.debug_struct("FerrofinTaskManager")
            .field("task_count", &count)
            .finish_non_exhaustive()
    }
}

/// Locks a mutex, recovering the guard from a poisoned lock (a prior panic
/// while holding it) — the maps stay consistent either way.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serializes `value` to `path` **atomically** (temp file + rename), creating
/// the parent directory, and logs a warning on failure.
///
/// Persisting registry state is best-effort: a read-only config or data
/// directory must never fail a task run. The rename matters because one file
/// holds every task's state (upstream keeps one small file per task, so a torn
/// write there costs one task) — a half-written file would take the whole map
/// down with it.
///
/// Callers must hold the lock guarding `value` across this call, so concurrent
/// writers cannot lose each other's updates.
fn write_store<T: serde::Serialize>(path: &std::path::Path, value: &T, store: &str) {
    let bytes = match serde_json::to_vec_pretty(value) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(store, error = %e, "failed to serialize task store");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // One fixed temp name per store: writers are serialized by the caller's
    // lock, and the two stores live in different directories. A crash between
    // the write and the rename leaves a stale `.tmp` behind, overwritten by the
    // next save.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = write_synced(&tmp, &bytes) {
        tracing::warn!(store, path = %tmp.display(), error = %e, "failed to persist task store");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!(store, path = %path.display(), error = %e, "failed to persist task store");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Writes `bytes` to `path` and flushes them to the device, so the rename that
/// follows publishes a complete file rather than a possibly empty one.
fn write_synced(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Reads a JSON store written by [`write_store`], returning an empty map when
/// the file does not exist (a first boot) and warning — loudly, as upstream's
/// `Error deserializing {File}` does — when it exists but cannot be parsed, so
/// a corrupt store is never mistaken for "nothing has run yet".
fn read_store<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path, store: &str) -> T {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            tracing::warn!(store, path = %path.display(), error = %e, "failed to read task store");
            return T::default();
        }
    };
    if bytes.is_empty() {
        return T::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        tracing::warn!(
            store,
            path = %path.display(),
            error = %e,
            "failed to parse task store; starting empty"
        );
        T::default()
    })
}

impl FerrofinTaskManager {
    /// Creates an empty task registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Points the registry at its trigger-override store, loading any
    /// previously persisted overrides (applied as tasks register).
    ///
    /// Call before [`register`](Self::register). A missing file is treated as
    /// empty (first boot); an unparseable one is warned about and treated as
    /// empty, rather than silently resetting every configured schedule.
    pub fn set_trigger_store(&self, path: PathBuf) {
        *lock(&self.stored_overrides) = read_store(&path, "task triggers");
        *lock(&self.store_path) = Some(path);
    }

    /// Persists the current trigger overrides to the store, if one is set.
    ///
    /// The map's lock is held across the write so two concurrent savers cannot
    /// lose each other's update (or interleave two truncating writes into
    /// invalid JSON).
    fn persist_overrides(&self) {
        let Some(path) = lock(&self.store_path).clone() else {
            return;
        };
        let overrides = lock(&self.stored_overrides);
        write_store(&path, &*overrides, "task triggers");
    }

    /// Points the registry at its run-history store, loading any previously
    /// persisted last results (applied as tasks register).
    ///
    /// Call before [`register`](Self::register). A missing file is treated as
    /// empty (first boot); an unparseable one is warned about and treated as
    /// empty. Port of upstream's per-task history file
    /// (`{DataPath}/ScheduledTasks/{id}.js`, read back into
    /// `ScheduledTaskWorker.LastExecutionResult`); one file for the whole map
    /// is equivalent and simpler, matching the trigger store next to it.
    ///
    /// Accepted divergence for drop-in adoption: upstream names those files by
    /// the MD5 of the task *type*, so a data directory adopted from a real
    /// Jellyfin install starts with an empty "Last ran" column here — each
    /// task fills it in on its first Ferrofin run. Nothing is lost: Jellyfin's
    /// own files stay untouched for a swap back.
    pub fn set_result_store(&self, path: PathBuf) {
        *lock(&self.stored_results) = read_store(&path, "task history");
        *lock(&self.result_store_path) = Some(path);
    }

    /// Records a task's last run outcome in the history store and persists it,
    /// if one is set.
    ///
    /// Blocking serialize + write of a few KB, called once per finished run
    /// (upstream's write is synchronous too). The map's lock is held across
    /// the write — see [`persist_overrides`](Self::persist_overrides).
    fn persist_result(&self, key: &str, result: &TaskResult) {
        let Some(path) = lock(&self.result_store_path).clone() else {
            return;
        };
        let mut results = lock(&self.stored_results);
        results.insert(key.to_owned(), result.clone());
        write_store(&path, &*results, "task history");
    }

    /// Registers a task, replacing any existing task with the same key.
    ///
    /// Mirrors the C# `AddTasks`: the new registration starts
    /// [`Idle`](TaskState::Idle), picking up any persisted trigger override
    /// **and** last run result for its key. If the scheduler is already
    /// running and the task has a `StartupTrigger`, that trigger fires here —
    /// upstream arms a task's triggers when its worker is constructed, so a
    /// task registered late (after
    /// [`start_scheduler`](Self::start_scheduler)) must not silently lose its
    /// startup run.
    pub fn register(&self, task: Arc<dyn ScheduledTask>) {
        let key = task.key().to_string();
        let triggers_override = lock(&self.stored_overrides).get(&key).cloned();
        let last_result = lock(&self.stored_results).get(&key).cloned();
        let mut registration = Registration {
            task,
            state: TaskState::Idle,
            last_result,
            triggers_override,
            progress: TaskProgress::default(),
            abort: None,
            started_at: None,
            trigger_fires: HashMap::new(),
            triggers_since: None,
            startup_fired: false,
        };
        // Insert, and decide whether to fire, under the *same* `tasks` guard
        // `start_scheduler`'s startup snapshot takes — and record the decision
        // on the registration itself, so each key's startup trigger fires
        // exactly once per process even if the key is re-registered.
        let fire_startup = {
            let mut guard = lock(&self.tasks);
            registration.startup_fired = guard.get(&key).is_some_and(|r| r.startup_fired);
            let fire = self.scheduler_started.load(Ordering::Acquire)
                && registration.has_startup_trigger()
                && !registration.startup_fired;
            registration.startup_fired |= fire;
            guard.insert(key.clone(), registration);
            fire
        };
        if !fire_startup {
            return;
        }
        // Late registration: the scheduler's one startup sweep has already run,
        // so fire this task's startup trigger now — after upstream's grace
        // period (`StartupTrigger.Start` awaits `DelayMs` first).
        if tokio::runtime::Handle::try_current().is_err() {
            // No runtime to spawn the delayed run on (`register` is also called
            // from sync code, e.g. tests): nothing fires, and saying so keeps
            // "why didn't my task run at startup?" answerable.
            tracing::debug!(task = key, "no tokio runtime; startup trigger not fired");
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_TRIGGER_DELAY).await;
            match this.queue_with_trigger(&key, "startup") {
                Ok(()) => {}
                // Benign: something (an admin pressing Run) started the task
                // inside the grace window, so the startup run is redundant.
                Err(ServiceError::InvalidInput(e)) => {
                    tracing::debug!(task = key, reason = %e, "startup trigger skipped");
                }
                Err(e) => {
                    tracing::warn!(task = key, error = %e, "startup trigger failed to queue");
                }
            }
        });
    }

    /// Lists every registered task as a wire [`TaskInfo`] (hidden tasks included),
    /// ordered by key for a stable listing.
    #[must_use]
    pub fn list(&self) -> Vec<TaskInfo> {
        let guard = lock(&self.tasks);
        let mut infos: Vec<TaskInfo> = guard.values().map(Registration::to_info).collect();
        infos.sort_by(|a, b| a.key.cmp(&b.key));
        infos
    }

    /// Gets a single task's info by key, if registered.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<TaskInfo> {
        lock(&self.tasks).get(key).map(Registration::to_info)
    }

    /// Attaches the domain-event seam so run outcomes are published as
    /// `TaskCompleted` events (→ the `ScheduledTaskEnded` client push).
    pub fn set_event_manager(&self, events: Arc<dyn ferrofin_traits::events::EventManager>) {
        *lock(&self.events) = Some(events);
    }

    /// Attaches the activity-log seam so failed runs surface in the
    /// dashboard's activity feed.
    pub fn set_activity_manager(
        &self,
        activity: Arc<dyn ferrofin_traits::activity::ActivityManager>,
    ) {
        *lock(&self.activity) = Some(activity);
    }

    /// Writes the `TaskFailed` activity entry for a failed run (best-effort,
    /// spawned — recording never blocks on the write). Only `Failed` outcomes
    /// are logged, matching upstream's `TaskCompletedLogger`.
    fn log_failed_task(&self, result: &TaskResult) {
        if result.status != TaskCompletionStatus::Failed {
            return;
        }
        let Some(activity) = lock(&self.activity).clone() else {
            return;
        };
        let entry = ferrofin_traits::activity::ActivityLogCreate {
            name: format!("{} failed", result.name.clone().unwrap_or_default()),
            type_: "TaskFailed".to_owned(),
            severity: ferrofin_model::activity::LogLevel::Error,
            overview: result.error_message.clone(),
            ..Default::default()
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = activity.create_entry(entry).await;
            });
        }
    }

    /// Publishes a finished run's [`TaskResult`] as a `TaskCompleted` event
    /// (best-effort, on a spawned task — outcome recording never blocks on
    /// delivery). No-op without an event seam or outside a tokio runtime.
    fn publish_task_completed(&self, result: &TaskResult) {
        let Some(events) = lock(&self.events).clone() else {
            return;
        };
        let Ok(payload) = serde_json::to_string(result) else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = events.publish("TaskCompleted", &payload).await;
            });
        }
    }

    /// Records a run's outcome and returns the task to [`Idle`](TaskState::Idle).
    fn record_result(&self, key: &str, result: TaskResult) {
        self.publish_task_completed(&result);
        self.log_failed_task(&result);
        self.persist_result(key, &result);
        let mut guard = lock(&self.tasks);
        if let Some(reg) = guard.get_mut(key) {
            reg.state = TaskState::Idle;
            reg.last_result = Some(result);
            reg.abort = None;
            reg.started_at = None;
        }
    }

    /// Runs the named task now, to completion, recording the outcome.
    ///
    /// This is the foreground `Execute` path. A failing task body is recorded
    /// as a [`TaskCompletionStatus::Failed`] result *and* propagated to the
    /// caller.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key,
    /// [`ServiceError::InvalidInput`] if it is already running (mirroring the C#
    /// guard that only queues an `Idle` task), or whatever error the task body
    /// itself returns.
    pub async fn run_now(&self, key: &str) -> Result<(), ServiceError> {
        let (task, progress) = self.claim(key)?;
        let start = Utc::now();
        let outcome = task.execute(&progress).await;
        self.finish(key, &task, start, &outcome);
        outcome
    }

    /// Starts the named task in the background, returning as soon as it is
    /// [`Running`](TaskState::Running).
    ///
    /// Port of `TaskManager.QueueScheduledTask` (the `StartTask` HTTP path and
    /// the scheduler's trigger path): the caller gets an immediate answer while
    /// the task runs to completion on a spawned tokio task — so the dashboard
    /// sees the `Running` state (and live progress) for the task's real
    /// duration. Failures are recorded on the task's last result (they have no
    /// caller to propagate to).
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key, or
    /// [`ServiceError::InvalidInput`] if it is already running.
    pub fn queue(&self, key: &str) -> Result<(), ServiceError> {
        // The public entry is the API/StartTask path; internal callers
        // (startup / scheduler) use `queue_with_trigger` with their own tag.
        self.queue_with_trigger(key, "api")
    }

    /// [`queue`](Self::queue) with an explicit `trigger` tag recorded on the
    /// run's root span (`api` / `schedule` / `startup`).
    ///
    /// The run gets its **own** root span (never parented under a request — a
    /// background unit of work per RULES_LOGGING); `.instrument()` makes the
    /// spawned future carry it, since a span created before `spawn` does not
    /// follow the task by itself.
    fn queue_with_trigger(&self, key: &str, trigger: &'static str) -> Result<(), ServiceError> {
        let (task, progress) = self.claim(key)?;
        let this = self.clone();
        let key_owned = key.to_owned();
        let start = Utc::now();
        let span = tracing::info_span!(
            "scheduled_task",
            task = %key_owned,
            trigger,
            outcome = tracing::field::Empty,
        );
        let handle = tokio::spawn(
            async move {
                let started = std::time::Instant::now();
                tracing::info!("scheduled task started");
                let outcome = task.execute(&progress).await;
                let elapsed_ms = started.elapsed().as_millis();
                let result = if outcome.is_ok() { "completed" } else { "failed" };
                tracing::Span::current().record("outcome", result);
                match &outcome {
                    Ok(()) => tracing::info!(elapsed_ms, outcome = result, "scheduled task finished"),
                    // Logged exactly once, here, at the background task's top level.
                    Err(e) => {
                        tracing::error!(elapsed_ms, outcome = result, error = %e, "scheduled task failed");
                    }
                }
                this.finish(&key_owned, &task, start, &outcome);
            }
            .instrument(span),
        );
        let mut guard = lock(&self.tasks);
        if let Some(reg) = guard.get_mut(key) {
            // The spawned run may already have finished (state back to Idle);
            // only track the handle while it is still the live run.
            if reg.state == TaskState::Running {
                reg.abort = Some(handle.abort_handle());
            }
        }
        Ok(())
    }

    /// Validates that `key` is registered and idle, flips it to
    /// [`Running`](TaskState::Running), and returns its task handle plus a
    /// fresh progress handle.
    fn claim(&self, key: &str) -> Result<(Arc<dyn ScheduledTask>, TaskProgress), ServiceError> {
        let mut guard = lock(&self.tasks);
        let reg = guard
            .get_mut(key)
            .ok_or_else(|| ServiceError::not_found(format!("scheduled task {key}")))?;
        if reg.state == TaskState::Running {
            return Err(ServiceError::invalid_input(format!(
                "scheduled task {key} is already running"
            )));
        }
        reg.state = TaskState::Running;
        reg.progress = TaskProgress::default();
        reg.started_at = Some(Utc::now());
        Ok((Arc::clone(&reg.task), reg.progress.clone()))
    }

    /// Records a finished run's outcome and returns the task to
    /// [`Idle`](TaskState::Idle). `start` is when the run was claimed.
    fn finish(
        &self,
        key: &str,
        task: &Arc<dyn ScheduledTask>,
        start: chrono::DateTime<Utc>,
        outcome: &Result<(), ServiceError>,
    ) {
        let (status, error_message) = match outcome {
            Ok(()) => (TaskCompletionStatus::Completed, None),
            Err(e) => (TaskCompletionStatus::Failed, Some(e.to_string())),
        };
        self.record_result(
            key,
            TaskResult {
                start_time_utc: start,
                end_time_utc: Utc::now(),
                status,
                name: Some(task.name().to_string()),
                key: Some(key.to_string()),
                id: Some(key.to_string()),
                error_message,
                long_error_message: None,
            },
        );
    }

    /// Cancels a task: a queued (spawned) run is aborted and recorded as
    /// [`Cancelled`](TaskCompletionStatus::Cancelled); an idle task is left
    /// idle.
    ///
    /// The manual `Cancel`/`StopTask` path.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key.
    pub fn cancel(&self, key: &str) -> Result<(), ServiceError> {
        self.stop_run(key, TaskCompletionStatus::Cancelled)
    }

    /// Aborts a running task's spawned run, recording `status`, and returns it
    /// to [`Idle`](TaskState::Idle).
    fn stop_run(&self, key: &str, status: TaskCompletionStatus) -> Result<(), ServiceError> {
        let mut guard = lock(&self.tasks);
        let reg = guard
            .get_mut(key)
            .ok_or_else(|| ServiceError::not_found(format!("scheduled task {key}")))?;
        if reg.state != TaskState::Running {
            return Ok(());
        }
        if let Some(abort) = reg.abort.take() {
            abort.abort();
        }
        let start = reg.started_at.take().unwrap_or_else(Utc::now);
        reg.state = TaskState::Idle;
        let result = TaskResult {
            start_time_utc: start,
            end_time_utc: Utc::now(),
            status,
            name: Some(reg.task.name().to_string()),
            key: Some(key.to_string()),
            id: Some(key.to_string()),
            error_message: None,
            long_error_message: None,
        };
        reg.last_result = Some(result.clone());
        drop(guard);
        self.publish_task_completed(&result);
        self.persist_result(key, &result);
        Ok(())
    }

    /// Replaces a task's configured triggers, surfaced on its next
    /// [`TaskInfo`] read, acted on by the scheduler, and persisted to the
    /// trigger store (if one is configured).
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key.
    pub fn set_triggers(
        &self,
        key: &str,
        triggers: &[TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        {
            let mut guard = lock(&self.tasks);
            let reg = guard
                .get_mut(key)
                .ok_or_else(|| ServiceError::not_found(format!("scheduled task {key}")))?;
            reg.triggers_override = Some(triggers.to_vec());
            reg.trigger_fires.clear();
            // The new trigger set only ever arms *future* occurrences; without
            // this floor the cleared fire history makes an already-run daily
            // occurrence look due again on the next sweep.
            reg.triggers_since = Some(Utc::now());
        }
        lock(&self.stored_overrides).insert(key.to_owned(), triggers.to_vec());
        self.persist_overrides();
        Ok(())
    }

    /// Starts the background trigger scheduler, returning its join handle.
    ///
    /// Fires each registered task's `StartupTrigger`s after
    /// [`STARTUP_TRIGGER_DELAY`], then evaluates daily/weekly/interval triggers
    /// every [`SCHEDULER_PERIOD`] and aborts runs that exceed their trigger's
    /// `max_runtime_ticks`. Call once from the composition root; a task
    /// registered afterwards fires its own startup trigger from
    /// [`register`](Self::register).
    #[must_use = "dropping the handle is fine; aborting it stops the scheduler"]
    pub fn start_scheduler(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        // The snapshot and the flag are taken under one `tasks` guard, and
        // `register` inserts + reads the flag under the same one: every task
        // fires its startup trigger from exactly one of the two paths.
        let startup_keys = {
            let mut guard = lock(&self.tasks);
            let keys: Vec<String> = guard
                .iter()
                .filter(|(_, reg)| reg.has_startup_trigger() && !reg.startup_fired)
                .map(|(k, _)| k.clone())
                .collect();
            for key in &keys {
                if let Some(reg) = guard.get_mut(key) {
                    reg.startup_fired = true;
                }
            }
            self.scheduler_started.store(true, Ordering::Release);
            keys
        };
        tokio::spawn(async move {
            let scheduler_start = Utc::now();
            // Startup triggers fire once, after upstream's grace period.
            tokio::time::sleep(STARTUP_TRIGGER_DELAY).await;
            for key in startup_keys {
                if let Err(e) = this.queue_with_trigger(&key, "startup") {
                    tracing::warn!(task = key, error = %e, "startup trigger failed to queue");
                }
            }
            let mut tick = tokio::time::interval(SCHEDULER_PERIOD);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                this.scheduler_sweep(scheduler_start, Utc::now());
            }
        })
    }

    /// One scheduler pass: queue tasks whose triggers are due and abort runs
    /// past their max runtime.
    fn scheduler_sweep(&self, scheduler_start: DateTime<Utc>, now: DateTime<Utc>) {
        let mut due: Vec<String> = Vec::new();
        let mut overrun: Vec<String> = Vec::new();
        {
            let mut guard = lock(&self.tasks);
            for (key, reg) in guard.iter_mut() {
                let triggers = reg.effective_triggers();
                if reg.state == TaskState::Running {
                    if let (Some(started), Some(max_ticks)) = (
                        reg.started_at,
                        triggers.iter().filter_map(|t| t.max_runtime_ticks).min(),
                    ) && now - started >= ticks_to_chrono(max_ticks)
                    {
                        overrun.push(key.clone());
                    }
                    continue;
                }
                let last_run_end = reg.last_result.as_ref().map(|r| r.end_time_utc);
                let triggers_since = reg.triggers_since;
                for (idx, trigger) in triggers.iter().enumerate() {
                    let last_fire = reg.trigger_fires.get(&idx).copied();
                    if trigger_due(
                        trigger,
                        key,
                        now,
                        last_fire,
                        last_run_end,
                        scheduler_start,
                        triggers_since,
                    ) {
                        reg.trigger_fires.insert(idx, now);
                        due.push(key.clone());
                        break;
                    }
                }
            }
        }
        for key in overrun {
            tracing::warn!(task = key, "scheduled task exceeded max runtime; aborting");
            let _ = self.stop_run(&key, TaskCompletionStatus::Aborted);
        }
        for key in due {
            tracing::info!(task = key, "trigger fired; queueing scheduled task");
            if let Err(e) = self.queue_with_trigger(&key, "schedule") {
                tracing::warn!(task = key, error = %e, "trigger failed to queue task");
            }
        }
    }
}

/// Converts trigger ticks (100 ns) to a chrono duration.
fn ticks_to_chrono(ticks: i64) -> chrono::Duration {
    chrono::Duration::microseconds(ticks / 10)
}

/// Maps the wire [`DayOfWeek`] onto chrono's weekday.
fn weekday_of(day: DayOfWeek) -> chrono::Weekday {
    match day {
        DayOfWeek::Sunday => chrono::Weekday::Sun,
        DayOfWeek::Monday => chrono::Weekday::Mon,
        DayOfWeek::Tuesday => chrono::Weekday::Tue,
        DayOfWeek::Wednesday => chrono::Weekday::Wed,
        DayOfWeek::Thursday => chrono::Weekday::Thu,
        DayOfWeek::Friday => chrono::Weekday::Fri,
        DayOfWeek::Saturday => chrono::Weekday::Sat,
    }
}

/// Whether `trigger` is due at `now`.
///
/// Port of the `ITaskTrigger` semantics:
/// - `DailyTrigger`: due once per day at `time_of_day_ticks` (local time).
/// - `WeeklyTrigger`: as daily, but only on `day_of_week`.
/// - `IntervalTrigger`: due `interval_ticks` after the most recent of its last
///   fire or the task's last run end — **not** scheduler start, which would
///   restart the cadence on every boot. A task with no recorded result is due
///   [`first_run_delay`] after startup. See the arm itself: this is an accepted
///   divergence from upstream, which does re-anchor on boot.
/// - `StartupTrigger`: handled at scheduler start, never due in a sweep.
///
/// `triggers_since` is when the task's trigger set was last reconfigured (see
/// [`Registration::triggers_since`]); a daily/weekly occurrence older than that
/// belongs to the previous configuration and never fires, matching upstream's
/// `DailyTrigger.Start`, which always arms the next future occurrence. Interval
/// triggers ignore it, so saving a schedule whose new interval has already
/// elapsed since the last run fires the task on the next sweep — upstream's
/// `ReloadTriggerEvents(false)` re-arms `now + 1min + interval` instead.
fn trigger_due(
    trigger: &TaskTriggerInfo,
    task_key: &str,
    now: DateTime<Utc>,
    last_fire: Option<DateTime<Utc>>,
    last_run_end: Option<DateTime<Utc>>,
    scheduler_start: DateTime<Utc>,
    triggers_since: Option<DateTime<Utc>>,
) -> bool {
    match trigger.type_ {
        TaskTriggerInfoType::StartupTrigger => false,
        TaskTriggerInfoType::IntervalTrigger => {
            let Some(interval) = trigger.interval_ticks.filter(|t| *t > 0) else {
                return false;
            };
            // ACCEPTED DIVERGENCE — anchored on the last COMPLETION, where
            // upstream re-anchors on every process start.
            //
            // `IntervalTrigger.Start`:
            //
            //   if (lastResult is null) triggerDate = now.AddHours(1);
            //   else triggerDate = new[] { lastResult.EndTimeUtc, _lastStartDate,
            //                              now.AddMinutes(1) }.Max().Add(_interval);
            //
            // Read that `Max()` carefully: the two stored instants are always in
            // the past, so `now.AddMinutes(1)` ALWAYS wins and the else-branch
            // is unconditionally `now + 1min + interval`. `Start` runs at boot
            // (`InitTriggerEvents` → `ReloadTriggerEvents(true)`), so Jellyfin
            // restarts the cadence on every boot too — a 24-hour task on a
            // server rebooted every 12 hours never fires **in Jellyfin either**.
            //
            // Ferrofin does not reproduce that (`docs`: do not port Jellyfin
            // bugs). Anchoring on the persisted completion is what makes a
            // "daily" task actually daily. Three consequences, all deliberate:
            //
            //  1. an overdue task fires on the next sweep, where upstream would
            //     wait a further `interval + 1min`;
            //  2. there is no 1-minute floor, so the cadence is
            //     `run_end + interval` rather than `fire + 1min + interval` —
            //     a 6-hour scan repeats every 24h here, every ~30h upstream;
            //  3. `set_triggers` clears `trigger_fires` but not `last_run_end`,
            //     so saving a schedule whose new interval has already elapsed
            //     launches the task on the next sweep.
            //
            // A task that has never completed is due `FIRST_RUN_DELAY` after
            // startup — upstream's `AddHours(1)` — spread per task so a fresh
            // or freshly-adopted install does not start thirteen of them at
            // once. See `first_run_due_at`.
            let Some(base) = [last_fire, last_run_end].into_iter().flatten().max() else {
                return now - scheduler_start >= first_run_delay(task_key);
            };
            now - base >= ticks_to_chrono(interval)
        }
        TaskTriggerInfoType::DailyTrigger | TaskTriggerInfoType::WeeklyTrigger => {
            let Some(time_of_day) = trigger.time_of_day_ticks else {
                return false;
            };
            let local_now = now.with_timezone(&Local);
            if trigger.type_ == TaskTriggerInfoType::WeeklyTrigger {
                let Some(day) = trigger.day_of_week else {
                    return false;
                };
                if local_now.weekday() != weekday_of(day) {
                    return false;
                }
            }
            let seconds_of_day = time_of_day / TICKS_PER_SECOND;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (h, m, s) = (
                (seconds_of_day / 3600) as u32 % 24,
                ((seconds_of_day / 60) % 60) as u32,
                (seconds_of_day % 60) as u32,
            );
            let Some(scheduled_local) = local_now
                .date_naive()
                .and_hms_opt(h, m, s)
                .and_then(|naive| Local.from_local_datetime(&naive).earliest())
            else {
                return false;
            };
            let scheduled = scheduled_local.with_timezone(&Utc);
            // An occurrence that predates scheduler start — or the moment the
            // trigger set was last saved — never fires: booting at noon must
            // not immediately run every daily-at-3am task, and saving the
            // schedule at 20:00 must not re-run this morning's occurrence (the
            // C# DailyTrigger schedules the *next* occurrence in both cases).
            let floor = triggers_since.map_or(scheduler_start, |since| since.max(scheduler_start));
            now >= scheduled && scheduled >= floor && last_fire.is_none_or(|f| f < scheduled)
        }
    }
}

/// The "Scan Media Library" task — port of `RefreshMediaLibraryTask`.
///
/// Its [`execute`](ScheduledTask::execute) runs the same library scan as
/// `POST /Library/Refresh` (`LibraryManager::queue_library_scan`). The `Key` is
/// Jellyfin's well-known `"RefreshLibrary"`, so jellyfin-web's dashboard "Scan
/// all libraries" button (which starts the task by that key) drives it.
pub struct RefreshLibraryTask {
    library: Arc<dyn ferrofin_traits::library::LibraryManager>,
}

impl RefreshLibraryTask {
    /// Builds the task over the library-manager seam it scans through.
    #[must_use]
    pub fn new(library: Arc<dyn ferrofin_traits::library::LibraryManager>) -> Self {
        Self { library }
    }
}

// Metadata accessors return `&'self str` backed by string literals — fine here.
#[allow(clippy::unnecessary_literal_bound)]
#[async_trait]
impl ScheduledTask for RefreshLibraryTask {
    fn key(&self) -> &str {
        "RefreshLibrary"
    }
    fn name(&self) -> &str {
        "Scan Media Library"
    }
    fn description(&self) -> &str {
        "Scans your media library for new files and refreshes metadata."
    }
    fn category(&self) -> &str {
        "Library"
    }
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(12 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        }]
    }
    async fn execute(&self, _progress: &TaskProgress) -> Result<(), ServiceError> {
        // This runs inside a `scheduled_task` span already; tag the spawned scan
        // as schedule-triggered so its own root trace records the origin.
        self.library
            .queue_library_scan_with_trigger("schedule")
            .await
    }
}

/// Bridges the concrete registry onto the `ferrofin-traits` [`TaskManager`] seam
/// the API layer depends on.
///
/// Delegates to the inherent [`list`](FerrofinTaskManager::list) /
/// [`get`](FerrofinTaskManager::get) / [`queue`](FerrofinTaskManager::queue)
/// methods; the read side is infallible here (an in-memory registry), so the
/// `Result` wrappers always yield `Ok`.
#[async_trait]
impl ferrofin_traits::tasks::TaskManager for FerrofinTaskManager {
    async fn get_tasks(&self) -> Result<Vec<TaskInfo>, ServiceError> {
        Ok(self.list())
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskInfo>, ServiceError> {
        Ok(self.get(task_id))
    }

    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError> {
        // The HTTP path queues (C# QueueScheduledTask): the request returns as
        // soon as the task is Running; the dashboard tracks state from there.
        self.queue(task_id)
    }

    async fn cancel_task(&self, task_id: &str) -> Result<(), ServiceError> {
        self.cancel(task_id)
    }

    async fn update_triggers(
        &self,
        task_id: &str,
        triggers: &[TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        self.set_triggers(task_id, triggers)
    }
}

#[cfg(test)]
mod tests {
    use super::{FIRST_RUN_DELAY, first_run_delay};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use ferrofin_model::tasks::{
        TaskCompletionStatus, TaskState, TaskTriggerInfo, TaskTriggerInfoType,
    };

    use ferrofin_traits::error::ServiceError;

    use super::{FerrofinTaskManager, ScheduledTask, TICKS_PER_SECOND, TaskProgress, trigger_due};

    struct CountingTask {
        runs: Arc<AtomicU32>,
        fail: bool,
        hidden: bool,
    }

    // The metadata accessors return `&self`-lifetime `&str` per the trait, but
    // this test task backs them with literals; that's fine here.
    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait]
    impl ScheduledTask for CountingTask {
        fn key(&self) -> &str {
            "counting"
        }
        fn name(&self) -> &str {
            "Counting Task"
        }
        fn description(&self) -> &str {
            "counts its runs"
        }
        fn category(&self) -> &str {
            "Test"
        }
        fn is_hidden(&self) -> bool {
            self.hidden
        }
        async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
            progress.report(50.0);
            self.runs.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(ServiceError::backend("boom"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn register_list_and_run() {
        let mgr = FerrofinTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(CountingTask {
            runs: runs.clone(),
            fail: false,
            hidden: false,
        }));

        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key.as_deref(), Some("counting"));
        assert_eq!(list[0].state, TaskState::Idle);
        assert!(list[0].last_execution_result.is_none());
        assert!(list[0].current_progress_percentage.is_none());

        mgr.run_now("counting").await.expect("run");
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let info = mgr.get("counting").expect("info");
        assert_eq!(info.state, TaskState::Idle);
        let result = info.last_execution_result.expect("result");
        assert_eq!(result.status, TaskCompletionStatus::Completed);
        assert!(result.end_time_utc >= result.start_time_utc);
    }

    // A finished run must publish `TaskCompleted` (→ the dashboard's
    // `ScheduledTaskEnded` push) carrying the run's TaskResult.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_outcome_publishes_task_completed_event() {
        let mgr = FerrofinTaskManager::new();
        let events = crate::event_manager::FerrofinEventManager::new();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        events.subscribe(
            "TaskCompleted",
            Arc::new(move |payload: &str| {
                let _ = tx.send(payload.to_owned());
                Ok(())
            }),
        );
        mgr.set_event_manager(Arc::new(events));
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));

        mgr.run_now("counting").await.expect("run");
        // The publish is spawned; wait for it (multi-thread runtime keeps the
        // spawned task progressing while this thread blocks on the channel).
        let payload = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("TaskCompleted published");
        let result: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(result["Key"], "counting");
        assert_eq!(result["Status"], "Completed");
    }

    /// A failed run must land a `TaskFailed` entry in the activity log (the
    /// dashboard's Alerts feed); a successful run must not.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_run_writes_a_task_failed_activity_entry() {
        #[derive(Default)]
        struct RecordingActivity {
            entries: std::sync::Mutex<Vec<ferrofin_traits::activity::ActivityLogCreate>>,
        }
        #[async_trait::async_trait]
        impl ferrofin_traits::activity::ActivityManager for RecordingActivity {
            async fn get_paged_result(
                &self,
                _query: &ferrofin_traits::activity::ActivityLogQuery,
            ) -> Result<
                ferrofin_model::querying::QueryResult<ferrofin_model::activity::ActivityLogEntry>,
                ServiceError,
            > {
                unimplemented!()
            }
            async fn create_entry(
                &self,
                entry: ferrofin_traits::activity::ActivityLogCreate,
            ) -> Result<(), ServiceError> {
                crate::scheduled_tasks::lock(&self.entries).push(entry);
                Ok(())
            }
            async fn clean(&self, _before: chrono::DateTime<Utc>) -> Result<u64, ServiceError> {
                Ok(0)
            }
        }

        let mgr = FerrofinTaskManager::new();
        let activity = Arc::new(RecordingActivity::default());
        mgr.set_activity_manager(activity.clone());
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: true,
            hidden: false,
        }));

        let _ = mgr.run_now("counting").await;
        // The entry write is spawned; poll for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let entries = crate::scheduled_tasks::lock(&activity.entries);
                if !entries.is_empty() {
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].type_, "TaskFailed");
                    assert_eq!(entries[0].name, "Counting Task failed");
                    assert_eq!(
                        entries[0].severity,
                        ferrofin_model::activity::LogLevel::Error
                    );
                    assert!(
                        entries[0]
                            .overview
                            .as_deref()
                            .unwrap_or("")
                            .contains("boom")
                    );
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "TaskFailed entry never written"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A task that stays running until its gate is released.
    struct GatedTask {
        gate: Arc<tokio::sync::Notify>,
    }

    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait]
    impl ScheduledTask for GatedTask {
        fn key(&self) -> &str {
            "gated"
        }
        fn name(&self) -> &str {
            "Gated Task"
        }
        fn description(&self) -> &str {
            "waits for its gate"
        }
        fn category(&self) -> &str {
            "Test"
        }
        async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError> {
            progress.report(25.0);
            self.gate.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn queue_reports_running_progress_for_the_tasks_real_duration() {
        let mgr = FerrofinTaskManager::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        mgr.register(Arc::new(GatedTask { gate: gate.clone() }));

        // Queue returns immediately with the task now Running (the dashboard's
        // run button feedback), and a second start is rejected while it runs.
        mgr.queue("gated").expect("queued");
        let info = mgr.get("gated").expect("info");
        assert_eq!(info.state, TaskState::Running);
        assert!(mgr.queue("gated").is_err());

        // The spawned run reports progress, visible while Running.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let info = mgr.get("gated").expect("info");
            if info.current_progress_percentage == Some(25.0) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "progress never seen");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Release the gate; the spawned run finishes and records its result.
        gate.notify_one();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let info = mgr.get("gated").expect("info");
            if info.state == TaskState::Idle {
                let result = info.last_execution_result.expect("result");
                assert_eq!(result.status, TaskCompletionStatus::Completed);
                break;
            }
            assert!(std::time::Instant::now() < deadline, "task never finished");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queue_exports_a_scheduled_task_span_with_trigger_and_outcome() {
        // Span-coverage smoke: a queued run exports a `scheduled_task` root span
        // carrying `task`, `trigger`, and `outcome`. A current-thread runtime +
        // `set_default` keeps the spawned run on this thread/subscriber so the
        // `.instrument()`ed span is captured (guards against orphan-span bugs).
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_simple_exporter(exporter.clone())
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("ferrofin"));
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        let mgr = FerrofinTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(CountingTask {
            runs,
            fail: false,
            hidden: false,
        }));
        mgr.queue("counting").expect("queued");

        // Drive the spawned run to completion so its span closes and exports.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if mgr.get("counting").expect("info").state == TaskState::Idle {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "task never finished");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        provider.force_flush().expect("flush");

        let spans = exporter.get_finished_spans().expect("spans");
        let span = spans
            .iter()
            .find(|s| s.name == "scheduled_task")
            .expect("scheduled_task span exported");
        let attrs: std::collections::HashMap<String, String> = span
            .attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        assert_eq!(attrs.get("task").map(String::as_str), Some("counting"));
        assert_eq!(attrs.get("trigger").map(String::as_str), Some("api"));
        assert_eq!(attrs.get("outcome").map(String::as_str), Some("completed"));
    }

    #[tokio::test]
    async fn cancel_aborts_a_queued_run_and_records_cancelled() {
        let mgr = FerrofinTaskManager::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        mgr.register(Arc::new(GatedTask { gate }));

        mgr.queue("gated").expect("queued");
        assert_eq!(mgr.get("gated").expect("info").state, TaskState::Running);

        mgr.cancel("gated").expect("cancel");
        let info = mgr.get("gated").expect("info");
        assert_eq!(info.state, TaskState::Idle);
        let result = info.last_execution_result.expect("result");
        assert_eq!(result.status, TaskCompletionStatus::Cancelled);

        // The task can be started again after cancellation.
        mgr.queue("gated").expect("requeued");
        assert_eq!(mgr.get("gated").expect("info").state, TaskState::Running);
    }

    #[tokio::test]
    async fn failing_task_records_failed_and_propagates() {
        let mgr = FerrofinTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(CountingTask {
            runs,
            fail: true,
            hidden: false,
        }));

        let err = mgr.run_now("counting").await.expect_err("should fail");
        assert!(matches!(err, ServiceError::Backend(_)));

        let result = mgr
            .get("counting")
            .expect("info")
            .last_execution_result
            .expect("result");
        assert_eq!(result.status, TaskCompletionStatus::Failed);
        assert_eq!(result.error_message.as_deref(), Some("backend error: boom"));
    }

    #[tokio::test]
    async fn unknown_key_is_not_found() {
        let mgr = FerrofinTaskManager::new();
        assert!(matches!(
            mgr.run_now("nope").await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(mgr.get("nope").is_none());
        assert!(matches!(mgr.cancel("nope"), Err(ServiceError::NotFound(_))));
    }

    #[tokio::test]
    async fn register_replaces_same_key() {
        let mgr = FerrofinTaskManager::new();
        let first = Arc::new(AtomicU32::new(0));
        let second = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(CountingTask {
            runs: first.clone(),
            fail: false,
            hidden: false,
        }));
        mgr.register(Arc::new(CountingTask {
            runs: second.clone(),
            fail: false,
            hidden: true,
        }));
        assert_eq!(mgr.list().len(), 1);
        assert!(mgr.get("counting").expect("info").is_hidden);

        mgr.run_now("counting").await.expect("run");
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn set_triggers_overrides_and_persists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("triggers.json");

        let mgr = FerrofinTaskManager::new();
        mgr.set_trigger_store(store.clone());
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));

        // Defaults are empty (CountingTask uses the trait default).
        assert!(mgr.get("counting").expect("info").triggers.is_empty());

        let triggers = vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(42),
            ..TaskTriggerInfo::default()
        }];
        mgr.set_triggers("counting", &triggers).expect("set");

        let info = mgr.get("counting").expect("info");
        assert_eq!(info.triggers.len(), 1);
        assert_eq!(info.triggers[0].interval_ticks, Some(42));

        // A fresh manager pointed at the same store restores the override on
        // register (a restart survives).
        let mgr2 = FerrofinTaskManager::new();
        mgr2.set_trigger_store(store);
        mgr2.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));
        let info = mgr2.get("counting").expect("info");
        assert_eq!(info.triggers.len(), 1);
        assert_eq!(info.triggers[0].interval_ticks, Some(42));

        // A missing task is NotFound.
        assert!(matches!(
            mgr2.set_triggers("nope", &triggers),
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn last_execution_result_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("task_results.json");

        let mgr = FerrofinTaskManager::new();
        mgr.set_result_store(store.clone());
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));
        mgr.run_now("counting").await.expect("run");
        let first = mgr
            .get("counting")
            .expect("info")
            .last_execution_result
            .expect("result");

        // A fresh registry over the same store reports the previous run — the
        // dashboard's "Last ran" column is not wiped by a restart.
        let mgr2 = FerrofinTaskManager::new();
        mgr2.set_result_store(store);
        mgr2.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));
        let restored = mgr2
            .get("counting")
            .expect("info")
            .last_execution_result
            .expect("restored result");
        assert_eq!(restored.status, TaskCompletionStatus::Completed);
        assert_eq!(restored.key.as_deref(), Some("counting"));
        assert_eq!(restored.name.as_deref(), Some("Counting Task"));
        // Times round-trip through the wire format, whose precision is coarser
        // than chrono's nanoseconds.
        assert_eq!(
            (restored.start_time_utc - first.start_time_utc).num_milliseconds(),
            0
        );
        assert_eq!(
            (restored.end_time_utc - first.end_time_utc).num_milliseconds(),
            0
        );
        // …and the restored task is idle, not stuck mid-run.
        assert_eq!(mgr2.get("counting").expect("info").state, TaskState::Idle);
    }

    #[tokio::test]
    async fn a_task_registered_after_the_scheduler_still_runs_at_startup() {
        // Regression: the composition root registers a few tasks *after*
        // `start_scheduler` (they need managers built later). Their
        // `StartupTrigger` used to be dropped on the floor, so those tasks
        // never ran and reported no `LastExecutionResult` at all.
        let mgr = FerrofinTaskManager::new();
        let handle = mgr.start_scheduler();

        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(StartupTask { runs: runs.clone() }));
        // Re-registering the same key must not queue a second startup run: the
        // fire is recorded on the registration and carried across the replace.
        mgr.register(Arc::new(StartupTask { runs: runs.clone() }));

        wait_for_run(&runs, "late registration never fired its startup trigger").await;
        // Exactly once — the scheduler's own startup sweep must not queue it a
        // second time.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    /// A task with a `StartupTrigger` that counts its runs — the fixture both
    /// startup-path tests share.
    struct StartupTask {
        runs: Arc<AtomicU32>,
    }

    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait]
    impl ScheduledTask for StartupTask {
        fn key(&self) -> &str {
            "startup"
        }
        fn name(&self) -> &str {
            "Startup Task"
        }
        fn description(&self) -> &str {
            "runs at startup"
        }
        fn category(&self) -> &str {
            "Test"
        }
        fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
            vec![TaskTriggerInfo {
                type_: TaskTriggerInfoType::StartupTrigger,
                ..TaskTriggerInfo::default()
            }]
        }
        async fn execute(&self, _progress: &TaskProgress) -> Result<(), ServiceError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Waits for a counter to reach one run, allowing for the startup grace
    /// period (`STARTUP_TRIGGER_DELAY`) plus generous slack.
    async fn wait_for_run(runs: &Arc<AtomicU32>, message: &str) {
        let deadline = std::time::Instant::now()
            + super::STARTUP_TRIGGER_DELAY
            + std::time::Duration::from_secs(10);
        while runs.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "{message}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_corrupt_result_store_starts_empty_and_still_registers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("task_results.json");
        std::fs::write(&store, b"{not json").expect("write");

        let mgr = FerrofinTaskManager::new();
        mgr.set_result_store(store.clone());
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));
        let info = mgr.get("counting").expect("info");
        assert!(info.last_execution_result.is_none());

        // …and the next run overwrites the corrupt file with a good one.
        mgr.run_now("counting").await.expect("run");
        let reloaded = FerrofinTaskManager::new();
        reloaded.set_result_store(store);
        reloaded.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));
        assert!(
            reloaded
                .get("counting")
                .expect("info")
                .last_execution_result
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_failed_run_is_persisted_with_its_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("task_results.json");

        let mgr = FerrofinTaskManager::new();
        mgr.set_result_store(store.clone());
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: true,
            hidden: false,
        }));
        mgr.run_now("counting").await.expect_err("task fails");

        let mgr2 = FerrofinTaskManager::new();
        mgr2.set_result_store(store);
        mgr2.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: true,
            hidden: false,
        }));
        let restored = mgr2
            .get("counting")
            .expect("info")
            .last_execution_result
            .expect("restored result");
        assert_eq!(restored.status, TaskCompletionStatus::Failed);
        assert!(restored.error_message.is_some());
    }

    #[tokio::test]
    async fn a_cancelled_run_is_persisted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("task_results.json");

        let mgr = FerrofinTaskManager::new();
        mgr.set_result_store(store.clone());
        let gate = Arc::new(tokio::sync::Notify::new());
        mgr.register(Arc::new(GatedTask { gate }));
        mgr.queue("gated").expect("queued");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        mgr.cancel("gated").expect("cancel");

        let mgr2 = FerrofinTaskManager::new();
        mgr2.set_result_store(store);
        mgr2.register(Arc::new(GatedTask {
            gate: Arc::new(tokio::sync::Notify::new()),
        }));
        let restored = mgr2
            .get("gated")
            .expect("info")
            .last_execution_result
            .expect("restored result");
        assert_eq!(restored.status, TaskCompletionStatus::Cancelled);
    }

    /// A restart must not reset the interval clock.
    ///
    /// The cadence is anchored on the last COMPLETION, which is persisted, so a
    /// task that is overdue when the process starts fires immediately and one
    /// that ran recently does not fire early. Including the scheduler's own
    /// start time in that anchor meant a 24-hour task on a server rebooted
    /// every 12 hours never ran at all — and that is the bug the merge-versions
    /// tasks work around with a `StartupTrigger` they call a deliberate
    /// divergence.
    #[test]
    fn a_restart_does_not_restart_the_interval_cadence() {
        let boot = Utc
            .with_ymd_and_hms(2026, 7, 1, 12, 0, 0)
            .single()
            .expect("ts");
        let daily = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(24 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        };
        let now = boot + chrono::Duration::minutes(1);

        // Completed 25 hours ago: overdue, so it fires a minute after boot
        // rather than 24 hours later.
        let stale = boot - chrono::Duration::hours(25);
        assert!(
            trigger_due(&daily, "TestTask", now, None, Some(stale), boot, None),
            "an overdue task must fire despite the restart"
        );

        // Completed an hour ago: NOT due, however many times we reboot. The old
        // anchor made this fire 24h after boot, i.e. 25h after the last run.
        let recent = boot - chrono::Duration::hours(1);
        assert!(
            !trigger_due(&daily, "TestTask", now, None, Some(recent), boot, None),
            "a recently-run task must not fire early after a restart"
        );
        // The anchor is the run, not the boot: the run was an hour BEFORE boot,
        // so the next fire is 23h after boot — not 24h after it.
        assert!(
            !trigger_due(
                &daily,
                "TestTask",
                boot + chrono::Duration::hours(22),
                None,
                Some(recent),
                boot,
                None
            ),
            "23h since the run is not yet a day"
        );
        assert!(
            trigger_due(
                &daily,
                "TestTask",
                boot + chrono::Duration::hours(23),
                None,
                Some(recent),
                boot,
                None
            ),
            "24h since the run, which is 23h after boot"
        );
    }

    /// The never-run tasks are spread, not released together.
    ///
    /// A fresh install — and the first boot of an adopted Jellyfin database,
    /// which carries no Ferrofin task-result file — puts every interval task on
    /// the never-run branch at once. Thirteen of them, including the library
    /// scan and both merge passes, all spawned from one sweep.
    #[test]
    fn never_run_tasks_do_not_all_become_due_at_the_same_moment() {
        let keys = [
            "RefreshLibrary",
            "MediaSegmentScan",
            "AudioNormalization",
            "DownloadSubtitles",
            "RefreshPeople",
            "OptimizeDatabase",
            "MergeMoviesTask",
            "MergeEpisodesTask",
        ];
        let delays: Vec<_> = keys.iter().map(|k| first_run_delay(k)).collect();
        let distinct: std::collections::HashSet<_> = delays.iter().collect();
        assert!(
            distinct.len() > keys.len() / 2,
            "the spread must actually separate them: {delays:?}"
        );
        for d in &delays {
            assert!(
                *d >= FIRST_RUN_DELAY && *d < FIRST_RUN_DELAY + chrono::TimeDelta::hours(1),
                "within the hour after the base delay: {d:?}"
            );
        }
        // Deterministic: a restart must not re-roll a task's slot, or repeated
        // reboots could starve whichever task keeps landing late.
        assert_eq!(first_run_delay("RefreshLibrary"), delays[0]);
    }

    /// A task that has never completed is due about an hour after startup, not
    /// a whole interval later (upstream's `lastResult is null` branch).
    #[test]
    fn a_never_run_interval_task_is_due_an_hour_after_startup() {
        let boot = Utc
            .with_ymd_and_hms(2026, 7, 1, 12, 0, 0)
            .single()
            .expect("ts");
        let daily = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(24 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        };
        // Its own jittered slot, not a bare hour — see `first_run_delay`.
        let due_at = first_run_delay("TestTask");
        assert!(!trigger_due(
            &daily,
            "TestTask",
            boot + due_at - chrono::Duration::minutes(1),
            None,
            None,
            boot,
            None
        ));
        assert!(
            trigger_due(&daily, "TestTask", boot + due_at, None, None, boot, None),
            "about an hour, not a whole interval"
        );
        // And emphatically not a whole interval later.
        assert!(due_at < chrono::TimeDelta::hours(2), "{due_at:?}");
    }

    #[test]
    fn interval_trigger_due_after_interval_from_latest_reference() {
        let start = Utc
            .with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
            .single()
            .expect("ts");
        // TWO hours, deliberately not one: with a 1-hour interval these
        // assertions were indistinguishable from the never-run
        // `first_run_delay` branch, and passed whether or not the interval
        // logic worked at all.
        let trigger = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(2 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        };

        // A recent run end pushes the next fire out.
        let run_end = start + chrono::Duration::minutes(45);
        let now = start + chrono::Duration::hours(1);
        assert!(!trigger_due(
            &trigger,
            "TestTask",
            now,
            None,
            Some(run_end),
            start,
            None
        ));
        // Due exactly one interval after the run END — not after the boot.
        let now = run_end + chrono::Duration::hours(2);
        assert!(trigger_due(
            &trigger,
            "TestTask",
            now,
            None,
            Some(run_end),
            start,
            None
        ));

        // Missing/zero interval never fires.
        let no_interval = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            ..TaskTriggerInfo::default()
        };
        assert!(!trigger_due(
            &no_interval,
            "TestTask",
            now,
            None,
            None,
            start,
            None
        ));
    }

    #[test]
    fn daily_trigger_due_once_per_day_at_time_of_day() {
        use chrono::{Local, TimeZone as _};

        // Anchor "today at 03:00 local" from a fixed local date.
        let scheduled_local = Local
            .with_ymd_and_hms(2026, 7, 1, 3, 0, 0)
            .single()
            .expect("ts");
        let scheduled = scheduled_local.with_timezone(&Utc);
        let start = scheduled - chrono::Duration::hours(6);
        let trigger = TaskTriggerInfo {
            type_: TaskTriggerInfoType::DailyTrigger,
            time_of_day_ticks: Some(3 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        };

        // Before 03:00 local — not due.
        assert!(!trigger_due(
            &trigger,
            "TestTask",
            scheduled - chrono::Duration::minutes(5),
            None,
            None,
            start,
            None
        ));
        // After 03:00 local, never fired — due.
        let now = scheduled + chrono::Duration::minutes(5);
        assert!(trigger_due(
            &trigger, "TestTask", now, None, None, start, None
        ));
        // Already fired for this occurrence — not due again.
        assert!(!trigger_due(
            &trigger,
            "TestTask",
            now,
            Some(now),
            None,
            start,
            None
        ));
        // Next day, same wall time — due again.
        let tomorrow = now + chrono::Duration::days(1);
        assert!(trigger_due(
            &trigger,
            "TestTask",
            tomorrow,
            Some(now),
            None,
            start,
            None
        ));
        // An occurrence that predates scheduler start never fires: a boot at
        // noon must not immediately run a daily-at-3am task.
        let late_start = scheduled + chrono::Duration::hours(9);
        assert!(!trigger_due(
            &trigger,
            "TestTask",
            late_start + chrono::Duration::minutes(5),
            None,
            None,
            late_start,
            None
        ));
    }

    #[test]
    fn saving_triggers_does_not_re_fire_todays_daily_occurrence() {
        use chrono::{Local, TimeZone as _};

        // Daily at 02:00 local; the server booted yesterday, so scheduler start
        // predates today's occurrence and the task already ran at 02:00.
        let scheduled = Local
            .with_ymd_and_hms(2026, 7, 1, 2, 0, 0)
            .single()
            .expect("ts")
            .with_timezone(&Utc);
        let start = scheduled - chrono::Duration::days(1);
        let trigger = TaskTriggerInfo {
            type_: TaskTriggerInfoType::DailyTrigger,
            time_of_day_ticks: Some(2 * 3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        };
        // 20:00 the same evening — an admin saves this task's schedule, which
        // clears the fire history (`last_fire` is gone) and stamps
        // `triggers_since`.
        let saved_at = scheduled + chrono::Duration::hours(18);
        let now = saved_at + chrono::Duration::seconds(30);

        // Without the save floor the cleared history makes this morning's
        // occurrence look due again — the bug this guards.
        assert!(
            trigger_due(&trigger, "TestTask", now, None, None, start, None),
            "precondition: a cleared fire history alone makes the past occurrence due"
        );
        // With it, today's occurrence stays spent.
        assert!(!trigger_due(
            &trigger,
            "TestTask",
            now,
            None,
            None,
            start,
            Some(saved_at)
        ));
        // Tomorrow's occurrence still fires.
        let tomorrow = scheduled + chrono::Duration::days(1) + chrono::Duration::minutes(5);
        assert!(trigger_due(
            &trigger,
            "TestTask",
            tomorrow,
            None,
            None,
            start,
            Some(saved_at)
        ));
        // An interval trigger is unaffected by the save (upstream's
        // `IntervalTrigger.Start` re-arms a due interval after a reload).
        let interval = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(3600 * TICKS_PER_SECOND),
            ..TaskTriggerInfo::default()
        };
        assert!(trigger_due(
            &interval,
            "TestTask",
            now,
            None,
            None,
            start,
            Some(saved_at)
        ));
    }

    #[test]
    fn weekly_trigger_requires_matching_weekday() {
        use chrono::{Datelike as _, Local, TimeZone as _};
        use ferrofin_model::dto::DayOfWeek;

        // 2026-07-01 is a Wednesday.
        let scheduled_local = Local
            .with_ymd_and_hms(2026, 7, 1, 3, 0, 0)
            .single()
            .expect("ts");
        assert_eq!(scheduled_local.weekday(), chrono::Weekday::Wed);
        let scheduled = scheduled_local.with_timezone(&Utc);
        let start = scheduled - chrono::Duration::hours(6);
        let now = scheduled + chrono::Duration::minutes(5);

        let wednesday = TaskTriggerInfo {
            type_: TaskTriggerInfoType::WeeklyTrigger,
            time_of_day_ticks: Some(3 * 3600 * TICKS_PER_SECOND),
            day_of_week: Some(DayOfWeek::Wednesday),
            ..TaskTriggerInfo::default()
        };
        assert!(trigger_due(
            &wednesday, "TestTask", now, None, None, start, None
        ));

        let thursday = TaskTriggerInfo {
            day_of_week: Some(DayOfWeek::Thursday),
            ..wednesday
        };
        assert!(!trigger_due(
            &thursday, "TestTask", now, None, None, start, None
        ));

        // Weekly without a day never fires.
        let dayless = TaskTriggerInfo {
            day_of_week: None,
            ..wednesday
        };
        assert!(!trigger_due(
            &dayless, "TestTask", now, None, None, start, None
        ));
    }

    #[tokio::test]
    async fn scheduler_fires_startup_triggers_and_aborts_overruns() {
        let mgr = FerrofinTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(StartupTask { runs: runs.clone() }));

        let handle = mgr.start_scheduler();
        wait_for_run(&runs, "startup never fired").await;
        // Exactly once: registering before the scheduler must fire from the
        // startup snapshot only, never also from `register`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        handle.abort();

        // Overrun abort: a hung run whose trigger caps runtime is aborted by a
        // sweep once the cap has passed.
        let mgr = FerrofinTaskManager::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        mgr.register(Arc::new(GatedTask { gate }));
        let cap = vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(3600 * TICKS_PER_SECOND),
            max_runtime_ticks: Some(1), // 100 ns — instantly overrun
            ..TaskTriggerInfo::default()
        }];
        mgr.set_triggers("gated", &cap).expect("set");
        mgr.queue("gated").expect("queued");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let start = Utc::now() - chrono::Duration::hours(1);
        mgr.scheduler_sweep(start, Utc::now());
        let info = mgr.get("gated").expect("info");
        assert_eq!(info.state, TaskState::Idle);
        assert_eq!(
            info.last_execution_result.expect("result").status,
            TaskCompletionStatus::Aborted
        );
    }

    #[tokio::test]
    async fn trait_delegates() {
        use ferrofin_traits::tasks::TaskManager;

        let mgr = FerrofinTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(CountingTask {
            runs: runs.clone(),
            fail: false,
            hidden: false,
        }));

        // `get_tasks` mirrors the inherent list; `get_task` hits / misses.
        let tasks = TaskManager::get_tasks(&mgr).await.expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id.as_deref(), Some("counting"));
        assert!(
            TaskManager::get_task(&mgr, "counting")
                .await
                .expect("get")
                .is_some()
        );
        assert!(
            TaskManager::get_task(&mgr, "nope")
                .await
                .expect("get")
                .is_none()
        );

        // `start_task` queues it; poll for the completed result.
        TaskManager::start_task(&mgr, "counting")
            .await
            .expect("start");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            let info = TaskManager::get_task(&mgr, "counting")
                .await
                .expect("get")
                .expect("info");
            if let Some(result) = info.last_execution_result {
                break result;
            }
            assert!(std::time::Instant::now() < deadline, "task never finished");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(result.status, TaskCompletionStatus::Completed);

        // Cancel and trigger updates delegate; unknown keys are NotFound.
        TaskManager::cancel_task(&mgr, "counting")
            .await
            .expect("cancel");
        let triggers = vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::StartupTrigger,
            ..TaskTriggerInfo::default()
        }];
        TaskManager::update_triggers(&mgr, "counting", &triggers)
            .await
            .expect("update");
        assert_eq!(
            TaskManager::get_task(&mgr, "counting")
                .await
                .expect("get")
                .expect("info")
                .triggers
                .len(),
            1
        );
        assert!(matches!(
            TaskManager::start_task(&mgr, "nope").await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            TaskManager::cancel_task(&mgr, "nope").await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            TaskManager::update_triggers(&mgr, "nope", &triggers).await,
            Err(ServiceError::NotFound(_))
        ));
    }
}
