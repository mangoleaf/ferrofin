//! System-configuration handler tests: read the full config, read a named config
//! section, and the unauthenticated-rejection path.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned config.
//! Managers a given handler never touches reuse the `test_support` panic fakes.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAdminUsers, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
    FakeDisplayPreferences, FakeDto, FakeLibrary, FakeLyrics, FakeMediaSegments, FakeMediaSources,
    FakeMusic, FakePlaylists, FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions,
    FakeSimilarItems, FakeSubtitles, FakeTasks, FakeTrickplay, FakeTvSeries, FakeUserData,
    FakeUserViews, FakeUsers,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::activity::{ActivityLogEntry, LogLevel};
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::entities_media::{ParentalRating, ParentalRatingScore};
use ferrofin_model::globalization::{CountryInfo, CultureDto, LocalizationOption};
use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::system::{FolderStorageInfo, PublicSystemInfo, SystemInfo, SystemStorageInfo};
use ferrofin_traits::activity::{ActivityLogQuery, ActivityManager};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::localization::LocalizationManager;
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

/// A configuration manager returning canned config + branding, capturing writes.
#[derive(Default)]
struct StubConfig {
    updates: Mutex<Vec<(String, Arc<str>)>>,
    branding: Mutex<BrandingOptions>,
    configuration: Mutex<Option<Arc<ServerConfiguration>>>,
    paths: StubPaths,
}

