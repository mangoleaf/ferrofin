//! System admin handler tests: info (full + public), ping, storage, logs,
//! endpoint, restart/shutdown, activity log, time-sync, and dashboard pages.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned data. Managers
//! a given handler never touches reuse the `test_support` panic fakes.

use std::io::Read as _;
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
use ferrofin_traits::filesystem::{FileMetadata, FileSystem, OpenedFile};
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
        normalized_username: "BOB".to_owned(),
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
    /// Four log files in a deliberately SCRAMBLED order, so the handler's sort —
    /// C# `OrderByDescending(DateModified).ThenByDescending(DateCreated)
    /// .ThenBy(Name)` — is actually exercised. Two share a mtime (forcing the
    /// DateCreated tiebreak) and two share both timestamps (forcing the
    /// ascending Name tiebreak).
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("timestamp")
        }
        vec![
            // Same mtime and ctime as "b.log": Name ascending decides, so
            // "a.log" must come first.
            FileMetadata {
                name: "b.log".to_owned(),
                full_name: "/logs/b.log".to_owned(),
                length: 2,
                date_created: at(50),
                date_modified: at(50),
            },
            FileMetadata {
                name: "a.log".to_owned(),
                full_name: "/logs/a.log".to_owned(),
                length: 1,
                date_created: at(50),
                date_modified: at(50),
            },
            // Newest mtime: first overall.
            FileMetadata {
                name: "ferrofin.log".to_owned(),
                full_name: "/logs/ferrofin.log".to_owned(),
                length: 42,
                date_created: at(10),
                date_modified: at(90),
            },
            // Same mtime as "a"/"b" but a NEWER ctime, so the DateCreated
            // tiebreak puts it ahead of both.
            FileMetadata {
                name: "z.log".to_owned(),
                full_name: "/logs/z.log".to_owned(),
                length: 3,
                date_created: at(80),
                date_modified: at(50),
            },
        ]
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
        Ok(PublicSystemInfo {
            // C# `SystemManager.GetPublicSystemInfo` sets
            // `ProductName = _applicationHost.Name`, the same value
            // `/System/Ping` returns — so the stub reports the product name
            // here, not the friendly name (which is `ServerName`).
            product_name: Some("Jellyfin Server".to_owned()),
            server_name: Some("ferrofin-test".to_owned()),
            ..PublicSystemInfo::default()
        })
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
    state_with_file_system(elevated, Arc::new(StubFileSystem))
}

