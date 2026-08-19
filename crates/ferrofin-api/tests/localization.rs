//! Localization handler tests: cultures / countries / parental-ratings / options.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with
//! stub `ferrofin-traits` impls that authenticate and return canned reference sets.
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
        // Deliberately out of order, with mixed-case initials: an ordinal sort
        // ("Zulu" < "afar") and a case-insensitive one ("afar" < "Zulu") disagree
        // on this list, so the ordering assertion can actually fail.
        ["Zulu", "English", "english", "German", "afar"]
            .into_iter()
            .enumerate()
            .map(|(i, display_name)| CultureDto {
                // A distinct Name per entry, so the dedupe's "keep the first in
                // source order" is observable.
                name: format!("c{i}"),
                display_name: display_name.to_owned(),
                two_letter_iso_language_name: display_name[..2].to_ascii_lowercase(),
                three_letter_iso_language_name: Some(display_name[..3].to_ascii_lowercase()),
                three_letter_iso_language_names: vec![display_name[..3].to_ascii_lowercase()],
            })
            .collect()
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

/// The order both `GET /Localization/Cultures` and `GET /Items/{id}/MetadataEditor`
/// must return for the shared stub culture list — the same constant is asserted in
/// `tests/item_update.rs`, because a client cross-references the two lists.
///
/// C#'s `OrderBy(c => c.DisplayName)` takes no comparer, so it sorts with
/// `Comparer<string>.Default` (`StringComparer.CurrentCulture`) — linguistic, not
/// ordinal, which puts "afar" before "Zulu". An ordinal sort would yield
/// `["English", "German", "Zulu", "afar"]`.
const EXPECTED_CULTURE_ORDER: [&str; 4] = ["afar", "English", "German", "Zulu"];

#[tokio::test]
async fn localization_cultures_are_distinct_and_ordered() {
    let (status, body) = get(full_state(), "/Localization/Cultures").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    let arr = v.as_array().unwrap();
    // "English"/"english" collapse to one (OrdinalIgnoreCase DistinctBy).
    let names: Vec<&str> = arr
        .iter()
        .map(|c| c["DisplayName"].as_str().unwrap())
        .collect();
    assert_eq!(names, EXPECTED_CULTURE_ORDER, "case-insensitive order");
    // The dedupe keeps the *first* entry in source order, so "English" (c1) wins
    // over "english" (c2).
    assert_eq!(arr[1]["Name"], "c1");
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
