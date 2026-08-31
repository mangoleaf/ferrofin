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
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeConfig,
    FakeDevices, FakeDisplayPreferences, FakeDto, FakeFileSystem, FakeLibrary, FakeLocalization,
    FakeLyrics, FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeProviders,
    FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem,
    FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers,
};
use ferrofin_model::tasks::TaskInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::tasks::TaskManager;

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
        Arc::new(OkAuth),
        Arc::new(OkAuth),
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

#[tokio::test]
async fn channel_features_for_an_unbacked_id_is_400() {
    // Upstream: `ChannelManager.GetChannelFeatures(id)` -> `GetChannel(id)` is
    // `_libraryManager.GetItemById(id) as Channel` -> null with no `IChannel`
    // provider registered -> `GetChannelProvider(null)` ->
    // `ArgumentNullException.ThrowIfNull(channel)` (v10.11.8 ChannelManager.cs:1177,
    // master :1176) -> `ExceptionMiddleware` `ArgumentException => 400`.
    let tasks = Arc::new(StubTasks::new(Vec::new()));
    let (status, _body) = send(
        tasks,
        "GET",
        "/Channels/11111111-1111-1111-1111-111111111111/Features",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn channel_features_never_fabricates_a_feature_set() {
    // The regression this replaces: a 200 carrying a default `ChannelFeatures`
    // that echoed the requested id back, asserting a channel no provider backs.
    // The body must not parse as one — in particular it must not carry `Id`.
    let tasks = Arc::new(StubTasks::new(Vec::new()));
    let (status, body) = send(
        tasks,
        "GET",
        "/Channels/00000000-0000-0000-0000-000000000000/Features",
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert!(
        parsed.get("Id").is_none() && parsed.get("MediaTypes").is_none(),
        "error body still looks like a ChannelFeatures: {parsed}"
    );
}

#[tokio::test]
async fn channel_items_for_an_unbacked_id_is_400() {
    // `ChannelManager.GetChannelItemsInternal` (v10.11.8 ChannelManager.cs:691-697)
    // runs the same GetChannel/GetChannelProvider pair before it queries anything.
    let tasks = Arc::new(StubTasks::new(Vec::new()));
    let (status, _body) = send(
        tasks,
        "GET",
        "/Channels/11111111-1111-1111-1111-111111111111/Items",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_collection_channel_routes_stay_200_empty() {
    // The other half of the split: these query for `Channel` items and
    // legitimately find none, so both servers answer 200 with an empty result.
    // Measured identical on the parity pair, and must not follow the per-channel
    // routes into a 4xx.
    for uri in ["/Channels", "/Channels/Features", "/Channels/Items/Latest"] {
        let tasks = Arc::new(StubTasks::new(Vec::new()));
        let (status, _body) = send(tasks, "GET", uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
}
