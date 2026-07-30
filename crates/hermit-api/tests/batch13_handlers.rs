//! Batch-13 handler success/failure-path tests: System admin + Configuration +
//! Branding + Localization + DisplayPreferences + ActivityLog + Dashboard +
//! Environment + TimeSync.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `hermit-traits` impls that authenticate and return canned data. Managers
//! a given handler never touches reuse the `test_support` panic fakes.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hermit_api::create_router;
use hermit_api::state::AppState;
use hermit_api::test_support::{
    FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
    FakeDisplayPreferences, FakeDto, FakeLibrary, FakeLyrics, FakeMediaSegments, FakeMediaSources,
    FakeMusic, FakePlaylists, FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSubtitles, FakeTasks, FakeTrickplay, FakeTvSeries, FakeUserData,
    FakeUserViews, FakeUsers,
};
use hermit_db::entities::display_preferences::{
    DisplayPreferencesEntity, ItemDisplayPreferencesEntity,
};
use hermit_db::entities::users::UserEntity;
use hermit_model::activity::{ActivityLogEntry, LogLevel};
use hermit_model::branding::BrandingOptions;
use hermit_model::configuration::ServerConfiguration;
use hermit_model::entities_media::{ParentalRating, ParentalRatingScore};
use hermit_model::globalization::{CountryInfo, CultureDto, LocalizationOption};
use hermit_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use hermit_model::querying::QueryResult;
use hermit_model::system::{FolderStorageInfo, PublicSystemInfo, SystemInfo, SystemStorageInfo};
use hermit_traits::activity::{ActivityLogQuery, ActivityManager};
use hermit_traits::configuration::ServerConfigurationManager;
use hermit_traits::error::ServiceError;
use hermit_traits::filesystem::{FileMetadata, FileSystem};
use hermit_traits::localization::LocalizationManager;
use hermit_traits::net::{AuthService, AuthorizationContext, RequestContext};
use hermit_traits::options::AuthorizationInfo;
use hermit_traits::system::{ServerApplicationPaths, SystemManager};
use std::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00D1_0000);

/// A minimal authenticated user.
fn user() -> UserEntity {
    let mut u: UserEntity = serde_default_user();
    u.id = USER_ID.to_string();
    u
}

/// Builds a `UserEntity` with all-default fields via JSON (the struct is large;
/// only `id` matters for these tests).
fn serde_default_user() -> UserEntity {
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
        normalized_username: String::new(),
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
struct OkAuth;

#[async_trait]
impl AuthService for OkAuth {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(AuthorizationInfo {
            token: Some("tok".to_owned()),
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
            token: Some("tok".to_owned()),
            user: Some(user()),
            is_authenticated: true,
            ..Default::default()
        })
    }
}

/// A configuration manager returning canned config + branding, capturing writes.
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
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(ServerConfiguration {
            server_name: "Hermit".to_owned(),
            ..Default::default()
        })
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

/// A localization manager returning one culture/country/rating/option.
struct StubLocalization;

impl LocalizationManager for StubLocalization {
    fn get_cultures(&self) -> Vec<CultureDto> {
        vec![
            CultureDto {
                name: "en".to_owned(),
                display_name: "English".to_owned(),
                two_letter_iso_language_name: "en".to_owned(),
                three_letter_iso_language_name: Some("eng".to_owned()),
                three_letter_iso_language_names: vec!["eng".to_owned()],
            },
            // A duplicate display name to prove de-duplication.
            CultureDto {
                name: "en2".to_owned(),
                display_name: "English".to_owned(),
                two_letter_iso_language_name: "en".to_owned(),
                three_letter_iso_language_name: Some("eng".to_owned()),
                three_letter_iso_language_names: vec!["eng".to_owned()],
            },
            CultureDto {
                name: "de".to_owned(),
                display_name: "German".to_owned(),
                two_letter_iso_language_name: "de".to_owned(),
                three_letter_iso_language_name: Some("deu".to_owned()),
                three_letter_iso_language_names: vec!["deu".to_owned()],
            },
        ]
    }
    fn get_countries(&self) -> Vec<CountryInfo> {
        vec![CountryInfo {
            name: "US".to_owned(),
            display_name: "United States".to_owned(),
            two_letter_iso_region_name: "US".to_owned(),
            three_letter_iso_region_name: "USA".to_owned(),
        }]
    }
    fn get_parental_ratings(&self) -> Vec<ParentalRating> {
        vec![ParentalRating::new(
            "PG-13".to_owned(),
            Some(ParentalRatingScore::new(13, None)),
        )]
    }
    fn get_localization_options(&self) -> Vec<LocalizationOption> {
        vec![LocalizationOption {
            name: "English".to_owned(),
            value: "en-US".to_owned(),
        }]
    }
    fn get_rating_score(
        &self,
        _rating: &str,
        _country_code: Option<&str>,
    ) -> Option<ParentalRatingScore> {
        None
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
        _entry: hermit_traits::activity::ActivityLogCreate,
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
            name: "hermit.log".to_owned(),
            full_name: "/logs/hermit.log".to_owned(),
            length: 42,
            date_created: chrono::Utc::now(),
            date_modified: chrono::Utc::now(),
        }]
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        if path == "/logs/hermit.log" {
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

/// Builds an [`AppState`] with the given managers under test, defaulting the
/// rest to the shared panic fakes.
#[allow(clippy::too_many_arguments)]
fn state(
    localization: Arc<dyn LocalizationManager>,
    activity: Arc<dyn ActivityManager>,
    file_system: Arc<dyn FileSystem>,
    config: Arc<dyn ServerConfigurationManager>,
    system: Arc<dyn SystemManager>,
) -> AppState {
    let auth = Arc::new(OkAuth);
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        system,
        Arc::new(FakeAppHost),
        config,
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
        localization,
        Arc::new(FakeDisplayPreferences),
        activity,
        file_system,
        Arc::new(FakeTasks),
    )
}

