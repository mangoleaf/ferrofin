//! Scheduled-tasks handler tests: `ScheduledTasksController` read/run/stop and
//! trigger update (`GET /ScheduledTasks`, `GET /ScheduledTasks/{taskId}`,
//! `POST /ScheduledTasks/Running/{taskId}`, `DELETE /ScheduledTasks/Running/{taskId}`,
//! `POST /ScheduledTasks/{taskId}/Triggers`).
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with a
//! compact [`TaskManager`] stub that returns canned [`TaskInfo`]s and records
//! run-now/cancel/trigger calls; every manager the handler does not touch reuses
//! the `test_support` panic fakes.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeConfig,
    FakeDevices, FakeDisplayPreferences, FakeDto, FakeFileSystem, FakeLibrary, FakeLocalization,
    FakeLyrics, FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeProviders,
    FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem,
    FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers,
};
use ferrofin_model::tasks::{TaskInfo, TaskState};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::tasks::TaskManager;

/// An auth pair that authenticates every request (no user detail needed — the
/// scheduled-task handlers only require *some* valid credential).
/// Every `ScheduledTasksController` route is `RequiresElevation` upstream,
/// so this stub authenticates as an API key — which satisfies the policy
/// without a user/policy lookup, exactly as C# does. The gate itself is pinned
/// end to end in `apps/ferrofin-server/tests/elevation.rs`.
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            is_api_key: true,
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            is_api_key: true,
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A rejecting auth pair — every request is unauthenticated (`401`).
struct NoAuth;

#[async_trait]
impl AuthService for NoAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Err(ServiceError::unauthorized("no credentials"))
    }
}

#[async_trait]
impl AuthorizationContext for NoAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo::default())
    }
}

/// Builds a [`TaskInfo`] with the given key/name and hidden flag.
fn task_info(key: &str, name: &str, hidden: bool) -> TaskInfo {
    TaskInfo {
        name: Some(name.to_owned()),
        state: TaskState::Idle,
        current_progress_percentage: None,
        id: Some(key.to_owned()),
        last_execution_result: None,
        triggers: Vec::new(),
        description: Some(format!("{name} description")),
        category: Some("Test".to_owned()),
        is_hidden: hidden,
        key: Some(key.to_owned()),
    }
}

/// The `(isHidden, isEnabled)` pair the handler forwarded to the task manager.
type TaskFilters = (Option<bool>, Option<bool>);

/// A [`TaskManager`] stub with a fixed task set that records `start_task`,
/// `cancel_task`, and `update_triggers` calls.
struct StubTasks {
    tasks: Vec<TaskInfo>,
    /// Keys of the tasks that implement C# `IConfigurableScheduledTask` — the
    /// only ones `GET /ScheduledTasks`' filters may reject. Empty by default.
    configurable: Vec<String>,
    /// The `(isHidden, isEnabled)` pair the handler forwarded, if any.
    filters: Arc<Mutex<Vec<TaskFilters>>>,
    started: Arc<Mutex<Vec<String>>>,
    cancelled: Arc<Mutex<Vec<String>>>,
    triggered: Arc<Mutex<Vec<(String, usize)>>>,
}

impl StubTasks {
    /// Marks these task keys configurable, so the query filters apply to them.
    fn configurable(mut self, keys: &[&str]) -> Self {
        self.configurable = keys.iter().map(|k| (*k).to_owned()).collect();
        self
    }