/// [`full_state_with`] over another filesystem.
fn state_with_file_system(elevated: bool, file_system: Arc<dyn FileSystem>) -> AppState {
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
        file_system,
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
async fn system_ping_returns_the_product_name_not_the_friendly_name() {
    let (status, body) = get(full_state(), "/System/Ping").await;
    assert_eq!(status, StatusCode::OK);
    // C# `SystemController.PingSystem` returns `_appHost.Name`, i.e.
    // `ApplicationProductName` — a build constant — NOT `_appHost.FriendlyName`.
    assert_eq!(json(&body), "Jellyfin Server");
    assert_ne!(
        json(&body),
        "ferrofin-test",
        "must not echo the friendly name"
    );
}

#[tokio::test]
async fn system_ping_matches_public_info_product_name() {
    // Both are `_appHost.Name` in C# (`SystemManager.GetPublicSystemInfo` sets
    // `ProductName = _applicationHost.Name`), so they can never disagree.
    let (ping_status, ping) = get(full_state(), "/System/Ping").await;
    let (info_status, info) = get(full_state(), "/System/Info/Public").await;
    assert_eq!(ping_status, StatusCode::OK);
    assert_eq!(info_status, StatusCode::OK);
    assert_eq!(json(&ping), json(&info)["ProductName"]);
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

    // The C# sort chain, end to end:
    //   DateModified desc  → ferrofin.log (newest mtime) first
    //   DateCreated  desc  → z.log ahead of a/b (same mtime, newer ctime)
    //   Name         asc   → a.log before b.log (same mtime AND ctime)
    let names: Vec<&str> = v
        .as_array()
        .expect("array")
        .iter()
        .map(|f| f["Name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["ferrofin.log", "z.log", "a.log", "b.log"]);

    // `LogFile` is exactly four fields (the contract is additionalProperties:false).
    let keys: std::collections::BTreeSet<&str> = v[0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["DateCreated", "DateModified", "Name", "Size"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    let (fetch_status, fetch_body) =
        get(elevated_full_state(), "/System/Logs/Log?name=ferrofin.log").await;
    assert_eq!(fetch_status, StatusCode::OK);
    assert_eq!(String::from_utf8(fetch_body).unwrap(), "log body");

    let (missing_status, _) = get(elevated_full_state(), "/System/Logs/Log?name=absent.log").await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
}

/// A log folder of one file, `big.log`, whose open the test scripts.
struct OneLog(fn() -> Result<OpenedFile, ServiceError>);

impl FileSystem for OneLog {
    fn get_file_system_entries(&self, _path: &str) -> Vec<FileSystemEntryInfo> {
        unimplemented!("unused")
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        unimplemented!("unused")
    }
    fn file_exists(&self, _path: &str) -> bool {
        unimplemented!("unused")
    }
    fn directory_exists(&self, _path: &str) -> bool {
        unimplemented!("unused")
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        unimplemented!("unused")
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        vec![FileMetadata {
            name: "big.log".to_owned(),
            full_name: "/logs/big.log".to_owned(),
            length: 0,
            date_created: chrono::DateTime::UNIX_EPOCH,
            date_modified: chrono::DateTime::UNIX_EPOCH,
        }]
    }
    fn read_file(&self, _path: &str) -> Result<Vec<u8>, ServiceError> {
        unimplemented!("a response streams")
    }
    fn open_file(&self, _path: &str) -> Result<OpenedFile, ServiceError> {
        (self.0)()
    }
}

/// `bytes` zeros for a log `length` long; the reader outrunning the length
/// is a log still being written.
fn opened(length: u64, bytes: u64) -> OpenedFile {
    OpenedFile {
        path: "/logs/big.log".to_owned(),
        length,
        reader: Box::new(std::io::repeat(b'x').take(bytes)),
    }
}

async fn get_log(file_system: OneLog, headers: &[(&str, &str)]) -> axum::response::Response {
    let mut request = Request::builder()
        .uri("/System/Logs/Log?name=BIG.LOG")
        .header("X-Emby-Token", "tok");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    create_router(state_with_file_system(true, Arc::new(file_system)))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_of(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

/// Oracle: Jellyfin 12.4 on a 5000-byte log. `File(stream, type)` sends the
/// length and takes no notice of `Range` or `If-Modified-Since`.
#[tokio::test]
async fn a_log_answers_as_upstreams_file_stream() {
    let response = get_log(
        OneLog(|| Ok(opened(5000, 5000))),
        &[
            ("Range", "bytes=0-9"),
            ("If-Modified-Since", "Fri, 01 Jan 2100 00:00:00 GMT"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(headers["content-length"], "5000");
    assert!(!headers.contains_key("accept-ranges"));
    assert!(!headers.contains_key("last-modified"));
    assert_eq!(body_of(response).await, vec![b'x'; 5000]);
}

/// An empty log is a 200 with a length of zero and an empty body.
#[tokio::test]
async fn an_empty_log_declares_a_length_of_zero() {
    let response = get_log(OneLog(|| Ok(opened(0, 0))), &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "0");
    assert_eq!(body_of(response).await, [] as [u8; 0]);
}

/// The compression layer sizes a response by its `Content-Length`, so streaming
/// the log leaves it as it was: a small one plain, a large one encoded.
#[tokio::test]
async fn a_streamed_log_compresses_as_the_whole_one_did() {
    let gzip = [("Accept-Encoding", "gzip")];
    let large = get_log(OneLog(|| Ok(opened(5000, 5000))), &gzip).await;
    assert_eq!(large.headers()["content-encoding"], "gzip");
    assert!(!large.headers().contains_key("content-length"));
    let small = get_log(OneLog(|| Ok(opened(100, 100))), &gzip).await;
    assert!(!small.headers().contains_key("content-encoding"));
    assert_eq!(small.headers()["content-length"], "100");
    assert_eq!(body_of(small).await.len(), 100);
}

/// Today's log grows while it is read. The body stops at the length the headers
/// declared: going past it would abort the response.
#[tokio::test]
async fn a_log_growing_under_the_reader_stops_at_the_declared_length() {
    let response = get_log(OneLog(|| Ok(opened(300_000, 900_000))), &[]).await;
    assert_eq!(response.headers()["content-length"], "300000");
    assert_eq!(body_of(response).await.len(), 300_000);
}

/// A log is read as the client takes it, whatever its size: only the chunks
/// asked for leave the disk. A log has no size cap.
#[tokio::test]
async fn a_log_is_read_as_the_client_takes_it_not_whole() {
    use axum::body::HttpBody as _;

    static READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Counting;
    impl std::io::Read for Counting {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            READ.fetch_add(buffer.len() as u64, std::sync::atomic::Ordering::SeqCst);
            Ok(buffer.len())
        }
    }

    const HUGE: u64 = 1 << 40;
    let response = get_log(
        OneLog(|| {
            Ok(OpenedFile {
                path: "/logs/big.log".to_owned(),
                length: HUGE,
                reader: Box::new(Counting),
            })
        }),
        &[],
    )
    .await;
    assert_eq!(response.headers()["content-length"], HUGE.to_string());
    let mut body = response.into_body();
    let first = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
        .await
        .expect("a frame")
        .expect("no error");
    assert!(first.is_data());
    assert!(
        READ.load(std::sync::atomic::Ordering::SeqCst) < 1 << 24,
        "read {} bytes for one frame",
        READ.load(std::sync::atomic::Ordering::SeqCst)
    );
}

/// A read that fails mid-file ends the body with an error, so the response is
/// aborted rather than passed off as complete.
#[tokio::test]
async fn a_failed_read_aborts_the_body() {
    struct Failing;
    impl std::io::Read for Failing {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("input/output error"))
        }
    }
    let response = get_log(
        OneLog(|| {
            Ok(OpenedFile {
                path: "/logs/big.log".to_owned(),
                length: 10,
                reader: Box::new(Failing),
            })
        }),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .is_err()
    );
}

/// Oracle: Jellyfin 12.4. A log gone since the listing is a 404, and one that
/// cannot be opened a 500.
#[tokio::test]
async fn a_log_that_cannot_be_opened_answers_as_upstream_does() {
    let gone = get_log(OneLog(|| Err(ServiceError::not_found("gone"))), &[]).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    let refused = get_log(OneLog(|| Err(ServiceError::backend("denied"))), &[]).await;
    assert_eq!(refused.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// `[FromQuery, Required] string name`: a blank value is no value.
#[tokio::test]
async fn a_blank_log_name_is_a_bad_request() {
    let (status, _) = get(elevated_full_state(), "/System/Logs/Log?name=%20").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Listing the log directory and reading a log file run on the blocking pool,
/// never inline on a runtime worker: with the pool's only thread held busy
/// neither request can finish, which an inline read would ignore. A debug
/// server log or a long encode's ffmpeg log runs to hundreds of megabytes.
#[test]
fn log_listing_and_reading_go_through_the_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (busy_tx, busy_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let hog = tokio::task::spawn_blocking(move || {
            busy_tx.send(()).ok();
            release_rx.recv().ok();
        });
        busy_rx.await.unwrap();

        for uri in ["/System/Logs", "/System/Logs/Log?name=ferrofin.log"] {
            let starved = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                get(elevated_full_state(), uri),
            )
            .await;
            assert!(
                starved.is_err(),
                "{uri} answered with the blocking pool starved, so it read inline"
            );
        }

        release_tx.send(()).unwrap();
        hog.await.unwrap();
        let (status, body) = get(elevated_full_state(), "/System/Logs/Log?name=ferrofin.log").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(String::from_utf8(body).unwrap(), "log body");
    });
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

/// `[FromQuery] DateTime? minDate, maxDate` bind through ASP.NET's default
/// binder; EF's `ToUniversalTime` reads an offset-less value as server time.
#[tokio::test]
async fn activity_log_binds_offsetless_and_date_only_bounds_as_server_time() {
    if ferrofin_api::test_support::rerun_in_time_zone(
        "activity_log_binds_offsetless_and_date_only_bounds_as_server_time",
    ) {
        return;
    }
    let activity = Arc::new(StubActivity::default());
    let app = state_with_activity(activity.clone());
    let (status, _) = get(
        app,
        "/System/ActivityLog/Entries?minDate=2026-10-01&maxDate=2026-10-28T12:00:00.5Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let q = activity.last_query.lock().unwrap().clone().unwrap();
    // Midnight in Asia/Kolkata (+05:30).
    assert_eq!(
        q.min_date,
        Some("2026-09-30T18:30:00Z".parse().expect("date"))
    );
    assert_eq!(
        q.max_date,
        Some("2026-10-28T12:00:00.5Z".parse().expect("date"))
    );
}

/// `[FromQuery] ActivityLogSortBy[]? sortBy, SortOrder[]? sortOrder` and
/// `LogLevel? severity`, as Jellyfin 12.2 binds them (measured on the oracle).
#[tokio::test]
async fn activity_log_binds_sorts_and_severity_like_the_model_binder() {
    use ferrofin_traits::activity::{ActivityLogSortBy as By, SortOrder as Dir};
    let bind = |query: &'static str| async move {
        let activity = Arc::new(StubActivity::default());
        let app = state_with_activity(activity.clone());
        let (status, _) = get(app, &format!("/System/ActivityLog/Entries?{query}")).await;
        let last = activity.last_query.lock().unwrap().clone();
        (status, last)
    };
    for (query, expected) in [
        (
            "sortBy=Type&sortBy=name",
            vec![(By::Type, Dir::Ascending), (By::Name, Dir::Ascending)],
        ),
        // Numbers are the C# values (DateCreated = 5, Overiew = 1).
        (
            "sortBy=5&sortBy=1&sortOrder=1",
            vec![
                (By::DateCreated, Dir::Descending),
                (By::Overview, Dir::Descending),
            ],
        ),
        ("sortBy=OVERIEW", vec![(By::Overview, Dir::Ascending)]),
        // `GetOrderBy` pads with the FIRST order.
        (
            "sortBy=Username&sortBy=LogSeverity&sortBy=Name&sortOrder=Descending&sortOrder=Ascending",
            vec![
                (By::Username, Dir::Descending),
                (By::LogSeverity, Dir::Ascending),
                (By::Name, Dir::Descending),
            ],
        ),
    ] {
        let (status, last) = bind(query).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(last.unwrap().order_by, expected, "{query}");
    }
    for query in [
        "sortBy=4",
        "sortBy=Bogus",
        "sortBy=",
        "sortBy=Name&sortOrder=bogus",
        "sortBy=Name&sortOrder=2",
    ] {
        let (status, _) = bind(query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    // `NullableEnumModelBinder`: an undefined number is kept, a bad name is null.
    let (_, last) = bind("severity=99").await;
    assert_eq!(last.unwrap().severity, Some(LogLevel::Unrecognized(99)));
    let (_, last) = bind("severity=bogus").await;
    assert_eq!(last.unwrap().severity, None);
    let (_, last) = bind("severity=WARNING").await;
    assert_eq!(last.unwrap().severity, Some(LogLevel::Warning));
}

#[tokio::test]
async fn dashboard_pages_empty_and_page_not_found() {
    let (pages, pb) = get(elevated_full_state(), "/web/ConfigurationPages").await;
    assert_eq!(pages, StatusCode::OK);
    let page_list = json(&pb);
    assert!(page_list.as_array().unwrap().is_empty(), "{page_list}");

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