/// Application paths returning a fixed log directory.
#[derive(Default, Clone)]
struct StubPaths {
    log_dir: String,
    config_dir: String,
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
        self.config_dir.clone()
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
    fn named_configuration_updated(&self, key: &str, json: Arc<str>) {
        let saved = std::fs::read(
            self.paths.config_dir.clone() + "/named/" + &key.to_ascii_lowercase() + ".json",
        )
        .unwrap();
        assert_eq!(json.as_bytes(), saved);
        self.updates.lock().unwrap().push((key.to_owned(), json));
    }

    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(self.paths.clone())
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(self
            .configuration
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                Arc::new(ServerConfiguration {
                    server_name: "Ferrofin".to_owned(),
                    ..Default::default()
                })
            }))
    }
    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        *self.configuration.lock().unwrap() = Some(Arc::new(configuration.clone()));
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
        vec![CultureDto {
            name: "en".to_owned(),
            display_name: "English".to_owned(),
            two_letter_iso_language_name: "en".to_owned(),
            three_letter_iso_language_name: Some("eng".to_owned()),
            three_letter_iso_language_names: vec!["eng".to_owned()],
        }]
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
    fn get_localized_string(&self, phrase: &str) -> String {
        phrase.to_owned()
    }
    fn get_localized_string_for(&self, phrase: &str, _culture: &str) -> String {
        phrase.to_owned()
    }
    fn get_language_display_name(&self, _language: &str) -> Option<String> {
        None
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

/// Builds an [`AppState`] whose batch-13 managers are all stubs.
fn full_state() -> AppState {
    state_with_paths(StubPaths::default())
}

/// Builds the same [`AppState`] with the given application paths, so a test can
/// point the named-configuration store at a real directory.
fn state_with_paths(paths: StubPaths) -> AppState {
    state_with(paths, Arc::new(FakeUsers))
}

/// [`state_with_paths`] whose caller is an administrator, as the elevated
/// `POST /System/Configuration/{key}` requires.
fn admin_state_with_paths(paths: StubPaths) -> AppState {
    state_with(paths, Arc::new(FakeAdminUsers))
}

/// [`state_with_paths`] with the caller's role decided by `users`.
fn state_with(paths: StubPaths, users: Arc<dyn UserManager>) -> AppState {
    state_with_config(
        users,
        Arc::new(StubConfig {
            paths,
            ..Default::default()
        }),
    )
}

fn state_with_config(
    users: Arc<dyn UserManager>,
    config: Arc<dyn ServerConfigurationManager>,
) -> AppState {
    let auth = Arc::new(OkAuth);
    AppState::new(
        Arc::new(FakeLibrary),
        users,
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(StubSystem),
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
        Arc::new(StubLocalization),
        Arc::new(FakeDisplayPreferences),
        Arc::new(StubActivity::default()),
        Arc::new(StubFileSystem),
        Arc::new(FakeTasks),
    )
}

/// Sends an authenticated GET and returns `(status, body-bytes)`.
async fn get(app: AppState, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = create_router(app)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("X-Emby-Token", "tok")
                .body(Body::empty())
                .unwrap(),
        )
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

/// Sends an authenticated JSON POST and returns its status.
async fn post(app: AppState, uri: &str, body: &serde_json::Value) -> StatusCode {
    create_router(app)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("X-Emby-Token", "tok")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// jellyfin-web's Dashboard → Libraries → Display page: it reads
/// `UseFileCreationTimeForDateAdded` from `GET /System/Configuration/metadata`
/// (the default, "Use file creation date", before anything is saved) and
/// saves "Use date scanned into the library" as `false` through the POST —
/// which the next GET returns and the scan reads from the same document.
#[tokio::test]
async fn the_date_added_behavior_round_trips_through_the_metadata_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    };

    let (status, body) = get(
        admin_state_with_paths(paths.clone()),
        "/System/Configuration/metadata",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json(&body)["UseFileCreationTimeForDateAdded"],
        serde_json::json!(true)
    );

    for saved in [false, true, false] {
        let status = post(
            admin_state_with_paths(paths.clone()),
            "/System/Configuration/metadata",
            &serde_json::json!({ "UseFileCreationTimeForDateAdded": saved }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = get(
            admin_state_with_paths(paths.clone()),
            "/System/Configuration/metadata",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json(&body)["UseFileCreationTimeForDateAdded"],
            serde_json::json!(saved)
        );
    }
    let on_disk: ferrofin_model::configuration::MetadataConfiguration = serde_json::from_slice(
        &std::fs::read(dir.path().join("named").join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert!(!on_disk.use_file_creation_time_for_date_added);

    // Saving it is an administrator's call.
    let status = post(
        state_with_paths(paths),
        "/System/Configuration/metadata",
        &serde_json::json!({ "UseFileCreationTimeForDateAdded": true }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Named configuration is the one body Jellyfin binds case-SENSITIVELY: the
/// controller deserializes the `JsonDocument` itself with `JsonDefaults.Options`,
/// which lacks the MVC binder's `PropertyNameCaseInsensitive`. Measured on a
/// live Jellyfin 12.2: a camelCase `useFileCreationTimeForDateAdded: false`
/// is ignored and the setting stays at its default, `true`. End-to-end parity
/// check; the extractor-level guard is `extract`'s
/// `a_json_document_body_binds_member_names_exactly`.
#[tokio::test]
async fn a_named_configuration_body_binds_member_names_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    };
    let status = post(
        admin_state_with_paths(paths),
        "/System/Configuration/metadata",
        &serde_json::json!({ "useFileCreationTimeForDateAdded": false }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let saved: ferrofin_model::configuration::MetadataConfiguration = serde_json::from_slice(
        &std::fs::read(dir.path().join("named").join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert!(
        saved.use_file_creation_time_for_date_added,
        "the camelCase member must not bind"
    );
}

#[tokio::test]
async fn configuration_read_returns_server_config() {
    let (status, body) = get(full_state(), "/System/Configuration").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["ServerName"], "Ferrofin");
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
async fn branding_configuration_path_serves_get() {
    // The dedicated POST /System/Configuration/Branding route must also answer GET
    // (the static route shadows the {key} route's GET for this exact path), so a
    // client GETting the branding config gets 200 + the branding object, not 405.
    let (status, body) = get(full_state(), "/System/Configuration/Branding").await;
    assert_eq!(status, StatusCode::OK);
    assert!(json(&body).is_object());
}

#[tokio::test]
async fn unauthenticated_system_configuration_is_401() {
    // No token header → RequireAuth rejects. Use the shared fake state (its
    // FakeAuthService rejects).
    let app = ferrofin_api::test_support::fake_state();
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

// ---------------------------------------------------------------------------
// A broken store must not read as an empty one.
//
// Jellyfin's `BaseConfigurationManager.LoadConfiguration` catches a failed
// deserialize, **logs** `Error loading configuration file: {Path}`, and then
// falls back to `Activator.CreateInstance` — so the response shape is the typed
// default either way, and the log is the only thing that tells an admin their
// saved settings did not really reset. These tests pin both halves: the body
// stays the lenient default (parity), and the failure is reported.
// ---------------------------------------------------------------------------

/// Log events captured by [`CaptureLayer`], as `"<LEVEL> field=value …"` lines.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<String>>>);

impl Captured {
    /// The captured lines that are `WARN` (or worse) and mention `needle`.
    fn warnings_matching(&self, needle: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("WARN") || line.starts_with("ERROR"))
            .filter(|line| line.contains(needle))
            .cloned()
            .collect()
    }
}

/// A `tracing` layer that flattens every event into [`Captured`].
struct CaptureLayer(Captured);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        use std::fmt::Write;

        struct Fields<'a>(&'a mut String);
        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = write!(self.0, " {}={value:?}", field.name());
            }
        }

        let mut line = event.metadata().level().to_string();
        event.record(&mut Fields(&mut line));
        self.0.0.lock().unwrap().push(line);
    }
}

/// Runs `future` with a scoped subscriber, returning its output + the log lines.
async fn with_captured_logs<F: std::future::Future>(future: F) -> (F::Output, Captured) {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::layer::SubscriberExt;

    let captured = Captured::default();
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer(Captured(Arc::clone(&captured.0))));
    let output = future.with_subscriber(subscriber).await;
    (output, captured)
}

/// Writes `body` to `{dir}/named/{key}.json` (creating the `named/` subdir).
fn write_named_config(dir: &std::path::Path, key: &str, body: &[u8]) {
    let named = dir.join("named");
    std::fs::create_dir_all(&named).unwrap();
    std::fs::write(named.join(format!("{key}.json")), body).unwrap();
}

#[tokio::test]
async fn corrupt_named_configuration_falls_back_to_default_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    // A truncated / partially-written save: the file exists but is not JSON.
    write_named_config(dir.path(), "encoding", b"{\"EnableThrottling\": tru");
    let state = state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });

    let ((status, body), logs) =
        with_captured_logs(get(state, "/System/Configuration/encoding")).await;

    // Parity: the response shape is unchanged — the typed default object.
    assert_eq!(status, StatusCode::OK);
    assert!(json(&body).is_object(), "expected default EncodingOptions");
    // …but the corrupt store is reported instead of silently swallowed.
    let warnings = logs.warnings_matching("not valid JSON");
    assert_eq!(
        warnings.len(),
        1,
        "corrupt named configuration must warn exactly once; captured: {:?}",
        logs.0.lock().unwrap()
    );
    assert!(
        warnings[0].contains("encoding.json"),
        "the warning must name the offending file: {}",
        warnings[0]
    );
}

