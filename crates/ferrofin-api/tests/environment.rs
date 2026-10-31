//! Environment handler tests: directory-contents browse, path validation, and
//! drives / parent-path / default-browser / network-shares.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned filesystem
//! data. Managers a given handler never touches reuse the `test_support` panic
//! fakes.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::{AppState, MAX_BLOCKING_FILE_SYSTEM_CALLS};
use ferrofin_api::test_support::{
    ApiKeyAuthService, AuthedAuthService, FakeApiKeys, FakeAppHost, FakeClientEventLogger,
    FakeCollections, FakeDevices, FakeDisplayPreferences, FakeDto, FakeLibrary, FakeLyrics,
    FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeProviders, FakeQuickConnect,
    FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeTasks, FakeTrickplay,
    FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers, setup_complete_state,
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
        normalized_username: "BOB".to_owned(),
    }
}

/// An auth stub that authenticates as [`USER_ID`].
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".into()),
            user: Some(user()),
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
        vec![
            FileSystemEntryInfo {
                name: "movies".to_owned(),
                path: "/media/movies".to_owned(),
                type_: FileSystemEntryType::Directory,
            },
            FileSystemEntryInfo {
                name: "readme.txt".to_owned(),
                path: "/media/readme.txt".to_owned(),
                type_: FileSystemEntryType::File,
            },
        ]
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

/// Builds an [`AppState`] whose filesystem/system/config/activity managers are
/// stubs and the rest are panic fakes.
fn full_state() -> AppState {
    state_with_file_system(Arc::new(StubFileSystem))
}