    fn new(tasks: Vec<TaskInfo>) -> Self {
        Self {
            tasks,
            configurable: Vec::new(),
            filters: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Mutex::new(Vec::new())),
            cancelled: Arc::new(Mutex::new(Vec::new())),
            triggered: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl TaskManager for StubTasks {
    async fn get_tasks(&self) -> Result<Vec<TaskInfo>, ServiceError> {
        Ok(self.tasks.clone())
    }
    /// The C# rule: the filters are applied only inside
    /// `if (task.ScheduledTask is IConfigurableScheduledTask)`. Every stub task
    /// is enabled, as every 10.11.8 configurable task is.
    async fn get_tasks_filtered(
        &self,
        is_hidden: Option<bool>,
        is_enabled: Option<bool>,
    ) -> Result<Vec<TaskInfo>, ServiceError> {
        self.filters
            .lock()
            .expect("lock")
            .push((is_hidden, is_enabled));
        Ok(self
            .tasks
            .iter()
            .filter(|t| {
                let configurable = t
                    .key
                    .as_deref()
                    .is_some_and(|k| self.configurable.iter().any(|c| c == k));
                if !configurable {
                    return true;
                }
                // Every stub task is enabled (as every 10.11.8 configurable
                // task is), so `isEnabled=false` rejects them all.
                is_hidden.is_none_or(|want| want == t.is_hidden) && is_enabled != Some(false)
            })
            .cloned()
            .collect())
    }
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskInfo>, ServiceError> {
        Ok(self
            .tasks
            .iter()
            .find(|t| t.id.as_deref() == Some(task_id))
            .cloned())
    }
    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError> {
        if self.tasks.iter().any(|t| t.id.as_deref() == Some(task_id)) {
            self.started.lock().expect("lock").push(task_id.to_owned());
            Ok(())
        } else {
            Err(ServiceError::not_found(format!("scheduled task {task_id}")))
        }
    }
    async fn cancel_task(&self, task_id: &str) -> Result<(), ServiceError> {
        if self.tasks.iter().any(|t| t.id.as_deref() == Some(task_id)) {
            self.cancelled
                .lock()
                .expect("lock")
                .push(task_id.to_owned());
            Ok(())
        } else {
            Err(ServiceError::not_found(format!("scheduled task {task_id}")))
        }
    }
    async fn update_triggers(
        &self,
        task_id: &str,
        triggers: &[ferrofin_model::tasks::TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        if self.tasks.iter().any(|t| t.id.as_deref() == Some(task_id)) {
            self.triggered
                .lock()
                .expect("lock")
                .push((task_id.to_owned(), triggers.len()));
            Ok(())
        } else {
            Err(ServiceError::not_found(format!("scheduled task {task_id}")))
        }
    }
}

/// Builds an [`AppState`] whose task manager is `tasks` and auth is `auth`; every
/// other manager is a panic fake.
fn state(tasks: Arc<StubTasks>, authed: bool) -> AppState {
    let auth_service: Arc<dyn AuthService> = if authed {
        Arc::new(OkAuth)
    } else {
        Arc::new(NoAuth)
    };
    let auth_context: Arc<dyn AuthorizationContext> = if authed {
        Arc::new(OkAuth)
    } else {
        Arc::new(NoAuth)
    };
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        auth_context,
        auth_service,
        Arc::new(FakeQuickConnect),
        Arc::new(FakePlaylists),
        Arc::new(FakeCollections),
        Arc::new(FakeTvSeries),
        Arc::new(FakeSubtitles),
        Arc::new(FakeLyrics),
        Arc::new(FakeMediaSegments),
        Arc::new(FakeTrickplay),
        Arc::new(FakeDevices),
        Arc::new(FakeClientEventLogger),
        Arc::new(FakeApiKeys),
        Arc::new(FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(FakeActivity),
        Arc::new(FakeFileSystem),
        tasks,
    )
}

/// Drives one request through a router built from `tasks`/`authed`.
async fn send(
    tasks: Arc<StubTasks>,
    authed: bool,
    method: &str,
    uri: &str,
) -> (StatusCode, Vec<u8>) {
    use tower::ServiceExt;
    let router = create_router(state(tasks, authed));
    let response = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", "Token abc")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn get_tasks_returns_every_task() {
    let tasks = Arc::new(StubTasks::new(vec![
        task_info("scan", "Scan", false),
        task_info("cleanup", "Cleanup", true),
    ]));
    let (status, body) = send(tasks, true, "GET", "/ScheduledTasks").await;
    assert_eq!(status, StatusCode::OK);
    let result: Vec<TaskInfo> = serde_json::from_slice(&body).expect("task list");
    assert_eq!(result.len(), 2);
}

#[tokio::test]
async fn get_tasks_is_hidden_filter_keeps_only_hidden() {
    let tasks = Arc::new(
        StubTasks::new(vec![
            task_info("scan", "Scan", false),
            task_info("cleanup", "Cleanup", true),
        ])
        .configurable(&["scan", "cleanup"]),
    );
    let (status, body) = send(tasks, true, "GET", "/ScheduledTasks?isHidden=true").await;
    assert_eq!(status, StatusCode::OK);
    let result: Vec<TaskInfo> = serde_json::from_slice(&body).expect("task list");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id.as_deref(), Some("cleanup"));
}

/// C# `ScheduledTasksController.GetTasks` applies `isHidden`/`isEnabled` only
/// inside `if (task.ScheduledTask is IConfigurableScheduledTask)`, so a task
/// that does not implement the interface is listed whatever the caller asked
/// for. Eleven of 10.11.8's twenty tasks are in that group.
#[tokio::test]
async fn get_tasks_filters_never_reject_a_non_configurable_task() {
    let tasks = Arc::new(
        StubTasks::new(vec![
            task_info("scan", "Scan", false),
            task_info("cleanup", "Cleanup", true),
        ])
        .configurable(&["cleanup"]),
    );
    let (status, body) = send(
        Arc::clone(&tasks),
        true,
        "GET",
        "/ScheduledTasks?isHidden=true&isEnabled=false",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: Vec<TaskInfo> = serde_json::from_slice(&body).expect("task list");
    // "scan" is not hidden and would fail both filters, but it is not
    // configurable, so it survives; "cleanup" is configurable and is rejected.
    assert_eq!(
        result
            .iter()
            .filter_map(|t| t.id.as_deref())
            .collect::<Vec<_>>(),
        vec!["scan"]
    );
    // The handler forwarded both query parameters verbatim.
    assert_eq!(
        *tasks.filters.lock().expect("lock"),
        vec![(Some(true), Some(false))]
    );
}

#[tokio::test]
async fn get_tasks_is_enabled_false_returns_none() {
    let tasks =
        Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]).configurable(&["scan"]));
    let (status, body) = send(tasks, true, "GET", "/ScheduledTasks?isEnabled=false").await;
    assert_eq!(status, StatusCode::OK);
    let result: Vec<TaskInfo> = serde_json::from_slice(&body).expect("task list");
    assert!(result.is_empty());
}

