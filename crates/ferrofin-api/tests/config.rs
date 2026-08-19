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
    FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
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
    branding: Mutex<BrandingOptions>,
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
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(self.paths.clone())
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(ServerConfiguration {
            server_name: "Ferrofin".to_owned(),
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
        Arc::new(StubConfig {
            paths,
            ..StubConfig::default()
        }),
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

/// A Live TV manager whose two configuration reads fail (backend down).
struct FailingLiveTv;

#[async_trait]
impl ferrofin_traits::stubs::LiveTvManager for FailingLiveTv {
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
    async fn delete_tuner_host(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn save_listing_provider(
        &self,
        _info: ferrofin_model::live_tv::ListingsProviderInfo,
    ) -> Result<ferrofin_model::live_tv::ListingsProviderInfo, ServiceError> {
        unreachable!()
    }
    async fn delete_listing_provider(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_channels(
        &self,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<QueryResult<ferrofin_model::dto::BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_channel(
        &self,
        _id: Uuid,
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
