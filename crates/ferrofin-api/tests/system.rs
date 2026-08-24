//! System admin handler tests: info (full + public), ping, storage, logs,
//! endpoint, restart/shutdown, activity log, time-sync, and dashboard pages.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned data. Managers
//! a given handler never touches reuse the `test_support` panic fakes.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
    FakeDisplayPreferences, FakeDto, FakeLibrary, FakeLocalization, FakeLyrics, FakeMediaSegments,
    FakeMediaSources, FakeMusic, FakePlaylists, FakeProviders, FakeQuickConnect, FakeSearch,
    FakeSessions, FakeSimilarItems, FakeSubtitles, FakeTasks, FakeTrickplay, FakeTvSeries,
    FakeUserData, FakeUserViews, FakeUsers,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::activity::{ActivityLogEntry, LogLevel};
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::system::{FolderStorageInfo, PublicSystemInfo, SystemInfo, SystemStorageInfo};
use ferrofin_traits::activity::{ActivityLogQuery, ActivityManager};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::system::{ServerApplicationPaths, SystemManager};
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00D1_0000);

/// A minimal authenticated user.
fn user() -> UserEntity {
    UserEntity {
        id: USER_ID.to_string(),
        audio_language_preference: None,
        authentication_provider_id: String::new(),
        cast_receiver_id: None,
        display_collections_view: false,
        display_missing_episodes: false,
        enable_auto_login: false,
        enable_local_password: false,
        enable_next_episode_auto_play: false,
        enable_user_preference_access: false,
        hide_played_in_latest: false,
        internal_id: 0,
        invalid_login_attempt_count: 0,
        last_activity_date: None,
        last_login_date: None,
        login_attempts_before_lockout: None,
        max_active_sessions: 0,
        max_parental_rating_score: None,
        max_parental_rating_sub_score: None,
        must_update_password: false,
        password: None,
        password_reset_provider_id: String::new(),
        play_default_audio_track: false,
        remember_audio_selections: false,
        remember_subtitle_selections: false,
        remote_client_bitrate_limit: None,
        row_version: 0,
        subtitle_language_preference: None,
        subtitle_mode: 0,
        sync_play_access: 0,
        username: "bob".to_owned(),
    }
}

/// An auth stub that authenticates as [`USER_ID`].
/// `elevated` authenticates as an API key, satisfying `RequiresElevation` on
/// `Info/Storage`, `Logs`, `Logs/Log` and `Shutdown`. Off by default so
/// `Info/Public`, `Ping` and `Endpoint` keep proving they work unelevated.
///
/// `Restart` is gated too, but by `LocalAccessOrRequiresElevation` — the
/// caller's peer address decides, and `elevated` only matters off-network. Its
/// tests inject `ConnectInfo` rather than relying on this flag.
struct OkAuth {
    elevated: bool,
}

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
            is_api_key: self.elevated,
            is_authenticated: true,
            ..Default::default()
        })
    }
}

#[async_trait]
impl AuthorizationContext for OkAuth {
    async fn get_authorization_info(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
            is_api_key: self.elevated,
            is_authenticated: true,
            ..Default::default()
        })
    }
}

/// A configuration manager returning canned config + branding.
#[derive(Default)]
struct StubConfig {
    branding: Mutex<BrandingOptions>,
    paths: StubPaths,
}

/// Application paths returning a fixed log directory.
#[derive(Default, Clone)]
struct StubPaths {
    log_dir: String,
}

impl ServerApplicationPaths for StubPaths {
    fn root_folder_path(&self) -> String {
        String::new()
    }
    fn default_user_views_path(&self) -> String {
        String::new()
    }
    fn people_path(&self) -> String {
        String::new()
    }
    fn genre_path(&self) -> String {
        String::new()
    }
    fn music_genre_path(&self) -> String {
        String::new()
    }
    fn studio_path(&self) -> String {
        String::new()
    }
    fn year_path(&self) -> String {
        String::new()
    }
    fn artists_path(&self) -> String {
        String::new()
    }
    fn user_configuration_directory_path(&self) -> String {
        String::new()
    }
    fn internal_metadata_path(&self) -> String {
        String::new()
    }
    fn program_data_path(&self) -> String {
        String::new()
    }
    fn web_path(&self) -> String {
        String::new()
    }
    fn data_path(&self) -> String {
        String::new()
    }
    fn image_cache_path(&self) -> String {
        String::new()
    }
    fn cache_path(&self) -> String {
        String::new()
    }
    fn log_directory_path(&self) -> String {
        self.log_dir.clone()
    }
}

