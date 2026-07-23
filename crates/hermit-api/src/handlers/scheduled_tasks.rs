//! `ScheduledTasksController` — enumerate, fetch, and start scheduled tasks.
//!
//! Ports the read/run slice of the elevation-gated `ScheduledTasksController`:
//!
//! - `GET /ScheduledTasks` — every task as a [`TaskInfo`], with the C#
//!   `isHidden`/`isEnabled` query filters applied.
//! - `GET /ScheduledTasks/{taskId}` — one task by id (`404` when missing).
//! - `POST /ScheduledTasks/Running/{taskId}` — run a task now (`204`; `404` when
//!   missing).
//!
//! Deferred (kept on the shared `501` stub, per the batch's scheduled-task-cron
//! deferral): `DELETE /ScheduledTasks/Running/{taskId}` (`ITaskManager.Cancel` —
//! there is no background execution to cancel) and
//! `POST /ScheduledTasks/{taskId}/Triggers` (`IConfigurableScheduledTask.Triggers`
//! — trigger-config persistence). Both need the un-ported cron scheduler.
//!
//! Elevation-policy enforcement (`Policies.RequiresElevation`) is deferred to the
//! composition root, matching the other admin controllers; every handler here
//! takes [`RequireAuth`] so an unauthenticated request still gets `401`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::tasks::TaskInfo;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Query parameters for `GET /ScheduledTasks` — the hidden/enabled filters.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTasksQuery {
    /// Optional. Filter tasks that are hidden, or not.
    #[serde(default)]
    is_hidden: Option<bool>,

    /// Optional. Filter tasks that are enabled, or not.
    #[serde(default)]
    is_enabled: Option<bool>,
}

/// `GET /ScheduledTasks` — the server's scheduled tasks, filtered.
///
/// Port of `ScheduledTasksController.GetTasks`: lists every task, then applies
/// the `isHidden`/`isEnabled` filters. This registry has no per-task *enabled*
/// flag — a registered task is always enabled — so an `isEnabled=false` filter
/// matches nothing while `isEnabled=true` keeps every task. The list is already
/// ordered by key (the C# orders by name; both are stable orderings for the
/// wire).
#[utoipa::path(
    get,
    path = "/ScheduledTasks",
    params(
        ("isHidden" = Option<bool>, Query, description = "Optional filter tasks that are hidden, or not."),
        ("isEnabled" = Option<bool>, Query, description = "Optional filter tasks that are enabled, or not."),
    ),
    responses((status = 200, description = "Scheduled tasks retrieved", body = [TaskInfo])),
    tag = "hermit"
)]
async fn get_tasks(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<GetTasksQuery>,
) -> Result<Json<Vec<TaskInfo>>, ApiError> {
    let tasks = state.tasks.get_tasks().await?;
    let filtered = tasks
        .into_iter()
        .filter(|task| query.is_hidden.is_none_or(|want| want == task.is_hidden))
        // Every registered task is enabled, so `is_enabled` only keeps tasks
        // when the caller asks for enabled ones (or does not filter at all).
        .filter(|_| query.is_enabled != Some(false))
        .collect();
    Ok(Json(filtered))
}

/// `GET /ScheduledTasks/{taskId}` — a single task by id.
///
/// Port of `ScheduledTasksController.GetTask`: returns the task, or `404` when
/// no task has that id.
#[utoipa::path(
    get,
    path = "/ScheduledTasks/{taskId}",
    params(("taskId" = String, Path, description = "Task Id.")),
    responses(
        (status = 200, description = "Task retrieved", body = TaskInfo),
        (status = 404, description = "Task not found"),
    ),
    tag = "hermit"
)]
async fn get_task(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(task_id): Path<String>,
) -> Result<Json<TaskInfo>, ApiError> {
    let task = state
        .tasks
        .get_task(&task_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("scheduled task {task_id}")))?;
    Ok(Json(task))
}

/// `POST /ScheduledTasks/Running/{taskId}` — run a task now.
///
/// Port of `ScheduledTasksController.StartTask` → `ITaskManager.Execute`,
/// reduced to the synchronous manual-run path: the task runs to completion and
/// the handler returns `204 No Content`. A missing task is `404`; the underlying
/// registry maps an already-running task to `400` (the C# guard that only queues
/// an idle task).
#[utoipa::path(
    post,
    path = "/ScheduledTasks/Running/{taskId}",
    params(("taskId" = String, Path, description = "Task Id.")),
    responses(
        (status = 204, description = "Task started"),
        (status = 404, description = "Task not found"),
    ),
    tag = "hermit"
)]
async fn start_task(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(task_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // Mirror the C# 404-before-run: report a missing task as `404` rather than
    // the registry's `NotFound`-mapped `404` from `start_task` (same status, but
    // this keeps the "not found" wording aligned with `GetTask`).
    if state.tasks.get_task(&task_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("scheduled task {task_id}")));
    }
    state.tasks.start_task(&task_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/ScheduledTasks", get(get_tasks))
        .route("/ScheduledTasks/{taskId}", get(get_task))
        .route("/ScheduledTasks/Running/{taskId}", post(start_task))
}
