//! [`HermitTaskManager`] — a **minimal** scheduled-task registry (no scheduler).
//!
//! Port of `Emby.Server.Implementations.ScheduledTasks.TaskManager` +
//! `ScheduledTaskWorker`, reduced to the registry core the rest of the server
//! needs to *enumerate* the tasks and *run one now*. The C# cron machinery — the
//! `ITaskTrigger` timers (`DailyTrigger`/`IntervalTrigger`/`StartupTrigger`), the
//! background execution queue (`ConcurrentQueue`), the on-disk trigger-config and
//! last-result persistence (`<key>.js`), and the completion event fan-out — is
//! **deferred**: this registry never fires a task on its own. A real scheduler is
//! a future wave and can wrap this same registry.
//!
//! No `hermit-traits` trait exists for this seam yet, so the registry is a
//! self-contained `hermit-core` type. A registered unit implements the local
//! [`ScheduledTask`] trait (the port of `IScheduledTask`); the registry owns each
//! task's live [`TaskState`] and its last [`TaskResult`], surfacing them as the
//! `hermit-model` [`TaskInfo`] wire DTO.
//!
//! Reused verbatim from `hermit-model`: [`TaskInfo`], [`TaskResult`],
//! [`TaskState`], [`TaskCompletionStatus`], [`TaskTriggerInfo`] — no new DTOs are
//! defined here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use hermit_model::tasks::{TaskCompletionStatus, TaskInfo, TaskResult, TaskState, TaskTriggerInfo};

use hermit_traits::error::ServiceError;

/// A unit of background work the registry can enumerate and run on demand.
///
/// Port of `MediaBrowser.Model.Tasks.IScheduledTask` (the `IConfigurableScheduledTask`
/// trigger-config surface is deferred with the scheduler). A task advertises
/// stable metadata and an idempotent [`execute`](ScheduledTask::execute) body;
/// the registry — not the task — owns run state and history.
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

    /// The task's default triggers (advisory only — this registry has no
    /// scheduler, so triggers are surfaced in [`TaskInfo`] but never fired).
    fn default_triggers(&self) -> Vec<TaskTriggerInfo> {
        Vec::new()
    }

    /// Runs the task to completion.
    ///
    /// Errors surface as a [`TaskCompletionStatus::Failed`] result recorded by
    /// the registry; the returned `Result` also propagates to the caller of
    /// [`HermitTaskManager::run_now`].
    async fn execute(&self) -> Result<(), ServiceError>;
}

/// One registered task plus the registry-owned run state and last result.
struct Registration {
    task: Arc<dyn ScheduledTask>,
    state: TaskState,
    last_result: Option<TaskResult>,
    /// Triggers set via [`HermitTaskManager::set_triggers`], overriding the
    /// task's [`default_triggers`](ScheduledTask::default_triggers). `None` until
    /// a client configures them (the C# `IConfigurableScheduledTask.Triggers`
    /// starts from the defaults, then is overwritten by an assignment).
    triggers_override: Option<Vec<TaskTriggerInfo>>,
}

impl Registration {
    /// Projects the registration onto the `hermit-model` wire DTO.
    fn to_info(&self) -> TaskInfo {
        let key = self.task.key().to_string();
        TaskInfo {
            name: Some(self.task.name().to_string()),
            state: self.state,
            current_progress_percentage: None,
            id: Some(key.clone()),
            last_execution_result: self.last_result.clone(),
            triggers: self
                .triggers_override
                .clone()
                .unwrap_or_else(|| self.task.default_triggers()),
            description: Some(self.task.description().to_string()),
            category: Some(self.task.category().to_string()),
            is_hidden: self.task.is_hidden(),
            key: Some(key),
        }
    }
}

/// The minimal scheduled-task registry.
///
/// Register tasks up front, enumerate them as [`TaskInfo`], and run one now by
/// key. There is deliberately no timer/queue: nothing runs unless
/// [`run_now`](Self::run_now) is called.
#[derive(Clone, Default)]
pub struct HermitTaskManager {
    // Keyed by `ScheduledTask::key`, mirroring the C# lookup-by-type. Guarded by
    // a std mutex — critical sections are trivial map edits, never held across an
    // `.await`; task execution happens on a clone taken *outside* the lock.
    tasks: Arc<Mutex<HashMap<String, Registration>>>,
}

impl std::fmt::Debug for HermitTaskManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.tasks.lock().map_or(0, |t| t.len());
        f.debug_struct("HermitTaskManager")
            .field("task_count", &count)
            .finish()
    }
}