#[tokio::test]
async fn unreadable_named_configuration_falls_back_to_default_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    // A directory where the config file belongs: `read` fails with EISDIR, i.e.
    // an I/O error that is *not* "never saved".
    std::fs::create_dir_all(dir.path().join("named").join("encoding.json")).unwrap();
    let state = state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });

    let ((status, body), logs) =
        with_captured_logs(get(state, "/System/Configuration/encoding")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(json(&body).is_object());
    let warnings = logs.warnings_matching("reading named configuration failed");
    assert_eq!(
        warnings.len(),
        1,
        "an unreadable named configuration must warn; captured: {:?}",
        logs.0.lock().unwrap()
    );
}

#[tokio::test]
async fn missing_named_configuration_is_silent() {
    // The ordinary "never saved" case (upstream's `File.Exists` guard) must not
    // cry wolf — otherwise the warnings above mean nothing.
    let dir = tempfile::tempdir().unwrap();
    let state = state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });

    let ((status, _), logs) =
        with_captured_logs(get(state, "/System/Configuration/encoding")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        logs.warnings_matching("named configuration").is_empty(),
        "a never-saved configuration must not warn: {:?}",
        logs.0.lock().unwrap()
    );
}

#[tokio::test]
async fn a_saved_network_configuration_is_served_under_the_contract_names() {
    // A `network.json` an older Ferrofin wrote, with the four names a
    // `PascalCase` derive produced. Served verbatim, jellyfin-web would not
    // recognise them — the network page would show an empty remote-IP filter,
    // and the operator's next Save would persist that emptiness.
    let dir = tempfile::tempdir().unwrap();
    write_named_config(
        dir.path(),
        "network",
        br#"{"BaseUrl":"/jf","EnableIpv4":false,"EnableIpv6":true,
            "RemoteIpFilter":["192.168.1.5"],"IsRemoteIpFilterBlacklist":true,
            "KnownProxies":["10.1.2.3"]}"#,
    );
    let state = state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });

    let (status, body) = get(state, "/System/Configuration/network").await;
    assert_eq!(status, StatusCode::OK);
    let value = json(&body);
    let object = value.as_object().expect("an object");

    assert_eq!(object["RemoteIPFilter"], serde_json::json!(["192.168.1.5"]));
    assert_eq!(object["IsRemoteIPFilterBlacklist"], serde_json::json!(true));
    assert_eq!(object["EnableIPv4"], serde_json::json!(false));
    assert_eq!(object["EnableIPv6"], serde_json::json!(true));
    for old in [
        "RemoteIpFilter",
        "IsRemoteIpFilterBlacklist",
        "EnableIpv4",
        "EnableIpv6",
    ] {
        assert!(!object.contains_key(old), "{old} must not reach the client");
    }
    // The values around them survive the normalization, and the fields the old
    // document never had come back as defaults.
    assert_eq!(object["BaseUrl"], serde_json::json!("/jf"));
    assert_eq!(object["KnownProxies"], serde_json::json!(["10.1.2.3"]));
    assert_eq!(object["EnableUPnP"], serde_json::json!(false));
}