#[async_trait]
impl ServerConfigurationManager for StubConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(self.paths.clone())
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(ServerConfiguration {
            server_name: "Ferrofin".to_owned(),
            ..Default::default()
        }))
    }
    async fn update_configuration(
        &self,
        _configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Ok(self.branding.lock().unwrap().clone())
    }
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        *self.branding.lock().unwrap() = branding.clone();
        Ok(())
    }
}

/// An activity manager returning one entry, capturing the query.
#[derive(Default)]
struct StubActivity {
    last_query: Mutex<Option<ActivityLogQuery>>,
}

#[async_trait]
impl ActivityManager for StubActivity {
    async fn get_paged_result(
        &self,
        query: &ActivityLogQuery,
    ) -> Result<QueryResult<ActivityLogEntry>, ServiceError> {
        *self.last_query.lock().unwrap() = Some(query.clone());
        #[allow(deprecated)]
        let entry = ActivityLogEntry {
            id: 7,
            name: "Server started".to_owned(),
            overview: None,
            short_overview: None,
            type_: "SessionStarted".to_owned(),
            item_id: None,
            date: chrono::Utc::now(),
            user_id: Uuid::nil(),
            user_primary_image_tag: None,
            severity: LogLevel::Information,
        };
        Ok(QueryResult::new(query.start_index, Some(1), vec![entry]))
    }
    async fn create_entry(
        &self,
        _entry: ferrofin_traits::activity::ActivityLogCreate,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn clean(&self, _before: chrono::DateTime<chrono::Utc>) -> Result<u64, ServiceError> {
        Ok(0)
    }
}

/// A filesystem stub returning canned directory entries, drives, and log files.
struct StubFileSystem;

impl FileSystem for StubFileSystem {
    fn get_file_system_entries(&self, _path: &str) -> Vec<FileSystemEntryInfo> {
        vec![FileSystemEntryInfo {
            name: "movies".to_owned(),
            path: "/media/movies".to_owned(),
            type_: FileSystemEntryType::Directory,
        }]
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        vec![FileSystemEntryInfo {
            name: "/".to_owned(),
            path: "/".to_owned(),
            type_: FileSystemEntryType::Directory,
        }]
    }
    fn file_exists(&self, path: &str) -> bool {
        path == "/exists/file.txt"
    }
    fn directory_exists(&self, path: &str) -> bool {
        path == "/exists"
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        vec![FileMetadata {
            name: "ferrofin.log".to_owned(),
            full_name: "/logs/ferrofin.log".to_owned(),
            length: 42,
            date_created: chrono::Utc::now(),
            date_modified: chrono::Utc::now(),
        }]
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        if path == "/logs/ferrofin.log" {
            Ok(b"log body".to_vec())
        } else {
            Err(ServiceError::not_found("no file"))
        }
    }
}

/// A system manager returning canned public/full info + storage.
struct StubSystem;

#[async_trait]
impl SystemManager for StubSystem {
    async fn get_system_info(&self, _request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        Ok(SystemInfo::default())
    }
    async fn get_public_system_info(
        &self,
        _request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        Ok(PublicSystemInfo::default())
    }
    async fn restart(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_system_storage_info(&self) -> Result<SystemStorageInfo, ServiceError> {
        Ok(SystemStorageInfo {
            program_data_folder: FolderStorageInfo {
                path: "/data".to_owned(),
                resolved_path: "/data".to_owned(),
                free_space: 100,
                used_space: 50,
                storage_type: None,
                device_id: None,
            },
            ..Default::default()
        })
    }
}

/// The ordinary-user state.
fn full_state() -> AppState {
    full_state_with(false)
}

/// The elevated state, for `Info/Storage`, `Logs`, `Logs/Log`, `Shutdown` and
/// the dashboard configuration pages.
fn elevated_full_state() -> AppState {
    full_state_with(true)
}

/// Builds an [`AppState`] whose system/filesystem/config/activity managers are
/// stubs and the rest are panic fakes.
fn full_state_with(elevated: bool) -> AppState {
    let auth = Arc::new(OkAuth { elevated });
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(StubSystem),
        Arc::new(FakeAppHost),
        Arc::new(StubConfig::default()),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        auth.clone(),
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
        Arc::new(StubActivity::default()),
        Arc::new(StubFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Activity-log state for an elevated caller — `/System/ActivityLog/Entries` is
/// `RequiresElevation` upstream.
fn state_with_activity(activity: Arc<StubActivity>) -> AppState {
    state_with_activity_elevated(activity, true)
}

/// Builds an [`AppState`] whose activity manager is `activity`; others are stubs.
fn state_with_activity_elevated(activity: Arc<StubActivity>, elevated: bool) -> AppState {
    let auth = Arc::new(OkAuth { elevated });
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(StubSystem),
        Arc::new(FakeAppHost),
        Arc::new(StubConfig::default()),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        auth.clone(),
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
        activity,
        Arc::new(StubFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Sends an authenticated GET and returns `(status, body-bytes)`.
async fn get(app: AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    send(app, "GET", uri, Body::empty(), None).await
}

async fn send(
    app: AppState,
    method: &str,
    uri: &str,
    body: Body,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", "tok");
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    let response = create_router(app)
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("valid JSON body")
}

#[tokio::test]
async fn system_info_authenticated_returns_body() {
    // `StubSystem` returns a default `SystemInfo`; with the always-ok auth the
    // `RequireAuth`-guarded handler runs and serializes it.
    let (status, body) = get(full_state(), "/System/Info").await;
    assert_eq!(status, StatusCode::OK);
    // A well-formed JSON object body (SystemInfo) is returned.
    assert!(json(&body).is_object());
}

#[tokio::test]
async fn public_system_info_returns_body() {
    let response = create_router(full_state())
        .oneshot(
            Request::builder()
                .uri("/System/Info/Public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    assert!(json(&bytes).is_object());
}

#[tokio::test]
async fn system_ping_returns_name() {
    let (status, body) = get(full_state(), "/System/Ping").await;
    assert_eq!(status, StatusCode::OK);
    // FakeAppHost's friendly name.
    assert_eq!(json(&body), "ferrofin-test");
}

#[tokio::test]
async fn system_storage_projects_dto() {
    let (status, body) = get(elevated_full_state(), "/System/Info/Storage").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["ProgramDataFolder"]["FreeSpace"], 100);
    assert_eq!(v["ProgramDataFolder"]["Path"], "/data");
}

#[tokio::test]
async fn system_logs_list_and_fetch() {
    let (status, body) = get(elevated_full_state(), "/System/Logs").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v[0]["Name"], "ferrofin.log");
    assert_eq!(v[0]["Size"], 42);

    let (fetch_status, fetch_body) =
        get(elevated_full_state(), "/System/Logs/Log?name=ferrofin.log").await;
    assert_eq!(fetch_status, StatusCode::OK);
    assert_eq!(String::from_utf8(fetch_body).unwrap(), "log body");

    let (missing_status, _) = get(elevated_full_state(), "/System/Logs/Log?name=absent.log").await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn system_endpoint_defaults_to_non_local() {
    let (status, body) = get(full_state(), "/System/Endpoint").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["IsLocal"], false);
    assert_eq!(v["IsInNetwork"], false);
}

#[tokio::test]
async fn system_restart_and_shutdown_no_content() {
    let (r, _) = send(
        elevated_full_state(),
        "POST",
        "/System/Restart",
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(r, StatusCode::NO_CONTENT);
    let (s, _) = send(
        elevated_full_state(),
        "POST",
        "/System/Shutdown",
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

/// `POST /System/Restart` from `peer`, as the state's user.
///
/// `peer` is injected the way the composition root's `with_connect_info` does
/// it — as a `ConnectInfo` request extension — so the policy sees a real peer
/// address. `None` stands for a request with no connection to ask.
async fn restart_from(app: AppState, peer: Option<&str>) -> StatusCode {
    let mut request = Request::builder()
        .method("POST")
        .uri("/System/Restart")
        .header("X-Emby-Token", "tok")
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
    }
    create_router(app).oneshot(request).await.unwrap().status()
}

// `LocalAccessOrRequiresElevation`: someone at the machine (or on the LAN) may
// restart it, a stranger may not. Before this gate the route took plain
// `RequireAuth`, so any account with a token could bounce the server from
// anywhere.
#[tokio::test]
async fn restart_is_allowed_from_the_local_network_whoever_asks() {
    for peer in [
        "127.0.0.1:5000",
        "[::1]:5000",
        "192.168.1.5:5000",
        "10.0.0.7:5000",
    ] {
        assert_eq!(
            restart_from(full_state(), Some(peer)).await,
            StatusCode::NO_CONTENT,
            "{peer} is on the local network — a non-admin may restart from there"
        );
    }
}

// A dual-stack bind (`--bind ::`) delivers every IPv4 client as an
// IPv4-mapped IPv6 address. Left unmapped, `::ffff:127.0.0.1` matches no IPv6
// local prefix, so the same-machine web client reads as off-network.
#[tokio::test]
async fn restart_treats_ipv4_mapped_ipv6_peers_as_the_ipv4_address() {
    for peer in ["[::ffff:127.0.0.1]:5000", "[::ffff:192.168.1.5]:5000"] {
        assert_eq!(
            restart_from(full_state(), Some(peer)).await,
            StatusCode::NO_CONTENT,
            "{peer} is a mapped local IPv4 address"
        );
    }
    assert_eq!(
        restart_from(full_state(), Some("[::ffff:203.0.113.9]:5000")).await,
        StatusCode::FORBIDDEN,
        "a mapped PUBLIC IPv4 address must stay remote — unmapping must not \
         make everything local"
    );
}

#[tokio::test]
async fn restart_from_off_network_requires_an_administrator() {
    for peer in ["203.0.113.9:5000", "[2001:db8::1]:5000"] {
        assert_eq!(
            restart_from(full_state(), Some(peer)).await,
            StatusCode::FORBIDDEN,
            "{peer} is off-network — an ordinary account must not restart the server"
        );
        assert_eq!(
            restart_from(elevated_full_state(), Some(peer)).await,
            StatusCode::NO_CONTENT,
            "{peer} is off-network, but an administrator may still restart"
        );
    }
}

// Deliberate divergence: C# accepts a null peer address as local. Here the
// address is missing only when there is no connection to ask, so the unknown
// case is the guarded one rather than the permissive one.
#[tokio::test]
async fn restart_treats_an_unknown_peer_as_remote() {
    assert_eq!(
        restart_from(full_state(), None).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        restart_from(elevated_full_state(), None).await,
        StatusCode::NO_CONTENT
    );
}

// `POST /System/Shutdown` is `RequiresElevation` in C# with no local-access
// arm, so being on the LAN must not buy a non-admin a shutdown.
#[tokio::test]
async fn shutdown_stays_admin_only_even_from_the_local_network() {
    let mut request = Request::builder()
        .method("POST")
        .uri("/System/Shutdown")
        .header("X-Emby-Token", "tok")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:5000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let status = create_router(full_state())
        .oneshot(request)
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn activity_log_binds_query_and_returns_entries() {
    let activity = Arc::new(StubActivity::default());
    let app = state_with_activity(activity.clone());
    let (status, body) = get(
        app,
        "/System/ActivityLog/Entries?startIndex=0&limit=5&hasUserId=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["TotalRecordCount"], 1);
    assert_eq!(v["Items"][0]["Name"], "Server started");
    let q = activity.last_query.lock().unwrap().clone().unwrap();
    assert_eq!(q.limit, Some(5));
    assert_eq!(q.has_user_id, Some(true));
}

#[tokio::test]
async fn dashboard_pages_empty_and_page_not_found() {
    let (pages, pb) = get(elevated_full_state(), "/web/ConfigurationPages").await;
    assert_eq!(pages, StatusCode::OK);
    assert!(json(&pb).as_array().unwrap().is_empty());

    let (page, _) = get(full_state(), "/web/ConfigurationPage?name=whatever").await;
    assert_eq!(page, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn time_sync_returns_two_timestamps() {
    let (status, body) = get(full_state(), "/GetUtcTime").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert!(v["RequestReceptionTime"].is_string());
    assert!(v["ResponseTransmissionTime"].is_string());
}