impl HermitTaskManager {
    /// Creates an empty task registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a task, replacing any existing task with the same key.
    ///
    /// Mirrors the C# `AddTasks`; the new registration starts
    /// [`Idle`](TaskState::Idle) with no last result.
    pub fn register(&self, task: Arc<dyn ScheduledTask>) {
        let key = task.key().to_string();
        let registration = Registration {
            task,
            state: TaskState::Idle,
            last_result: None,
            triggers_override: None,
        };
        // A poisoned lock means a prior panic while holding it; recover the guard
        // rather than propagate — the map itself is still consistent.
        let mut guard = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(key, registration);
    }

    /// Lists every registered task as a wire [`TaskInfo`] (hidden tasks included),
    /// ordered by key for a stable listing.
    #[must_use]
    pub fn list(&self) -> Vec<TaskInfo> {
        let guard = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut infos: Vec<TaskInfo> = guard.values().map(Registration::to_info).collect();
        infos.sort_by(|a, b| a.key.cmp(&b.key));
        infos
    }

    /// Gets a single task's info by key, if registered.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<TaskInfo> {
        let guard = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(key).map(Registration::to_info)
    }

    /// Sets the live state of a registered task, returning the previous state.
    fn set_state(&self, key: &str, state: TaskState) -> Option<TaskState> {
        let mut guard = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get_mut(key)
            .map(|reg| std::mem::replace(&mut reg.state, state))
    }

    /// Records a run's outcome and returns the task to [`Idle`](TaskState::Idle).
    fn record_result(&self, key: &str, result: TaskResult) {
        let mut guard = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reg) = guard.get_mut(key) {
            reg.state = TaskState::Idle;
            reg.last_result = Some(result);
        }
    }

    /// Runs the named task now, to completion, recording the outcome.
    ///
    /// This is the manual `Execute`/`RunNow` path — the *only* way a task runs in
    /// this scheduler-less registry. A failing task body is recorded as a
    /// [`TaskCompletionStatus::Failed`] result *and* propagated to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key,
    /// [`ServiceError::InvalidInput`] if it is already running (mirroring the C#
    /// guard that only queues an `Idle` task), or whatever error the task body
    /// itself returns.
    pub async fn run_now(&self, key: &str) -> Result<(), ServiceError> {
        // Take a task-handle clone and flip to `Running` under the lock, then drop
        // the guard before awaiting the body.
        let task = {
            let guard = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let reg = guard
                .get(key)
                .ok_or_else(|| ServiceError::not_found(format!("scheduled task {key}")))?;
            if reg.state == TaskState::Running {
                return Err(ServiceError::invalid_input(format!(
                    "scheduled task {key} is already running"
                )));
            }
            Arc::clone(&reg.task)
        };
        self.set_state(key, TaskState::Running);

        let start = Utc::now();
        let outcome = task.execute().await;
        let end = Utc::now();

        let (status, error_message) = match &outcome {
            Ok(()) => (TaskCompletionStatus::Completed, None),
            Err(e) => (TaskCompletionStatus::Failed, Some(e.to_string())),
        };
        self.record_result(
            key,
            TaskResult {
                start_time_utc: start,
                end_time_utc: end,
                status,
                name: Some(task.name().to_string()),
                key: Some(key.to_string()),
                id: Some(key.to_string()),
                error_message,
                long_error_message: None,
            },
        );
        outcome
    }

    /// Cancels a task, returning the task to [`Idle`](TaskState::Idle).
    ///
    /// The manual `Cancel`/`StopTask` path. This registry runs nothing in the
    /// background, so cancelling is a state reset to `Idle`; a missing task is
    /// reported so the caller can map it to a `404` (mirroring the C# guard).
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key.
    pub fn cancel(&self, key: &str) -> Result<(), ServiceError> {
        if self.set_state(key, TaskState::Idle).is_none() {
            return Err(ServiceError::not_found(format!("scheduled task {key}")));
        }
        Ok(())
    }

    /// Replaces a task's configured triggers, surfaced on its next
    /// [`TaskInfo`] read.
    ///
    /// The manual `task.Triggers = …` path. Persists the trigger list on the
    /// registration; there is no scheduler, so the triggers are advisory but
    /// visible to clients.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::NotFound`] if no task has that key.
    pub fn set_triggers(
        &self,
        key: &str,
        triggers: &[TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        let mut guard = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reg = guard
            .get_mut(key)
            .ok_or_else(|| ServiceError::not_found(format!("scheduled task {key}")))?;
        reg.triggers_override = Some(triggers.to_vec());
        Ok(())
    }
}

