//! [`HermitTaskManager`] — the scheduled-task registry and trigger scheduler.
//!
//! Port of `Emby.Server.Implementations.ScheduledTasks.TaskManager` +
//! `ScheduledTaskWorker` + the `ITaskTrigger` implementations
//! (`DailyTrigger`/`WeeklyTrigger`/`IntervalTrigger`/`StartupTrigger`). The
//! registry enumerates tasks, runs one now (foreground or queued to a spawned
//! tokio task), cancels a queued run by aborting its tokio task, tracks live
//! progress, and — once [`HermitTaskManager::start_scheduler`] is called —
//! fires each task's configured triggers on its own. Trigger overrides set via
//! [`HermitTaskManager::set_triggers`] persist to a JSON file (the C# stores a
//! `<key>.js` per task; one file for the whole map is equivalent and simpler).
//!
//! Reused verbatim from `hermit-model`: [`TaskInfo`], [`TaskResult`],
//! [`TaskState`], [`TaskCompletionStatus`], [`TaskTriggerInfo`] — no new DTOs
//! are defined here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use hermit_model::dto::DayOfWeek;
use hermit_model::tasks::{
    TaskCompletionStatus, TaskInfo, TaskResult, TaskState, TaskTriggerInfo, TaskTriggerInfoType,
};

use hermit_traits::error::ServiceError;

pub mod library;
pub mod maintenance;

/// 100-nanosecond ticks per second (the `TaskTriggerInfo` time unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// How often the scheduler evaluates triggers.
const SCHEDULER_PERIOD: Duration = Duration::from_secs(30);

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
    /// via [`HermitTaskManager::set_triggers`].
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        Vec::new()
    }

    /// Runs the task to completion, reporting progress through `progress`.
    ///
    /// Errors surface as a [`TaskCompletionStatus::Failed`] result recorded by
    /// the registry; the returned `Result` also propagates to the caller of
    /// [`HermitTaskManager::run_now`].
    async fn execute(&self, progress: &TaskProgress) -> Result<(), ServiceError>;
}

/// One registered task plus the registry-owned run state and last result.
struct Registration {
    task: Arc<dyn ScheduledTask>,
    state: TaskState,
    last_result: Option<TaskResult>,
    /// Triggers set via [`HermitTaskManager::set_triggers`], overriding the
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
}

impl Registration {
    /// The triggers the scheduler acts on: the configured override, else the
    /// task's defaults.
    fn effective_triggers(&self) -> Vec<TaskTriggerInfo> {
        self.triggers_override
            .clone()
            .unwrap_or_else(|| self.task.default_triggers())
    }

    /// Projects the registration onto the `hermit-model` wire DTO.
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
pub struct HermitTaskManager {
    // Keyed by `ScheduledTask::key`, mirroring the C# lookup-by-type. Guarded by
    // a std mutex — critical sections are trivial map edits, never held across an
    // `.await`; task execution happens on a clone taken *outside* the lock.
    tasks: Arc<Mutex<HashMap<String, Registration>>>,
    /// Trigger overrides loaded from / persisted to `store_path`, applied to a
    /// task when it registers.
    stored_overrides: Arc<Mutex<HashMap<String, Vec<TaskTriggerInfo>>>>,
    /// Where trigger overrides persist (`None` = in-memory only).
    store_path: Arc<Mutex<Option<PathBuf>>>,
}

impl std::fmt::Debug for HermitTaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = lock(&self.tasks).len();
        // ponytail: task count only; the registration map isn't Debug.
        f.debug_struct("HermitTaskManager")
            .field("task_count", &count)
            .finish_non_exhaustive()
    }
}