/// The default state: every batch-13 manager is a stub.
fn full_state() -> AppState {
    state(
        Arc::new(StubLocalization),
        Arc::new(StubActivity::default()),
        Arc::new(StubFileSystem),
        Arc::new(StubConfig::default()),
        Arc::new(StubSystem),
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
async fn localization_cultures_are_distinct_and_ordered() {
    let (status, body) = get(full_state(), "/Localization/Cultures").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    let arr = v.as_array().unwrap();
    // "English" (duplicated) collapses to one; ordered English < German.
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["DisplayName"], "English");
    assert_eq!(arr[1]["DisplayName"], "German");
}

#[tokio::test]
async fn localization_countries_ratings_options() {
    let (s1, b1) = get(full_state(), "/Localization/Countries").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(json(&b1)[0]["TwoLetterISORegionName"], "US");

    let (s2, b2) = get(full_state(), "/Localization/ParentalRatings").await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(json(&b2)[0]["Name"], "PG-13");

    let (s3, b3) = get(full_state(), "/Localization/Options").await;
    assert_eq!(s3, StatusCode::OK);
    assert_eq!(json(&b3)[0]["Value"], "en-US");
}

#[tokio::test]
async fn branding_configuration_and_css() {
    // Seed a branding config with custom CSS.
    let config = Arc::new(StubConfig {
        branding: Mutex::new(BrandingOptions {
            custom_css: Some("body{color:red}".to_owned()),
            splashscreen_enabled: true,
            splashscreen_location: Some("/secret.png".to_owned()),
            login_disclaimer: Some("Hi".to_owned()),
        }),
        paths: StubPaths::default(),
    });
    let app = state(
        Arc::new(StubLocalization),
        Arc::new(StubActivity::default()),
        Arc::new(StubFileSystem),
        config,
        Arc::new(StubSystem),
    );

    let (status, body) = get(app.clone(), "/Branding/Configuration").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["CustomCss"], "body{color:red}");
    assert_eq!(v["SplashscreenEnabled"], true);
    // The DTO must not leak the splashscreen location.
    assert!(v.get("SplashscreenLocation").is_none());

    let (css_status, css_body) = get(app, "/Branding/Css").await;
    assert_eq!(css_status, StatusCode::OK);
    assert_eq!(String::from_utf8(css_body).unwrap(), "body{color:red}");
}

