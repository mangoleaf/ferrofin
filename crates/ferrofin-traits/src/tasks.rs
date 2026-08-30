//! Scheduled-task manager trait — the DI seam for `ScheduledTasksController`.
//!
//! Port of the *read/run* slice of `MediaBrowser.Model.Tasks.ITaskManager`
//! (plus the `ScheduledTaskHelpers.GetTaskInfo` projection the controller calls
//! per task). A handler holds an `Arc<dyn TaskManager>` in
//! [`AppState`](../../ferrofin_api/state) and never names the concrete
//! `ferrofin-core` registry.
//!
//! **Deferred (kept off this trait):** the cron scheduler itself — the
//! `ITaskTrigger` timers, the background execution queue, per-task cancellation
//! (`ITaskManager.Cancel`), and trigger-config persistence
//! (`IConfigurableScheduledTask.Triggers = …`). Those back the two routes the
//! batch leaves on the `501` stub (`DELETE /ScheduledTasks/Running/{taskId}` and
//! `POST /ScheduledTasks/{taskId}/Triggers`). The trait exposes only what the
//! scheduler-less registry can honour: enumerate tasks, fetch one, and run one
//! now on demand.
//!
//! Reuses the `ferrofin-model` wire DTO [`TaskInfo`] verbatim.

use async_trait::async_trait;
use ferrofin_model::tasks::{TaskInfo, TaskTriggerInfo};

use crate::error::ServiceError;

/// Enumerates the server's scheduled tasks and runs one on demand.
///
/// Port of `ITaskManager` (read + manual-run slice). Object-safe so it can be
/// held as `Arc<dyn TaskManager>`.
#[async_trait]
pub trait TaskManager: Send + Sync {
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

    /// Runs the named task now, to completion, recording the outcome.
    ///
    /// Ports `ScheduledTasksController.StartTask` → `ITaskManager.Execute`,
    /// reduced to the synchronous manual-run path (there is no background queue).
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id;
    /// [`ServiceError::InvalidInput`] when it is already running; or whatever
    /// error the task body itself returns.
    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError>;

    /// Cancels a running task.
    ///
    /// Ports `ScheduledTasksController.StopTask` → `ITaskManager.Cancel`. This
    /// scheduler-less registry never runs a task in the background, so cancelling
    /// an idle task is a no-op; the observable behaviour is the C# one — a missing
    /// task is [`ServiceError::NotFound`], otherwise it succeeds.
    ///
    /// # Errors
    ///
    /// [`ServiceError::NotFound`] when no task has that id.
    async fn cancel_task(&self, task_id: &str) -> Result<(), ServiceError>;

    /// Replaces a task's configured triggers.
    ///
    /// Ports `ScheduledTasksController.UpdateTask` → `task.Triggers = triggerInfos`.
    /// The stored triggers are surfaced in the task's [`TaskInfo`]; this registry
    /// has no scheduler, so they are advisory (never fired) but do persist across
    /// reads, exactly as the C# assignment updates the configurable task's
    /// trigger list.
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