#[tokio::test]
async fn get_task_by_id_returns_it() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, body) = send(tasks, true, "GET", "/ScheduledTasks/scan").await;
    assert_eq!(status, StatusCode::OK);
    let result: TaskInfo = serde_json::from_slice(&body).expect("task");
    assert_eq!(result.id.as_deref(), Some("scan"));
    assert_eq!(result.name.as_deref(), Some("Scan"));
}

#[tokio::test]
async fn get_task_unknown_id_is_404() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, _body) = send(tasks, true, "GET", "/ScheduledTasks/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn start_task_runs_it_and_returns_204() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, _body) = send(
        Arc::clone(&tasks),
        true,
        "POST",
        "/ScheduledTasks/Running/scan",
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(tasks.started.lock().expect("lock").as_slice(), ["scan"]);
}

#[tokio::test]
async fn start_task_unknown_id_is_404() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, _body) = send(
        Arc::clone(&tasks),
        true,
        "POST",
        "/ScheduledTasks/Running/nope",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(tasks.started.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn get_tasks_requires_auth() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, _body) = send(tasks, false, "GET", "/ScheduledTasks").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Drives one request with a JSON `body` through a router built from
/// `tasks`/`authed`.
async fn send_json(
    tasks: Arc<StubTasks>,
    authed: bool,
    method: &str,
    uri: &str,
    body: &str,
) -> StatusCode {
    use tower::ServiceExt;
    let router = create_router(state(tasks, authed));
    let response = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", "Token abc")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    response.status()
}

#[tokio::test]
async fn stop_task_cancels_and_returns_204() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, _body) = send(
        Arc::clone(&tasks),
        true,
        "DELETE",
        "/ScheduledTasks/Running/scan",
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(tasks.cancelled.lock().expect("lock").as_slice(), ["scan"]);
}

#[tokio::test]
async fn stop_task_unknown_id_is_404() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let (status, _body) = send(
        Arc::clone(&tasks),
        true,
        "DELETE",
        "/ScheduledTasks/Running/nope",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(tasks.cancelled.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn update_triggers_persists_and_returns_204() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let status = send_json(
        Arc::clone(&tasks),
        true,
        "POST",
        "/ScheduledTasks/scan/Triggers",
        r#"[{"Type":"IntervalTrigger","IntervalTicks":42}]"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let recorded = tasks.triggered.lock().expect("lock");
    assert_eq!(recorded.as_slice(), [("scan".to_owned(), 1)]);
}

#[tokio::test]
async fn update_triggers_unknown_id_is_404() {
    let tasks = Arc::new(StubTasks::new(vec![task_info("scan", "Scan", false)]));
    let status = send_json(
        Arc::clone(&tasks),
        true,
        "POST",
        "/ScheduledTasks/nope/Triggers",
        "[]",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(tasks.triggered.lock().expect("lock").is_empty());
}