#[tokio::test]
async fn update_branding_preserves_splashscreen_location() {
    let config = Arc::new(StubConfig {
        branding: Mutex::new(BrandingOptions {
            splashscreen_location: Some("/keep.png".to_owned()),
            ..Default::default()
        }),
        paths: StubPaths::default(),
    });
    let app = state(
        Arc::new(StubLocalization),
        Arc::new(StubActivity::default()),
        Arc::new(StubFileSystem),
        config.clone(),
        Arc::new(StubSystem),
    );

    let (status, _) = send(
        app,
        "POST",
        "/System/Configuration/Branding",
        Body::from(r#"{"LoginDisclaimer":"New","SplashscreenEnabled":true}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let saved = config.branding.lock().unwrap().clone();
    assert_eq!(saved.login_disclaimer.as_deref(), Some("New"));
    assert!(saved.splashscreen_enabled);
    // The pre-existing splashscreen location must be preserved.
    assert_eq!(saved.splashscreen_location.as_deref(), Some("/keep.png"));
}

#[tokio::test]
async fn configuration_read_returns_server_config() {
    let (status, body) = get(full_state(), "/System/Configuration").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["ServerName"], "Hermit");
}

#[tokio::test]
async fn named_configuration_branding_and_unknown() {
    let (s1, b1) = get(full_state(), "/System/Configuration/branding").await;
    assert_eq!(s1, StatusCode::OK);
    // Branding serializes as an object.
    assert!(json(&b1).is_object());

    let (s2, _) = get(full_state(), "/System/Configuration/unknownkey").await;
    assert_eq!(s2, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn system_ping_returns_name() {
    let (status, body) = get(full_state(), "/System/Ping").await;
    assert_eq!(status, StatusCode::OK);
    // FakeAppHost's friendly name.
    assert_eq!(json(&body), "hermit-test");
}

#[tokio::test]
async fn system_storage_projects_dto() {
    let (status, body) = get(full_state(), "/System/Info/Storage").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["ProgramDataFolder"]["FreeSpace"], 100);
    assert_eq!(v["ProgramDataFolder"]["Path"], "/data");
}

#[tokio::test]
async fn system_logs_list_and_fetch() {
    let (status, body) = get(full_state(), "/System/Logs").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v[0]["Name"], "hermit.log");
    assert_eq!(v[0]["Size"], 42);

    let (fetch_status, fetch_body) = get(full_state(), "/System/Logs/Log?name=hermit.log").await;
    assert_eq!(fetch_status, StatusCode::OK);
    assert_eq!(String::from_utf8(fetch_body).unwrap(), "log body");

    let (missing_status, _) = get(full_state(), "/System/Logs/Log?name=absent.log").await;
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
    let (r, _) = send(full_state(), "POST", "/System/Restart", Body::empty(), None).await;
    assert_eq!(r, StatusCode::NO_CONTENT);
    let (s, _) = send(
        full_state(),
        "POST",
        "/System/Shutdown",
        Body::empty(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn activity_log_binds_query_and_returns_entries() {
    let activity = Arc::new(StubActivity::default());
    let app = state(
        Arc::new(StubLocalization),
        activity.clone(),
        Arc::new(StubFileSystem),
        Arc::new(StubConfig::default()),
        Arc::new(StubSystem),
    );
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
    assert!(json(&nsb).as_array().unwrap().is_empty());
}

#[tokio::test]
async fn dashboard_pages_empty_and_page_not_found() {
    let (pages, pb) = get(full_state(), "/web/ConfigurationPages").await;
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

/// A display-preferences manager returning a canned row + item prefs, capturing
/// the last saved row.
#[derive(Default)]
struct StubDisplayPreferences {
    row: Mutex<Option<DisplayPreferencesEntity>>,
    item: Mutex<Option<ItemDisplayPreferencesEntity>>,
    custom: Mutex<Option<std::collections::HashMap<String, Option<String>>>>,
}

fn canned_prefs() -> DisplayPreferencesEntity {
    DisplayPreferencesEntity {
        id: 1,
        chromecast_version: 1,
        client: "web".to_owned(),
        dashboard_theme: Some("dark".to_owned()),
        enable_next_video_info_overlay: true,
        index_by: Some(1),
        item_id: "11111111-1111-1111-1111-111111111111".to_owned(),
        scroll_direction: 1,
        show_backdrop: true,
        show_sidebar: false,
        skip_backward_length: 10000,
        skip_forward_length: 30000,
        tv_home: Some("mytv".to_owned()),
        user_id: USER_ID.to_string(),
    }
}

fn canned_item_prefs() -> ItemDisplayPreferencesEntity {
    ItemDisplayPreferencesEntity {
        id: 1,
        client: "web".to_owned(),
        index_by: None,
        item_id: "11111111-1111-1111-1111-111111111111".to_owned(),
        remember_indexing: true,
        remember_sorting: false,
        sort_by: "SortName".to_owned(),
        sort_order: 1,
        user_id: USER_ID.to_string(),
        view_type: 0,
    }
}

#[async_trait]
impl hermit_traits::configuration::DisplayPreferencesManager for StubDisplayPreferences {
    async fn get_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<DisplayPreferencesEntity, ServiceError> {
        Ok(canned_prefs())
    }
    async fn get_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<ItemDisplayPreferencesEntity, ServiceError> {
        Ok(canned_item_prefs())
    }
    async fn list_item_display_preferences(
        &self,
        _user_id: Uuid,
        _client: &str,
    ) -> Result<Vec<ItemDisplayPreferencesEntity>, ServiceError> {
        Ok(vec![canned_item_prefs()])
    }
    async fn list_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
    ) -> Result<std::collections::HashMap<String, Option<String>>, ServiceError> {
        let mut m = std::collections::HashMap::new();
        m.insert("customKey".to_owned(), Some("customVal".to_owned()));
        Ok(m)
    }
    async fn set_custom_item_display_preferences(
        &self,
        _user_id: Uuid,
        _item_id: Uuid,
        _client: &str,
        custom_preferences: &std::collections::HashMap<String, Option<String>>,
    ) -> Result<(), ServiceError> {
        *self.custom.lock().unwrap() = Some(custom_preferences.clone());
        Ok(())
    }
    async fn update_display_preferences(
        &self,
        display_preferences: &DisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        *self.row.lock().unwrap() = Some(display_preferences.clone());
        Ok(())
    }
    async fn update_item_display_preferences(
        &self,
        item_display_preferences: &ItemDisplayPreferencesEntity,
    ) -> Result<(), ServiceError> {
        *self.item.lock().unwrap() = Some(item_display_preferences.clone());
        Ok(())
    }
}

/// Builds an [`AppState`] whose display-preferences manager is `prefs`.
fn state_with_display_prefs(
    prefs: Arc<dyn hermit_traits::configuration::DisplayPreferencesManager>,
) -> AppState {
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
        Arc::new(StubLocalization),
        prefs,
        Arc::new(StubActivity::default()),
        Arc::new(StubFileSystem),
        Arc::new(FakeTasks),
    )
}

#[tokio::test]
async fn display_preferences_get_folds_scalars_into_custom_prefs() {
    let app = state_with_display_prefs(Arc::new(StubDisplayPreferences::default()));
    let (status, body) = get(app, "/DisplayPreferences/home?client=web").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    // Scalar row fields surface under CustomPrefs.
    assert_eq!(v["CustomPrefs"]["chromecastVersion"], "unstable");
    assert_eq!(v["CustomPrefs"]["skipForwardLength"], "30000");
    assert_eq!(v["CustomPrefs"]["dashboardTheme"], "dark");
    // Stored custom prefs are carried through.
    assert_eq!(v["CustomPrefs"]["customKey"], "customVal");
    // IndexBy discriminant (1) → ProductionYear; scroll direction (1) → Vertical.
    assert_eq!(v["IndexBy"], "ProductionYear");
    assert_eq!(v["ScrollDirection"], "Vertical");
    assert_eq!(v["SortOrder"], "Descending");
}

#[tokio::test]
async fn display_preferences_post_parses_scalars_back() {
    let prefs = Arc::new(StubDisplayPreferences::default());
    let app = state_with_display_prefs(prefs.clone());
    let body = r#"{
        "CustomPrefs": {
            "chromecastVersion": "stable",
            "skipForwardLength": "5000",
            "skipBackLength": "5000",
            "enableNextVideoInfoOverlay": "false",
            "dashboardTheme": "light",
            "tvhome": "grid",
            "homesection0": "resume",
            "keepMe": "yes"
        },
        "SortBy": "DateCreated",
        "SortOrder": "Ascending",
        "ScrollDirection": "Horizontal",
        "ShowBackdrop": false,
        "ShowSidebar": true,
        "RememberIndexing": false,
        "RememberSorting": true,
        "PrimaryImageHeight": 250,
        "PrimaryImageWidth": 250
    }"#;
    let (status, _) = send(
        app,
        "POST",
        "/DisplayPreferences/home?client=web",
        Body::from(body),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let saved = prefs.row.lock().unwrap().clone().unwrap();
    assert_eq!(saved.chromecast_version, 0); // stable
    assert_eq!(saved.skip_forward_length, 5000);
    assert!(!saved.enable_next_video_info_overlay);
    assert_eq!(saved.dashboard_theme.as_deref(), Some("light"));
    assert_eq!(saved.tv_home.as_deref(), Some("grid"));
    assert!(!saved.show_backdrop);
    assert!(saved.show_sidebar);

    let item = prefs.item.lock().unwrap().clone().unwrap();
    assert_eq!(item.sort_by, "DateCreated");
    assert_eq!(item.sort_order, 0); // Ascending

    // The scalar + homesection keys are stripped; only "keepMe" remains custom.
    let custom = prefs.custom.lock().unwrap().clone().unwrap();
    assert!(custom.contains_key("keepMe"));
    assert!(!custom.contains_key("chromecastVersion"));
    assert!(!custom.contains_key("homesection0"));
}

#[tokio::test]
async fn unauthenticated_system_configuration_is_401() {
    // No token header → RequireAuth rejects. The `full_state` auth stub always
    // authenticates, so use a router whose auth service rejects: reuse the
    // shared fake state (its FakeAuthService rejects).
    let app = hermit_api::test_support::fake_state();
    let response = create_router(app)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/System/Configuration")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