#[tokio::test]
async fn a_network_configuration_that_cannot_be_read_returns_defaults_without_overwriting() {
    // Match BaseConfigurationManager.LoadConfiguration: a malformed stored
    // section returns a typed default and a warning. Reading must not replace
    // the original document, which the operator may still be able to repair.
    let dir = tempfile::tempdir().unwrap();
    write_named_config(
        dir.path(),
        "network",
        br#"{"InternalHttpPort":"not a port"}"#,
    );
    let state = state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });

    let ((status, body), logs) =
        with_captured_logs(get(state, "/System/Configuration/network")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["InternalHttpPort"], serde_json::json!(8096));
    assert_eq!(
        logs.warnings_matching("named configuration").len(),
        1,
        "the unreadable configuration must be reported: {:?}",
        logs.0.lock().unwrap()
    );
    assert_eq!(
        std::fs::read(dir.path().join("named/network.json")).unwrap(),
        br#"{"InternalHttpPort":"not a port"}"#
    );
}

/// These partial documents predate typed named-config writes. The dashboard
/// must see constructor defaults for omitted members, just as it does when
/// Jellyfin loads a partial XML configuration. Unknown members are not typed
/// settings and must not leak back into a core configuration response.
#[tokio::test]
async fn saved_core_configurations_return_typed_defaults() {
    for (key, mut saved, default_field, default_value) in [
        (
            "metadata",
            serde_json::json!({}),
            "UseFileCreationTimeForDateAdded",
            serde_json::json!(true),
        ),
        (
            "xbmcmetadata",
            serde_json::json!({"UserId":"user-1"}),
            "SaveImagePathsInNfo",
            serde_json::json!(true),
        ),
        (
            "livetv",
            serde_json::json!({"PrePaddingSeconds":120}),
            "SaveRecordingNFO",
            serde_json::json!(true),
        ),
        (
            "network",
            serde_json::json!({"EnableIpv4":false}),
            "InternalHttpPort",
            serde_json::json!(8096),
        ),
        (
            "encoding",
            serde_json::json!({"EncodingThreadCount":3}),
            "DownMixAudioBoost",
            serde_json::json!(2.0),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        saved["UnrecognizedSetting"] = serde_json::json!("old client value");
        let bytes = serde_json::to_vec(&saved).unwrap();
        write_named_config(dir.path(), key, &bytes);
        let app = state_with_paths(StubPaths {
            log_dir: String::new(),
            config_dir: dir.path().to_string_lossy().into_owned(),
        });
        // The key is case-insensitive even though JSON members bind exactly.
        let uri = format!("/System/Configuration/{}", key.to_ascii_uppercase());
        let ((status, body), logs) = with_captured_logs(get(app, &uri)).await;
        assert_eq!(status, StatusCode::OK, "{key}");
        let response = json(&body);
        assert_eq!(response[default_field], default_value, "{key}");
        assert!(response.get("UnrecognizedSetting").is_none(), "{key}");
        for (field, value) in saved.as_object().unwrap() {
            match field.as_str() {
                "UnrecognizedSetting" => {}
                "EnableIpv4" => assert_eq!(&response["EnableIPv4"], value),
                _ => assert_eq!(&response[field], value),
            }
        }
        assert!(logs.warnings_matching("named configuration").is_empty());
        assert_eq!(
            std::fs::read(dir.path().join("named").join(format!("{key}.json"))).unwrap(),
            bytes,
            "a GET must not rewrite the stored document"
        );
    }
}

#[tokio::test]
async fn invalid_core_configurations_return_defaults_and_report_the_section() {
    for (key, field, default_value) in [
        ("metadata", "UseFileCreationTimeForDateAdded", true),
        ("xbmcmetadata", "SaveImagePathsInNfo", true),
        ("livetv", "SaveRecordingNFO", true),
        ("encoding", "EnableThrottling", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({field: "invalid boolean"})).unwrap();
        write_named_config(dir.path(), key, &bytes);
        let app = state_with_paths(StubPaths {
            log_dir: String::new(),
            config_dir: dir.path().to_string_lossy().into_owned(),
        });
        let ((status, body), logs) =
            with_captured_logs(get(app, &format!("/System/Configuration/{key}"))).await;
        assert_eq!(status, StatusCode::OK, "{key}");
        assert_eq!(json(&body)[field], default_value, "{key}");
        let warnings = logs.warnings_matching("named configuration");
        assert_eq!(warnings.len(), 1, "{key}");
        assert!(warnings[0].contains(key));
        assert_eq!(
            std::fs::read(dir.path().join("named").join(format!("{key}.json"))).unwrap(),
            bytes
        );
    }
}

#[tokio::test]
async fn plugin_named_configuration_keeps_its_own_shape() {
    let dir = tempfile::tempdir().unwrap();
    let app = admin_state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });
    let uri = "/System/Configuration/plugin-owned";
    let document = serde_json::json!({"CustomSetting":{"NestedValue":[1,"two",false]}});
    assert_eq!(
        post(app.clone(), uri, &document).await,
        StatusCode::NO_CONTENT
    );
    let (status, body) = get(app, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body), document);
}