/// The "Scan all libraries" task — port of `RefreshMediaLibraryTask`.
///
/// Its [`execute`](ScheduledTask::execute) runs the same library scan as
/// `POST /Library/Refresh` (`LibraryManager::queue_library_scan`). The `Key` is
/// Jellyfin's well-known `"RefreshLibrary"`, so jellyfin-web's dashboard "Scan
/// all libraries" button (which starts the task by that key) drives it. Triggers
/// are advisory here (no scheduler), so it only runs when started manually.
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
        "Scan all libraries"
    }
    fn description(&self) -> &str {
        "Scans your media library for new files and refreshes metadata."
    }
    fn category(&self) -> &str {
        "Library"
    }
    async fn execute(&self) -> Result<(), ServiceError> {
        self.library.queue_library_scan().await
    }
}

/// Bridges the concrete registry onto the `hermit-traits` [`TaskManager`] seam
/// the API layer depends on.
///
/// Delegates to the inherent [`list`](HermitTaskManager::list) /
/// [`get`](HermitTaskManager::get) / [`run_now`](HermitTaskManager::run_now)
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
        self.run_now(task_id).await
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
    use hermit_model::tasks::{TaskCompletionStatus, TaskState};

    use hermit_traits::error::ServiceError;

    use super::{HermitTaskManager, ScheduledTask};

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
        async fn execute(&self) -> Result<(), ServiceError> {
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

        mgr.run_now("counting").await.expect("run");
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let info = mgr.get("counting").expect("info");
        assert_eq!(info.state, TaskState::Idle);
        let result = info.last_execution_result.expect("result");
        assert_eq!(result.status, TaskCompletionStatus::Completed);
        assert!(result.end_time_utc >= result.start_time_utc);
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
    async fn cancel_resets_state_and_missing_is_not_found() {
        let mgr = HermitTaskManager::new();
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));

        // Force the task into `Running`, then cancel it back to `Idle`.
        mgr.set_state("counting", TaskState::Running);
        mgr.cancel("counting").expect("cancel");
        assert_eq!(mgr.get("counting").expect("info").state, TaskState::Idle);

        // A missing task is NotFound.
        assert!(matches!(mgr.cancel("nope"), Err(ServiceError::NotFound(_))));
    }

    #[tokio::test]
    async fn set_triggers_overrides_and_missing_is_not_found() {
        use hermit_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};

        let mgr = HermitTaskManager::new();
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
        assert_eq!(info.triggers[0].type_, TaskTriggerInfoType::IntervalTrigger);

        // A missing task is NotFound.
        assert!(matches!(
            mgr.set_triggers("nope", &triggers),
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn trait_cancel_and_update_triggers_delegate() {
        use hermit_model::tasks::{TaskTriggerInfo, TaskTriggerInfoType};
        use hermit_traits::tasks::TaskManager;

        let mgr = HermitTaskManager::new();
        mgr.register(Arc::new(CountingTask {
            runs: Arc::new(AtomicU32::new(0)),
            fail: false,
            hidden: false,
        }));

        TaskManager::cancel_task(&mgr, "counting")
            .await
            .expect("cancel");
        assert!(matches!(
            TaskManager::cancel_task(&mgr, "nope").await,
            Err(ServiceError::NotFound(_))
        ));

        let triggers = vec![TaskTriggerInfo {
            type_: TaskTriggerInfoType::StartupTrigger,
            ..TaskTriggerInfo::default()
        }];
        TaskManager::update_triggers(&mgr, "counting", &triggers)
            .await
            .expect("update");
        let info = TaskManager::get_task(&mgr, "counting")
            .await
            .expect("get")
            .expect("info");
        assert_eq!(info.triggers.len(), 1);
        assert!(matches!(
            TaskManager::update_triggers(&mgr, "nope", &triggers).await,
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn task_manager_trait_delegates() {
        use hermit_traits::tasks::TaskManager;

        let mgr = HermitTaskManager::new();
        let runs = Arc::new(AtomicU32::new(0));
        mgr.register(Arc::new(CountingTask {
            runs: runs.clone(),
            fail: false,
            hidden: false,
        }));

        // `get_tasks` mirrors the inherent list.
        let tasks = TaskManager::get_tasks(&mgr).await.expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id.as_deref(), Some("counting"));

        // `get_task` hits / misses.
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

        // `start_task` runs it and records a completed result.
        TaskManager::start_task(&mgr, "counting")
            .await
            .expect("start");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let info = TaskManager::get_task(&mgr, "counting")
            .await
            .expect("get")
            .expect("info");
        assert_eq!(
            info.last_execution_result.expect("result").status,
            TaskCompletionStatus::Completed
        );

        // Unknown key → NotFound.
        assert!(matches!(
            TaskManager::start_task(&mgr, "nope").await,
            Err(ServiceError::NotFound(_))
        ));
    }
}