/// Locks a mutex, recovering the guard from a poisoned lock (a prior panic
/// while holding it) — the maps stay consistent either way.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl HermitTaskManager {
    /// Creates an empty task registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Points the registry at its trigger-override store, loading any
    /// previously persisted overrides (applied as tasks register).
    ///
    /// Call before [`register`](Self::register). A missing or unreadable file
    /// is treated as empty (first boot).
    pub fn set_trigger_store(&self, path: PathBuf) {
        let loaded: HashMap<String, Vec<TaskTriggerInfo>> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        *lock(&self.stored_overrides) = loaded;
        *lock(&self.store_path) = Some(path);
    }

    /// Persists the current trigger overrides to the store, if one is set.
    fn persist_overrides(&self) {
        let Some(path) = lock(&self.store_path).clone() else {
            return;
        };
        let overrides = lock(&self.stored_overrides).clone();
        match serde_json::to_vec_pretty(&overrides) {
            Ok(bytes) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&path, bytes) {
                    tracing::warn!(path = %path.display(), error = %e, "failed to persist task triggers");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to serialize task triggers"),
        }
    }

    /// Registers a task, replacing any existing task with the same key.
    ///
    /// Mirrors the C# `AddTasks`; the new registration starts
    /// [`Idle`](TaskState::Idle) with no last result, picking up any persisted
    /// trigger override for its key.
    pub fn register(&self, task: Arc<dyn ScheduledTask>) {
        let key = task.key().to_string();
        let triggers_override = lock(&self.stored_overrides).get(&key).cloned();
        let registration = Registration {
            task,
            state: TaskState::Idle,
            last_result: None,
            triggers_override,
            progress: TaskProgress::default(),
            abort: None,
            started_at: None,
            trigger_fires: HashMap::new(),
        };
        lock(&self.tasks).insert(key, registration);
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

    /// Records a run's outcome and returns the task to [`Idle`](TaskState::Idle).
    fn record_result(&self, key: &str, result: TaskResult) {
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
        let (task, progress) = self.claim(key)?;
        let this = self.clone();
        let key_owned = key.to_owned();
        let start = Utc::now();
        let handle = tokio::spawn(async move {
            let outcome = task.execute(&progress).await;
            if let Err(e) = &outcome {
                tracing::warn!(task = key_owned, error = %e, "scheduled task failed");
            }
            this.finish(&key_owned, &task, start, &outcome);
        });
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
        reg.last_result = Some(TaskResult {
            start_time_utc: start,
            end_time_utc: Utc::now(),
            status,
            name: Some(reg.task.name().to_string()),
            key: Some(key.to_string()),
            id: Some(key.to_string()),
            error_message: None,
            long_error_message: None,
        });
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
        }
        lock(&self.stored_overrides).insert(key.to_owned(), triggers.to_vec());
        self.persist_overrides();
        Ok(())
    }

    /// Starts the background trigger scheduler, returning its join handle.
    ///
    /// Fires each registered task's `StartupTrigger`s immediately, then
    /// evaluates daily/weekly/interval triggers every [`SCHEDULER_PERIOD`] and
    /// aborts runs that exceed their trigger's `max_runtime_ticks`. Call once
    /// from the composition root after registering all tasks.
    #[must_use = "dropping the handle is fine; aborting it stops the scheduler"]
    pub fn start_scheduler(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let scheduler_start = Utc::now();
            // Startup triggers fire once, now.
            for key in this.keys_with_startup_trigger() {
                if let Err(e) = this.queue(&key) {
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

    /// Keys of tasks whose effective triggers include a `StartupTrigger`.
    fn keys_with_startup_trigger(&self) -> Vec<String> {
        lock(&self.tasks)
            .iter()
            .filter(|(_, reg)| {
                reg.effective_triggers()
                    .iter()
                    .any(|t| t.type_ == TaskTriggerInfoType::StartupTrigger)
            })
            .map(|(k, _)| k.clone())
            .collect()
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
                for (idx, trigger) in triggers.iter().enumerate() {
                    let last_fire = reg.trigger_fires.get(&idx).copied();
                    if trigger_due(trigger, now, last_fire, last_run_end, scheduler_start) {
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
            if let Err(e) = self.queue(&key) {
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
///   fire, the task's last run end, or scheduler start (so a fresh boot waits a
///   full interval rather than firing everything at once).
/// - `StartupTrigger`: handled at scheduler start, never due in a sweep.
fn trigger_due(
    trigger: &TaskTriggerInfo,
    now: DateTime<Utc>,
    last_fire: Option<DateTime<Utc>>,
    last_run_end: Option<DateTime<Utc>>,
    scheduler_start: DateTime<Utc>,
) -> bool {
    match trigger.type_ {
        TaskTriggerInfoType::StartupTrigger => false,
        TaskTriggerInfoType::IntervalTrigger => {
            let Some(interval) = trigger.interval_ticks.filter(|t| *t > 0) else {
                return false;
            };
            let base = [Some(scheduler_start), last_fire, last_run_end]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(scheduler_start);
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
            // An occurrence that predates scheduler start never fires: booting
            // at noon must not immediately run every daily-at-3am task (the C#
            // DailyTrigger schedules the *next* occurrence at startup).
            now >= scheduled
                && scheduled >= scheduler_start
                && last_fire.is_none_or(|f| f < scheduled)
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
    library: Arc<dyn hermit_traits::library::LibraryManager>,
}

impl RefreshLibraryTask {
    /// Builds the task over the library-manager seam it scans through.
    #[must_use]
    pub fn new(library: Arc<dyn hermit_traits::library::LibraryManager>) -> Self {
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
        self.library.queue_library_scan().await
    }
}

/// Bridges the concrete registry onto the `hermit-traits` [`TaskManager`] seam
/// the API layer depends on.
///
/// Delegates to the inherent [`list`](HermitTaskManager::list) /
/// [`get`](HermitTaskManager::get) / [`queue`](HermitTaskManager::queue)
/// methods; the read side is infallible here (an in-memory registry), so the
/// `Result` wrappers always yield `Ok`.
#[async_trait]
impl hermit_traits::tasks::TaskManager for HermitTaskManager {
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use hermit_model::tasks::{
        TaskCompletionStatus, TaskState, TaskTriggerInfo, TaskTriggerInfoType,
    };

    use hermit_traits::error::ServiceError;

    use super::{HermitTaskManager, ScheduledTask, TICKS_PER_SECOND, TaskProgress, trigger_due};

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
        let mgr = HermitTaskManager::new();
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
        let mgr = HermitTaskManager::new();
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

    #[tokio::test]
    async fn cancel_aborts_a_queued_run_and_records_cancelled() {
        let mgr = HermitTaskManager::new();
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
        let mgr = HermitTaskManager::new();
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
        let mgr = HermitTaskManager::new();
        assert!(matches!(
            mgr.run_now("nope").await,
            Err(ServiceError::NotFound(_))
        ));
        assert!(mgr.get("nope").is_none());
        assert!(matches!(mgr.cancel("nope"), Err(ServiceError::NotFound(_))));
    }

    #[tokio::test]
    async fn register_replaces_same_key() {
        let mgr = HermitTaskManager::new();
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

        let mgr = HermitTaskManager::new();
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
        let mgr2 = HermitTaskManager::new();
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

    #[test]
    fn interval_trigger_due_after_interval_from_latest_reference() {
        let start = Utc
            .with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
            .single()
            .expect("ts");
        let trigger = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            interval_ticks: Some(3600 * TICKS_PER_SECOND), // 1 hour
            ..TaskTriggerInfo::default()
        };

        // Not due 30 minutes after scheduler start.
        let now = start + chrono::Duration::minutes(30);
        assert!(!trigger_due(&trigger, now, None, None, start));

        // Due one hour after scheduler start.
        let now = start + chrono::Duration::hours(1);
        assert!(trigger_due(&trigger, now, None, None, start));

        // A recent run end pushes the next fire out.
        let run_end = start + chrono::Duration::minutes(45);
        assert!(!trigger_due(&trigger, now, None, Some(run_end), start));
        let now = run_end + chrono::Duration::hours(1);
        assert!(trigger_due(&trigger, now, None, Some(run_end), start));

        // Missing/zero interval never fires.
        let no_interval = TaskTriggerInfo {
            type_: TaskTriggerInfoType::IntervalTrigger,
            ..TaskTriggerInfo::default()
        };
        assert!(!trigger_due(&no_interval, now, None, None, start));
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
            scheduled - chrono::Duration::minutes(5),
            None,
            None,
            start
        ));
        // After 03:00 local, never fired — due.
        let now = scheduled + chrono::Duration::minutes(5);
        assert!(trigger_due(&trigger, now, None, None, start));
        // Already fired for this occurrence — not due again.
        assert!(!trigger_due(&trigger, now, Some(now), None, start));
        // Next day, same wall time — due again.
        let tomorrow = now + chrono::Duration::days(1);
        assert!(trigger_due(&trigger, tomorrow, Some(now), None, start));
        // An occurrence that predates scheduler start never fires: a boot at
        // noon must not immediately run a daily-at-3am task.
        let late_start = scheduled + chrono::Duration::hours(9);
        assert!(!trigger_due(
            &trigger,
            late_start + chrono::Duration::minutes(5),
            None,
            None,
            late_start
        ));
    }

    #[test]
    fn weekly_trigger_requires_matching_weekday() {
        use chrono::{Datelike as _, Local, TimeZone as _};
        use hermit_model::dto::DayOfWeek;

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
        assert!(trigger_due(&wednesday, now, None, None, start));

        let thursday = TaskTriggerInfo {
            day_of_week: Some(DayOfWeek::Thursday),
            ..wednesday
        };
        assert!(!trigger_due(&thursday, now, None, None, start));

        // Weekly without a day never fires.
        let dayless = TaskTriggerInfo {
            day_of_week: None,
            ..wednesday
        };
        assert!(!trigger_due(&dayless, now, None, None, start));
    }

    #[tokio::test]
    async fn scheduler_fires_startup_triggers_and_aborts_overruns() {
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

        let mgr = HermitTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(StartupTask { runs: runs.clone() }));

        let handle = mgr.start_scheduler();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while runs.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "startup never fired");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        handle.abort();

        // Overrun abort: a hung run whose trigger caps runtime is aborted by a
        // sweep once the cap has passed.
        let mgr = HermitTaskManager::new();
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
        use hermit_traits::tasks::TaskManager;

        let mgr = HermitTaskManager::new();
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
