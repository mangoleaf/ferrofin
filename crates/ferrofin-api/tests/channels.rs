//! Channels handler test: `ChannelsController` with no providers resolves to an
//! empty result (not a `501` stub).
//!
//! Drives the real handler through `tower::ServiceExt::oneshot` with a compact
//! [`TaskManager`] stub and always-ok auth; every other manager reuses the
//! `test_support` panic fakes.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeActivity, FakeAdminUsers, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections,
    FakeConfig, FakeDevices, FakeDisplayPreferences, FakeDto, FakeFileSystem, FakeLibrary,
    FakeLocalization, FakeLyrics, FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists,
    FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles,
    FakeSystem, FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers,
    fake_user_entity,
};
use ferrofin_model::tasks::TaskInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::tasks::TaskManager;
use uuid::Uuid;

/// The authenticated caller in the cross-user probes.
const CALLER_ID: Uuid = Uuid::from_u128(0x00C4_0000);

/// Another account, never the caller.
const OTHER_USER_ID: Uuid = Uuid::from_u128(0x00C4_0001);

/// An auth pair that authenticates every request.
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
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
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// An auth pair authenticating as a concrete user, so the cross-user gate has a
/// caller identity and a role to resolve.
struct UserAuth(Uuid);

#[async_trait]
impl AuthService for UserAuth {
    async fn authenticate(&self, _r: &RequestContext) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(fake_user_entity(self.0, "caller")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for UserAuth {
    async fn get_authorization_info(
        &self,
        _r: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            user: Some(fake_user_entity(self.0, "caller")),
            is_authenticated: true,
            ..AuthorizationInfo::default()
        })
    }
}

/// A [`TaskManager`] stub with a fixed (empty) task set.
struct StubTasks {
    tasks: Vec<TaskInfo>,
    started: Arc<Mutex<Vec<String>>>,
    cancelled: Arc<Mutex<Vec<String>>>,
    triggered: Arc<Mutex<Vec<(String, usize)>>>,
}

impl StubTasks {
    fn new(tasks: Vec<TaskInfo>) -> Self {
        Self {
            tasks,
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
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskInfo>, ServiceError> {
        Ok(self
            .tasks
            .iter()
            .find(|t| t.id.as_deref() == Some(task_id))
            .cloned())
    }
    async fn start_task(&self, task_id: &str) -> Result<(), ServiceError> {
        self.started.lock().expect("lock").push(task_id.to_owned());
        Ok(())
    }
    async fn cancel_task(&self, task_id: &str) -> Result<(), ServiceError> {
        self.cancelled
            .lock()
            .expect("lock")
            .push(task_id.to_owned());
        Ok(())
    }
    async fn update_triggers(
        &self,
        task_id: &str,
        triggers: &[ferrofin_model::tasks::TaskTriggerInfo],
    ) -> Result<(), ServiceError> {
        self.triggered
            .lock()
            .expect("lock")
            .push((task_id.to_owned(), triggers.len()));
        Ok(())
    }
}

/// Builds an [`AppState`] whose task manager is `tasks` with always-ok auth; every
/// other manager is a panic fake.
fn state(tasks: Arc<StubTasks>) -> AppState {
    let auth = Arc::new(OkAuth);
    state_as(tasks, Arc::new(FakeUsers), auth.clone(), auth)
}

/// [`state`], with the caller's identity and role chosen by the test:
/// [`FakeUsers`] reports an ordinary account, [`FakeAdminUsers`] an
/// administrator.
fn state_as(
    tasks: Arc<StubTasks>,
    users: Arc<dyn ferrofin_traits::library::UserManager>,
    auth: Arc<dyn AuthService>,
    auth_ctx: Arc<dyn AuthorizationContext>,
) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        users,
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
        auth_ctx,
        auth,
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

/// Drives one request through a router built from `tasks`.
async fn send(tasks: Arc<StubTasks>, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
    use tower::ServiceExt;
    let router = create_router(state(tasks));
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
async fn channels_route_returns_empty_not_501() {
    // `ChannelsController` is implemented: with no channel providers it resolves
    // to an empty result (a stock Jellyfin with none registered behaves the same).
    let tasks = Arc::new(StubTasks::new(Vec::new()));
    let (status, _body) = send(tasks, "GET", "/Channels").await;
    assert_eq!(status, StatusCode::OK);
}

/// The three user-scoped channel routes run C# `RequestHelpers.GetUserId` on
/// `userId` before the (empty) query, so a non-administrator naming another
/// user's id is a `403` upstream. Ferrofin dropped the parameter on the floor
/// and answered `200` — an empty body is still an answer the caller was not
/// entitled to.
#[tokio::test]
async fn channels_cross_user_as_non_admin_is_forbidden() {
    for uri in [
        format!("/Channels?userId={OTHER_USER_ID}"),
        format!("/Channels/{OTHER_USER_ID}/Items?userId={OTHER_USER_ID}"),
        format!("/Channels/Items/Latest?userId={OTHER_USER_ID}"),
    ] {
        let auth = Arc::new(UserAuth(CALLER_ID));
        let router = create_router(state_as(
            Arc::new(StubTasks::new(Vec::new())),
            Arc::new(FakeUsers),
            auth.clone(),
            auth,
        ));
        let (status, _) = oneshot(router, &uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}

/// The administrator side of the same rule still resolves to the empty result.
#[tokio::test]
async fn channels_cross_user_as_admin_is_allowed() {
    for uri in [
        format!("/Channels?userId={OTHER_USER_ID}"),
        format!("/Channels/{OTHER_USER_ID}/Items?userId={OTHER_USER_ID}"),
        format!("/Channels/Items/Latest?userId={OTHER_USER_ID}"),
    ] {
        let auth = Arc::new(UserAuth(CALLER_ID));
        let router = create_router(state_as(
            Arc::new(StubTasks::new(Vec::new())),
            Arc::new(FakeAdminUsers),
            auth.clone(),
            auth,
        ));
        let (status, _) = oneshot(router, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}

/// A non-administrator naming their own id is served, as before the gate.
#[tokio::test]
async fn channels_self_as_non_admin_is_allowed() {
    for uri in [
        format!("/Channels?userId={CALLER_ID}"),
        format!("/Channels/{OTHER_USER_ID}/Items?userId={CALLER_ID}"),
        format!("/Channels/Items/Latest?userId={CALLER_ID}"),
    ] {
        let auth = Arc::new(UserAuth(CALLER_ID));
        let router = create_router(state_as(
            Arc::new(StubTasks::new(Vec::new())),
            Arc::new(FakeUsers),
            auth.clone(),
            auth,
        ));
        let (status, _) = oneshot(router, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}

/// Drives one authenticated GET through `router`.
async fn oneshot(router: axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    use tower::ServiceExt;
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
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
