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
            triggers: self.task.default_triggers(),
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
}