/// A Live TV manager whose two configuration reads fail (backend down).
struct FailingLiveTv;

#[async_trait]
impl ferrofin_traits::stubs::LiveTvManager for FailingLiveTv {
    async fn get_guide_info(&self) -> Result<ferrofin_model::live_tv::GuideInfo, ServiceError> {
        unimplemented!("this fake is never asked for the guide window")
    }
    async fn get_recommended_programs(
        &self,
        _query: &ferrofin_traits::options::InternalItemsQuery,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>, ServiceError>
    {
        unimplemented!("this fake is never asked for recommended programs")
    }
    async fn get_tuner_hosts(
        &self,
    ) -> Result<Vec<ferrofin_model::live_tv::TunerHostInfo>, ServiceError> {
        Err(ServiceError::backend("tuner store unavailable"))
    }
    async fn get_listing_providers(
        &self,
    ) -> Result<Vec<ferrofin_model::live_tv::ListingsProviderInfo>, ServiceError> {
        Err(ServiceError::backend("listings store unavailable"))
    }
    async fn get_live_tv_info(&self) -> Result<ferrofin_model::live_tv::LiveTvInfo, ServiceError> {
        unreachable!()
    }
    async fn save_tuner_host(
        &self,
        _info: ferrofin_model::live_tv::TunerHostInfo,
    ) -> Result<ferrofin_model::live_tv::TunerHostInfo, ServiceError> {
        unreachable!()
    }
    fn tuner_host_types(&self) -> Vec<ferrofin_model::dto::NameIdPair> {
        unreachable!()
    }
    async fn discover_tuners(
        &self,
        _discovery_duration_ms: u64,
        _new_devices_only: bool,
    ) -> Result<Vec<ferrofin_model::live_tv::TunerHostInfo>, ServiceError> {
        unreachable!()
    }
    async fn delete_tuner_host(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn save_listing_provider(
        &self,
        _info: ferrofin_model::live_tv::ListingsProviderInfo,
    ) -> Result<ferrofin_model::live_tv::ListingsProviderInfo, ServiceError> {
        unreachable!()
    }
    async fn get_lineups(
        &self,
        _provider_id: Option<&str>,
        _provider_type: Option<&str>,
        _country: Option<&str>,
        _location: Option<&str>,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unreachable!()
    }
    async fn get_channel_mapping_options(
        &self,
        _provider_id: &str,
    ) -> Result<ferrofin_model::live_tv::ChannelMappingOptionsDto, ServiceError> {
        unreachable!()
    }
    async fn set_channel_mapping(
        &self,
        _provider_id: &str,
        _tuner_channel_id: &str,
        _provider_channel_id: &str,
    ) -> Result<ferrofin_model::live_tv::TunerChannelMapping, ServiceError> {
        unreachable!()
    }
    async fn delete_listing_provider(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_channels(
        &self,
        _query: &ferrofin_traits::stubs::LiveTvChannelQuery,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<QueryResult<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_channel(
        &self,
        _id: Uuid,
        _user: Option<&ferrofin_db::entities::users::UserEntity>,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_programs(
        &self,
        _query: &ferrofin_traits::options::InternalItemsQuery,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<QueryResult<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_program(
        &self,
        _id: Uuid,
        _user: Option<&ferrofin_db::entities::users::UserEntity>,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_channel_stream_url(&self, _id: Uuid) -> Result<Option<String>, ServiceError> {
        unreachable!()
    }
    async fn get_timers(&self) -> Result<Vec<ferrofin_model::live_tv::TimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn get_timer(
        &self,
        _id: &str,
    ) -> Result<Option<ferrofin_model::live_tv::TimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn create_timer(
        &self,
        _timer: ferrofin_model::live_tv::TimerInfoDto,
    ) -> Result<String, ServiceError> {
        unreachable!()
    }
    async fn update_timer(
        &self,
        _id: &str,
        _timer: ferrofin_model::live_tv::TimerInfoDto,
    ) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn cancel_timer(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_series_timers(
        &self,
        _query: &ferrofin_model::live_tv::SeriesTimerQuery,
    ) -> Result<Vec<ferrofin_model::live_tv::SeriesTimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn get_series_timer(
        &self,
        _id: &str,
    ) -> Result<Option<ferrofin_model::live_tv::SeriesTimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn create_series_timer(
        &self,
        _timer: ferrofin_model::live_tv::SeriesTimerInfoDto,
    ) -> Result<String, ServiceError> {
        unreachable!()
    }
    async fn update_series_timer(
        &self,
        _id: &str,
        _timer: ferrofin_model::live_tv::SeriesTimerInfoDto,
    ) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn cancel_series_timer(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_recordings(
        &self,
    ) -> Result<QueryResult<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_recording(
        &self,
        _id: Uuid,
    ) -> Result<Option<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_recording_path(&self, _id: Uuid) -> Result<Option<String>, ServiceError> {
        unreachable!()
    }
    async fn delete_recording(&self, _id: Uuid) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
        unreachable!()
    }
}

#[tokio::test]
async fn live_tv_config_backend_failure_warns_instead_of_reading_as_unconfigured() {
    let state = full_state().with_live_tv(Arc::new(FailingLiveTv));

    let ((status, body), logs) =
        with_captured_logs(get(state, "/System/Configuration/livetv")).await;

    // Parity: the dashboard still gets a well-formed LiveTvOptions with empty
    // row arrays — a 500 here would break the Live TV settings page.
    assert_eq!(status, StatusCode::OK);
    let value = json(&body);
    assert_eq!(value["TunerHosts"], serde_json::json!([]));
    assert_eq!(value["ListingProviders"], serde_json::json!([]));
    // …but "the Live TV backend is broken" is no longer indistinguishable from
    // "no tuners configured".
    assert_eq!(
        logs.warnings_matching("tuner hosts failed").len(),
        1,
        "a failed tuner-host read must warn; captured: {:?}",
        logs.0.lock().unwrap()
    );
    assert_eq!(
        logs.warnings_matching("listing providers failed").len(),
        1,
        "a failed listing-provider read must warn; captured: {:?}",
        logs.0.lock().unwrap()
    );
}

#[tokio::test]
async fn named_configuration_converts_values_before_persisting_and_keeps_failed_writes_out() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    };
    let app = admin_state_with_paths(paths.clone());
    let response = create_router(app.clone()).oneshot(Request::builder()
        .method("POST").uri("/System/Configuration/encoding")
        .header("X-Emby-Token", "tok").header("Content-Type", "application/json")
        .body(Body::from(r#"{"EncodingThreadCount":"3","encodingThreadCount":"bad","DownMixAudioBoost":"1.5","TranscodingTempPath":1.50,"DownMixStereoAlgorithm":1}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let saved_path = dir.path().join("named/encoding.json");
    let saved = std::fs::read(&saved_path).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(value["EncodingThreadCount"], 3);
    assert_eq!(value["DownMixAudioBoost"], 1.5);
    assert_eq!(value["TranscodingTempPath"], "1.50");
    assert!(
        !value
            .as_object()
            .unwrap()
            .contains_key("encodingThreadCount")
    );
    assert!(value["DownMixStereoAlgorithm"].is_string());
    for bad in [
        serde_json::json!({"EncodingThreadCount":" 3"}),
        serde_json::json!({"EnableThrottling":"true"}),
        serde_json::json!([]),
    ] {
        assert_eq!(
            post(app.clone(), "/System/Configuration/encoding", &bad).await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(std::fs::read(&saved_path).unwrap(), saved);
    }
    assert_eq!(
        post(
            app,
            "/System/Configuration/livetv",
            &serde_json::json!({"GuideDays":""})
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let live: ferrofin_model::live_tv::LiveTvOptions =
        serde_json::from_slice(&std::fs::read(dir.path().join("named/livetv.json")).unwrap())
            .unwrap();
    assert_eq!(live.guide_days, None);
}

#[tokio::test]
async fn nonfinite_encoding_values_survive_storage_and_fail_http_serialization() {
    let dir = tempfile::tempdir().unwrap();
    let app = admin_state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });
    let uri = "/System/Configuration/encoding";
    for field in [
        "DownMixAudioBoost",
        "TonemappingDesat",
        "TonemappingPeak",
        "TonemappingParam",
        "VppTonemappingBrightness",
        "VppTonemappingContrast",
    ] {
        for literal in ["NaN", "Infinity", "-Infinity"] {
            assert_eq!(
                post(app.clone(), uri, &serde_json::json!({field: literal})).await,
                StatusCode::NO_CONTENT
            );
            let saved = std::fs::read(dir.path().join("named/encoding.json")).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&saved).unwrap();
            assert_eq!(value[field], literal);
            // The same ordinary model reader used by the configuration manager
            // restores the actual floating-point value, not null or a default.
            let options: ferrofin_model::configuration::EncodingOptions =
                serde_json::from_slice(&saved).unwrap();
            assert_eq!(serde_json::to_value(options).unwrap()[field], literal);
            assert_eq!(get(app.clone(), uri).await.0, StatusCode::BAD_REQUEST);
        }
    }
    assert_eq!(
        post(
            app.clone(),
            uri,
            &serde_json::json!({"DownMixAudioBoost":2})
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(get(app, uri).await.0, StatusCode::OK);
}

/// The store is supplied by the host; these tests exercise its API boundary.
struct RejectEncoderChange;

impl ferrofin_common::configuration::ValidatingConfiguration for RejectEncoderChange {
    fn validate(
        &self,
        old: &str,
        new: &str,
    ) -> Result<(), ferrofin_common::configuration::ConfigurationValidationError> {
        use ferrofin_common::configuration::ConfigurationValidationError;
        let old: ferrofin_model::configuration::EncodingOptions =
            serde_json::from_str(old).unwrap();
        let new: ferrofin_model::configuration::EncodingOptions =
            serde_json::from_str(new).unwrap();
        assert_eq!(old.encoding_thread_count, 7);
        if new.transcoding_temp_path.as_deref() == Some("missing") {
            return Err(ConfigurationValidationError::DirectoryNotFound(
                "missing directory".into(),
            ));
        }
        Err(ConfigurationValidationError::InvalidOperation(
            "rejected update".into(),
        ))
    }
}

#[tokio::test]
async fn registered_store_validates_before_replacing_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let original = br#"{"EncodingThreadCount":7}"#;
    write_named_config(dir.path(), "encoding", original);
    let app = admin_state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    })
    .with_configuration_validator("ENCODING", Arc::new(RejectEncoderChange));
    for (body, status) in [
        (
            serde_json::json!({"TranscodingTempPath":"missing"}),
            StatusCode::NOT_FOUND,
        ),
        (
            serde_json::json!({"EncoderAppPath":"changed"}),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        assert_eq!(
            post(app.clone(), "/System/Configuration/Encoding", &body).await,
            status
        );
        assert_eq!(
            std::fs::read(dir.path().join("named/encoding.json")).unwrap(),
            original
        );
    }
    assert_eq!(
        std::fs::read_dir(dir.path().join("named")).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn failed_and_concurrent_network_saves_keep_effective_policy_consistent() {
    use ferrofin_networking::{NetworkConfiguration, NetworkManager};
    let dir = tempfile::tempdir().unwrap();
    let state = admin_state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    })
    .with_network(Arc::new(std::sync::RwLock::new(
        NetworkManager::with_defaults(NetworkConfiguration::default(), ""),
    )));
    let remote = "203.0.113.9".parse().unwrap();
    let uri = "/System/Configuration/network";
    let body = serde_json::json!({"LocalNetworkSubnets":["203.0.113.0/24"]});
    // A destination directory makes rename fail after the temporary file is written.
    std::fs::create_dir_all(dir.path().join("named/network.json")).unwrap();
    assert!(!state.is_in_local_network(remote));
    assert_eq!(
        post(state.clone(), uri, &body).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(!state.is_in_local_network(remote));
    assert_eq!(
        std::fs::read_dir(dir.path().join("named")).unwrap().count(),
        1
    );
    std::fs::remove_dir(dir.path().join("named/network.json")).unwrap();
    let mut workers = tokio::task::JoinSet::new();
    for index in 0..20 {
        let app = state.clone();
        workers.spawn(async move {
            let subnets = if index % 2 == 0 {
                vec!["203.0.113.0/24"]
            } else {
                vec!["192.168.0.0/16"]
            };
            post(
                app,
                uri,
                &serde_json::json!({"LocalNetworkSubnets": subnets}),
            )
            .await
        });
    }
    while let Some(result) = workers.join_next().await {
        assert_eq!(result.unwrap(), StatusCode::NO_CONTENT);
    }
    let saved: NetworkConfiguration =
        serde_json::from_slice(&std::fs::read(dir.path().join("named/network.json")).unwrap())
            .unwrap();
    let expected = NetworkManager::with_defaults(saved, "").is_in_local_network(remote);
    assert_eq!(state.is_in_local_network(remote), expected);
    assert_eq!(
        std::fs::read_dir(dir.path().join("named")).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn only_successful_named_saves_notify_configuration_observers() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(StubConfig {
        paths: StubPaths {
            log_dir: String::new(),
            config_dir: dir.path().to_string_lossy().into_owned(),
        },
        ..Default::default()
    });
    let app = state_with_config(Arc::new(FakeAdminUsers), manager.clone());
    let uri = "/System/Configuration/metadata";
    assert_eq!(
        post(
            app.clone(),
            uri,
            &serde_json::json!({"UseFileCreationTimeForDateAdded": false})
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(manager.updates.lock().unwrap().len(), 1);
    assert_eq!(
        post(
            app.clone(),
            uri,
            &serde_json::json!({"UseFileCreationTimeForDateAdded": "bad"})
        )
        .await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    std::fs::remove_file(dir.path().join("named/metadata.json")).unwrap();
    std::fs::create_dir(dir.path().join("named/metadata.json")).unwrap();
    assert_eq!(
        post(app, uri, &serde_json::json!({})).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(manager.updates.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn current_web_configuration_payloads_survive_save_and_readback() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/dashboard-settings.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let app = admin_state_with_paths(StubPaths {
        log_dir: String::new(),
        config_dir: dir.path().to_string_lossy().into_owned(),
    });
    for key in [
        "server",
        "branding",
        "encoding",
        "network",
        "metadata",
        "xbmcmetadata",
        "livetv",
    ] {
        let uri = if key == "server" {
            "/System/Configuration".into()
        } else {
            format!("/System/Configuration/{key}")
        };
        let payload = &fixture[key];
        assert_eq!(
            post(app.clone(), &uri, payload).await,
            StatusCode::NO_CONTENT,
            "{key}"
        );
        let (status, bytes) = get(app.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK, "{key}");
        let returned = json(&bytes);
        for (field, expected) in payload.as_object().unwrap() {
            assert_eq!(
                &returned[field], expected,
                "{key}.{field} must survive the Web save action"
            );
        }
    }
}
