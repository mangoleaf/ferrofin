//! Branding handler tests: branding configuration + CSS, the branding update
//! route (splashscreen-location preservation), and the splashscreen
//! upload/get/delete lifecycle.
//!
//! Each test drives one real handler through `tower::ServiceExt::oneshot` with a
//! stub [`ServerConfigurationManager`] backed by an in-memory branding record and
//! a per-test data path; every other manager reuses the `test_support` panic
//! fakes. All tests share one harness: a `StubConfig` (mutable branding + data
//! path) driven through a single `send`/`get` pair.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeActivity, FakeApiKeys, FakeAppHost, FakeClientEventLogger, FakeCollections, FakeDevices,
    FakeDisplayPreferences, FakeDto, FakeFileSystem, FakeLibrary, FakeLocalization, FakeLyrics,
    FakeMediaSegments, FakeMediaSources, FakeMusic, FakePlaylists, FakeProviders, FakeQuickConnect,
    FakeSearch, FakeSessions, FakeSimilarItems, FakeSubtitles, FakeSystem, FakeTasks,
    FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::system::ServerApplicationPaths;
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

/// A [`ServerApplicationPaths`] whose `data_path` is a per-test directory.
struct TmpPaths {
    data: String,
}

impl ServerApplicationPaths for TmpPaths {
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
        self.data.clone()
    }
    fn image_cache_path(&self) -> String {
        String::new()
    }
    fn cache_path(&self) -> String {
        String::new()
    }
    fn log_directory_path(&self) -> String {
        String::new()
    }
}

/// A [`ServerConfigurationManager`] backed by an in-memory branding record and a
/// per-test data path, recording `update_branding` so the write routes can be
/// checked.
struct StubConfig {
    data_path: String,
    branding: Mutex<BrandingOptions>,
}

#[async_trait]
impl ServerConfigurationManager for StubConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(TmpPaths {
            data: self.data_path.clone(),
        })
    }
    async fn configuration(&self) -> Result<ServerConfiguration, ServiceError> {
        Ok(ServerConfiguration::default())
    }
    async fn update_configuration(&self, _c: &ServerConfiguration) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Ok(self.branding.lock().expect("lock").clone())
    }
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        *self.branding.lock().expect("lock") = branding.clone();
        Ok(())
    }
}

/// Builds a shared config Arc with the given branding + data path.
fn make_config(branding: BrandingOptions, data_path: &str) -> Arc<StubConfig> {
    Arc::new(StubConfig {
        data_path: data_path.to_owned(),
        branding: Mutex::new(branding),
    })
}

/// Builds an [`AppState`] whose configuration manager is `config`; every other
/// manager is a panic fake.
fn state(config: Arc<StubConfig>) -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(FakeSystem),
        Arc::new(FakeAppHost),
        config,
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
        Arc::new(FakeTasks),
    )
}

/// Drives one request through a router built from `config`, optionally with a
/// `(content_type, payload)` body. Returns `(status, body-bytes)`.
async fn send(
    config: Arc<StubConfig>,
    method: &str,
    uri: &str,
    body: Option<(&str, &str)>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Bearer token");
    let request = if let Some((content_type, payload)) = body {
        builder = builder.header("Content-Type", content_type);
        builder
            .body(Body::from(payload.to_owned()))
            .expect("request")
    } else {
        builder.body(Body::empty()).expect("request")
    };
    let response = create_router(state(config))
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, bytes)
}

/// Convenience: an authenticated GET.
async fn get(config: Arc<StubConfig>, uri: &str) -> (StatusCode, Vec<u8>) {
    send(config, "GET", uri, None).await
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("valid JSON body")
}

fn default_branding() -> BrandingOptions {
    BrandingOptions::default()
}

// ---- branding configuration + css ---------------------------------------------

#[tokio::test]
async fn branding_configuration_and_css() {
    // Seed a branding config with custom CSS.
    let config = make_config(
        BrandingOptions {
            custom_css: Some("body{color:red}".to_owned()),
            splashscreen_enabled: true,
            splashscreen_location: Some("/secret.png".to_owned()),
            login_disclaimer: Some("Hi".to_owned()),
        },
        "/tmp",
    );

    let (status, body) = get(config.clone(), "/Branding/Configuration").await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v["CustomCss"], "body{color:red}");
    assert_eq!(v["SplashscreenEnabled"], true);
    // The DTO must not leak the splashscreen location.
    assert!(v.get("SplashscreenLocation").is_none());

    let (css_status, css_body) = get(config, "/Branding/Css").await;
    assert_eq!(css_status, StatusCode::OK);
    assert_eq!(String::from_utf8(css_body).unwrap(), "body{color:red}");
}

#[tokio::test]
async fn update_branding_preserves_splashscreen_location() {
    let config = make_config(
        BrandingOptions {
            splashscreen_location: Some("/keep.png".to_owned()),
            ..Default::default()
        },
        "/tmp",
    );

    let (status, _) = send(
        config.clone(),
        "POST",
        "/System/Configuration/Branding",
        Some((
            "application/json",
            r#"{"LoginDisclaimer":"New","SplashscreenEnabled":true}"#,
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let saved = config.branding.lock().unwrap().clone();
    assert_eq!(saved.login_disclaimer.as_deref(), Some("New"));
    assert!(saved.splashscreen_enabled);
    // The pre-existing splashscreen location must be preserved.
    assert_eq!(saved.splashscreen_location.as_deref(), Some("/keep.png"));
}

// ---- branding splashscreen ----------------------------------------------------

#[tokio::test]
async fn splashscreen_upload_then_get_then_delete() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let data_path = dir.to_string_lossy().into_owned();

    let mut branding = default_branding();
    branding.splashscreen_enabled = true;
    let config = make_config(branding, &data_path);

    // Upload: writes <data>/splashscreen-upload.png and records the location.
    let (status, _) = send(
        config.clone(),
        "POST",
        "/Branding/Splashscreen",
        Some(("image/png", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let recorded = config.get_branding().await.expect("branding");
    let location = recorded.splashscreen_location.expect("location");
    assert!(std::path::Path::new(&location).is_file());

    // GET: serves the uploaded file.
    let (status, body) = send(config.clone(), "GET", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hi");

    // DELETE: removes the file and clears the location.
    let (status, _) = send(config.clone(), "DELETE", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!std::path::Path::new(&location).exists());
    assert!(
        config
            .get_branding()
            .await
            .expect("branding")
            .splashscreen_location
            .is_none()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn splashscreen_get_disabled_is_404() {
    let config = make_config(default_branding(), "/tmp");
    let (status, _) = send(config, "GET", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn splashscreen_get_enabled_but_no_file_is_404() {
    let mut branding = default_branding();
    branding.splashscreen_enabled = true;
    // A data path with no splashscreen.png present.
    let config = make_config(branding, "/nonexistent-ferrofin-data-dir");
    let (status, _) = send(config, "GET", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn splashscreen_upload_bad_content_type_is_400() {
    let config = make_config(default_branding(), "/tmp");
    let (status, _) = send(
        config,
        "POST",
        "/Branding/Splashscreen",
        Some(("text/plain", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn splashscreen_delete_no_location_is_204() {
    let config = make_config(default_branding(), "/tmp");
    let (status, _) = send(config, "DELETE", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