/// [`full_state`] over another filesystem.
fn state_with_file_system(file_system: Arc<dyn FileSystem>) -> AppState {
    let auth = Arc::new(OkAuth);
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
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(StubActivity::default()),
        file_system,
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
async fn environment_directory_contents_filters_and_orders() {
    // Only directories.
    let (status, body) = get(
        full_state(),
        "/Environment/DirectoryContents?path=/media&includeDirectories=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["Name"], "movies");

    // Both files and directories.
    let (_, both) = get(
        full_state(),
        "/Environment/DirectoryContents?path=/media&includeFiles=true&includeDirectories=true",
    )
    .await;
    assert_eq!(json(&both).as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn environment_validate_path_existing_and_missing() {
    let (ok, _) = send(
        full_state(),
        "POST",
        "/Environment/ValidatePath",
        Body::from(r#"{"Path":"/exists","IsFile":false,"ValidateWritable":false}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(ok, StatusCode::NO_CONTENT);

    let (missing, _) = send(
        full_state(),
        "POST",
        "/Environment/ValidatePath",
        Body::from(r#"{"Path":"/nope","IsFile":false,"ValidateWritable":false}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn environment_drives_parent_and_default_browser() {
    let (d, db) = get(full_state(), "/Environment/Drives").await;
    assert_eq!(d, StatusCode::OK);
    assert_eq!(json(&db)[0]["Name"], "/");

    let (p, pb) = get(full_state(), "/Environment/ParentPath?path=/a/b/c").await;
    assert_eq!(p, StatusCode::OK);
    assert_eq!(json(&pb), "/a/b");

    let (dd, ddb) = get(full_state(), "/Environment/DefaultDirectoryBrowser").await;
    assert_eq!(dd, StatusCode::OK);
    // Path is null (absent) by default.
    assert!(json(&ddb).get("Path").is_none());

    let (ns, nsb) = get(full_state(), "/Environment/NetworkShares").await;
    assert_eq!(ns, StatusCode::OK);
    let shares = json(&nsb);
    assert!(shares.as_array().unwrap().is_empty(), "{shares}");
}

#[tokio::test]
async fn environment_routes_require_an_administrator_once_setup_is_complete() {
    // Upstream gates the whole controller `FirstTimeSetupOrElevated`
    // (EnvironmentController.cs:20): after the wizard, browsing and probing the
    // server's filesystem is for administrators only.
    let routes = [
        ("GET", "/Environment/DirectoryContents?path=/", ""),
        ("POST", "/Environment/ValidatePath", r#"{"Path":"/"}"#),
        ("GET", "/Environment/Drives", ""),
        ("GET", "/Environment/NetworkShares", ""),
        ("GET", "/Environment/ParentPath?path=/a/b", ""),
        ("GET", "/Environment/DefaultDirectoryBrowser", ""),
    ];
    for (method, uri, body) in routes {
        let state = setup_complete_state(Arc::new(AuthedAuthService));
        let (status, _) = send(
            state,
            method,
            uri,
            Body::from(body),
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // An administrator still gets through (an API key is elevated upstream).
    let (status, body) = get(
        setup_complete_state(Arc::new(ApiKeyAuthService)),
        "/Environment/NetworkShares",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let shares = json(&body);
    assert!(shares.as_array().unwrap().is_empty(), "{shares}");
}

/// Runs `body` on a current-thread runtime whose blocking pool has one thread,
/// held busy until `body` returns. A filesystem call made inline on the only
/// runtime worker ignores that hog and answers; one on the pool waits for it.
fn with_blocking_pool_starved(body: impl AsyncFnOnce()) {
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
        body().await;
        release_tx.send(()).unwrap();
        hog.await.unwrap();
        let (status, _) = get(full_state(), "/Environment/Drives").await;
        assert_eq!(status, StatusCode::OK, "the pool's thread is back");
    });
}

/// The browse handlers never run the filesystem inline on a runtime worker: a
/// `statvfs` or a `read_dir` on a hung network mount would pin that worker in
/// uninterruptible sleep, and one call per core stops all HTTP traffic.
#[test]
fn environment_filesystem_calls_go_through_the_blocking_pool() {
    with_blocking_pool_starved(async || {
        for uri in [
            "/Environment/Drives",
            "/Environment/DirectoryContents?path=/media&includeFiles=true",
        ] {
            let starved = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                get(full_state(), uri),
            )
            .await;
            assert!(starved.is_err(), "{uri} answered inline");
        }
        for body in [
            r#"{"Path":"/exists","IsFile":false}"#,
            r#"{"Path":"/exists/file.txt","IsFile":true}"#,
            r#"{"Path":"/exists","ValidateWritable":true}"#,
        ] {
            let starved = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                send(
                    full_state(),
                    "POST",
                    "/Environment/ValidatePath",
                    Body::from(body),
                    Some("application/json"),
                ),
            )
            .await;
            assert!(starved.is_err(), "ValidatePath {body} answered inline");
        }
    });
}

/// A filesystem answering its listing and its writable check from a script,
/// recording the path it was asked to list.
struct ScriptedFs {
    listing: fn() -> Result<Vec<FileSystemEntryInfo>, ServiceError>,
    writable: fn() -> Result<(), ServiceError>,
    listed: Mutex<Option<String>>,
}

impl ScriptedFs {
    fn listing(listing: fn() -> Result<Vec<FileSystemEntryInfo>, ServiceError>) -> Arc<Self> {
        Arc::new(Self {
            listing,
            writable: || Ok(()),
            listed: Mutex::new(None),
        })
    }

    fn writable(writable: fn() -> Result<(), ServiceError>) -> Arc<Self> {
        Arc::new(Self {
            listing: || Ok(Vec::new()),
            writable,
            listed: Mutex::new(None),
        })
    }
}

impl FileSystem for ScriptedFs {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        *self.listed.lock().unwrap() = Some(path.to_owned());
        (self.listing)()
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        Vec::new()
    }
    fn file_exists(&self, _path: &str) -> bool {
        false
    }
    fn directory_exists(&self, _path: &str) -> bool {
        true
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        (self.writable)()
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        Vec::new()
    }
    fn read_file(&self, _path: &str) -> Result<Vec<u8>, ServiceError> {
        Err(ServiceError::not_found("no file"))
    }
}

fn entry(path: &str) -> FileSystemEntryInfo {
    FileSystemEntryInfo {
        name: path.rsplit('/').next().unwrap_or(path).to_owned(),
        path: path.to_owned(),
        type_: FileSystemEntryType::File,
    }
}

/// `OrderBy(i => i.FullName)` is `string.CompareTo`, the current culture's order.
/// Oracle: Jellyfin 12.4 listing these five names.
#[tokio::test]
async fn directory_contents_order_by_the_culture_not_by_bytes() {
    let fs = ScriptedFs::listing(|| {
        Ok(["/d/Z", "/d/A.txt", "/d/b", "/d/a.txt", "/d/_x"]
            .map(entry)
            .into())
    });
    let (status, body) = get(
        state_with_file_system(fs),
        "/Environment/DirectoryContents?path=/d&includeFiles=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<String> = json(&body)
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["Name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["_x", "a.txt", "A.txt", "b", "Z"]);
}

/// Oracle: Jellyfin 12.4. A missing path and a path that is a file are
/// `DirectoryNotFoundException`, a 404; a directory the server may not read
/// lists empty (`IgnoreInaccessible`); anything else is a 500.
#[tokio::test]
async fn directory_contents_failures_answer_as_upstream_does() {
    let uri = "/Environment/DirectoryContents?path=/d&includeFiles=true";

    let gone = ScriptedFs::listing(|| Err(ServiceError::not_found("no directory")));
    let (status, _) = get(state_with_file_system(gone), uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let shut = ScriptedFs::listing(|| Err(ServiceError::Forbidden("permission denied".into())));
    let (status, body) = get(state_with_file_system(shut), uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body), serde_json::json!([]));

    let broken = ScriptedFs::listing(|| Err(ServiceError::backend("input/output error")));
    let (status, _) = get(state_with_file_system(broken), uri).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// `new DirectoryInfo(path)` runs `Path.GetFullPath`, so the entries come back
/// under the resolved path.
#[tokio::test]
async fn directory_contents_lists_the_full_path() {
    let fs = ScriptedFs::listing(|| Ok(Vec::new()));
    let uri = "/Environment/DirectoryContents?path=/media/movies/../tv/./";
    let (status, _) = get(state_with_file_system(Arc::clone(&fs) as _), uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fs.listed.lock().unwrap().as_deref(), Some("/media/tv"));
}

/// `[FromQuery, Required] string path`: the binder nulls a blank value, so
/// whitespace is the same 400 as no value at all. A flag must be a boolean.
#[tokio::test]
async fn directory_contents_rejects_what_the_binder_rejects() {
    for uri in [
        "/Environment/DirectoryContents",
        "/Environment/DirectoryContents?path=",
        "/Environment/DirectoryContents?path=%20",
        "/Environment/DirectoryContents?path=/d&includeFiles=maybe",
    ] {
        let (status, _) = get(full_state(), uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

/// Oracle: Jellyfin 12.4. A read-only directory is a 500, a path that is a file
/// a 404 (`DirectoryNotFoundException` from the write under it), and the `IsFile`
/// branches never try to write.
#[tokio::test]
async fn validate_path_writable_failures_answer_as_upstream_does() {
    let writable_body = r#"{"Path":"/d","ValidateWritable":true}"#;
    let post = async |fs: Arc<ScriptedFs>, body: &'static str| {
        send(
            state_with_file_system(fs),
            "POST",
            "/Environment/ValidatePath",
            Body::from(body),
            Some("application/json"),
        )
        .await
    };

    let read_only = ScriptedFs::writable(|| Err(ServiceError::backend("read-only")));
    let (status, _) = post(read_only, writable_body).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    let under_a_file = ScriptedFs::writable(|| Err(ServiceError::not_found("not a directory")));
    let (status, _) = post(under_a_file, writable_body).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let never_written = ScriptedFs::writable(|| panic!("the IsFile branch never writes"));
    let (status, _) = post(
        never_written,
        r#"{"Path":"/d","IsFile":false,"ValidateWritable":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// `NotFound()`: the ProblemDetails body, not a message.
#[tokio::test]
async fn validate_path_missing_is_upstreams_problem_details() {
    let (status, body) = send(
        full_state(),
        "POST",
        "/Environment/ValidatePath",
        Body::from(r#"{"Path":"/nope"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["title"], "Not Found");
    assert_eq!(json(&body)["status"], 404);
}

/// A listing that sleeps until released, as one on a hung mount does.
#[derive(Default)]
struct HungListingFs {
    in_listing: AtomicUsize,
    released: Mutex<bool>,
    release: std::sync::Condvar,
}

impl HungListingFs {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl FileSystem for HungListingFs {
    fn get_file_system_entries(&self, _path: &str) -> Vec<FileSystemEntryInfo> {
        unimplemented!("the handler tells a failed listing from an empty one")
    }
    fn try_get_file_system_entries(
        &self,
        _path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        self.in_listing.fetch_add(1, Ordering::SeqCst);
        let released = self.released.lock().unwrap();
        drop(self.release.wait_while(released, |released| !*released));
        self.in_listing.fetch_sub(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        vec![entry("/")]
    }
    fn file_exists(&self, _path: &str) -> bool {
        false
    }
    fn directory_exists(&self, _path: &str) -> bool {
        false
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        Vec::new()
    }
    fn read_file(&self, _path: &str) -> Result<Vec<u8>, ServiceError> {
        Err(ServiceError::not_found("no file"))
    }
}

/// Clicks on a hung mount cannot use up the blocking pool: past the cap they
/// wait without a thread, and the drive list still opens, which is how the
/// admin picks another path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hung_listings_hold_no_more_threads_than_the_cap() {
    /// Lets the hung calls go whatever the test's outcome: the runtime waits
    /// for its blocking threads when it is dropped.
    struct ReleaseAtTheEnd(Arc<HungListingFs>);
    impl Drop for ReleaseAtTheEnd {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    let fs = Arc::new(HungListingFs::default());
    let release = ReleaseAtTheEnd(Arc::clone(&fs));
    let app = state_with_file_system(Arc::clone(&fs) as _);
    let uri = "/Environment/DirectoryContents?path=/mnt/nas&includeFiles=true";
    let requests: Vec<_> = (0..MAX_BLOCKING_FILE_SYSTEM_CALLS * 2)
        .map(|_| tokio::spawn(get(app.clone(), uri)))
        .collect();

    let give_up = tokio::time::Instant::now() + Duration::from_secs(30);
    while fs.in_listing.load(Ordering::SeqCst) < MAX_BLOCKING_FILE_SYSTEM_CALLS {
        assert!(tokio::time::Instant::now() < give_up, "calls never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fs.in_listing.load(Ordering::SeqCst),
        MAX_BLOCKING_FILE_SYSTEM_CALLS
    );

    let (status, _) = tokio::time::timeout(
        Duration::from_secs(30),
        get(app.clone(), "/Environment/Drives"),
    )
    .await
    .expect("the drive list must not wait for a slot");
    assert_eq!(status, StatusCode::OK);

    drop(release);
    for request in requests {
        let (status, _) = request.await.unwrap();
        assert_eq!(status, StatusCode::OK);
    }
}
