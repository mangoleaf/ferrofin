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
    FakeTrickplay, FakeTvSeries, FakeUserData, FakeUserViews, FakeUsers, fake_user_entity,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcDiscovery, OidcProvider, OidcProviderInfo, OidcProviderList};
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{AuthService, AuthorizationContext, RequestContext};
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::AuthenticationRequest;
use ferrofin_traits::sso::{OidcCallback, OidcExchangeReply, SsoManager, SsoReply};
use ferrofin_traits::system::ServerApplicationPaths;
use tower::ServiceExt;
use uuid::Uuid;

const USER_ID: Uuid = Uuid::from_u128(0x00D1_0000);

/// A minimal authenticated user.
fn user() -> UserEntity {
    fake_user_entity(USER_ID, "bob")
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
    /// When set, `update_branding` says it has started and waits to be
    /// released before it records the branding.
    gate: Option<Arc<SaveGate>>,
}

/// Holds a branding save between its start and its write.
#[derive(Default)]
struct SaveGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ServerConfigurationManager for StubConfig {
    fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
        Arc::new(TmpPaths {
            data: self.data_path.clone(),
        })
    }
    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        Ok(Arc::new(ServerConfiguration::default()))
    }
    async fn update_configuration(&self, _c: &ServerConfiguration) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
        Ok(self.branding.lock().expect("lock").clone())
    }
    async fn update_branding(&self, branding: &BrandingOptions) -> Result<(), ServiceError> {
        if let Some(gate) = &self.gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        *self.branding.lock().expect("lock") = branding.clone();
        Ok(())
    }
}

/// Builds a shared config Arc with the given branding + data path.
fn make_config(branding: BrandingOptions, data_path: &str) -> Arc<StubConfig> {
    Arc::new(StubConfig {
        data_path: data_path.to_owned(),
        branding: Mutex::new(branding),
        gate: None,
    })
}

/// Builds an [`AppState`] whose configuration manager is `config`; every other
/// manager is a panic fake.
fn state(config: Arc<StubConfig>) -> AppState {
    state_as(config, &Caller::Key)
}

/// Who the stub auth services say is calling.
enum Caller {
    /// An API key: elevated (the default for every test above).
    Key,
    /// A signed-in user who is not an administrator.
    Regular,
    /// No credentials at all.
    Anonymous,
}

#[async_trait]
impl AuthService for Caller {
    async fn authenticate(
        &self,
        _request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        Ok(match self {
            Self::Key => AuthorizationInfo {
                token: Some("tok".into()),
                user: Some(Arc::new(user())),
                is_api_key: true,
                is_authenticated: true,
                ..Default::default()
            },
            Self::Regular => AuthorizationInfo {
                token: Some("tok".into()),
                user: Some(Arc::new(user())),
                user_policy: Some(Arc::new(ferrofin_model::users::UserPolicy::default())),
                is_authenticated: true,
                ..Default::default()
            },
            Self::Anonymous => AuthorizationInfo::default(),
        })
    }
}

#[async_trait]
impl AuthorizationContext for Caller {
    async fn get_authorization_info(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError> {
        self.authenticate(request).await
    }
}

fn state_as(config: Arc<StubConfig>, caller: &Caller) -> AppState {
    let auth = || {
        Arc::new(match caller {
            Caller::Key => Caller::Key,
            Caller::Regular => Caller::Regular,
            Caller::Anonymous => Caller::Anonymous,
        })
    };
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
        auth(),
        auth(),
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

/// The upload is named from `MimeTypes.ToExtension`: `image/jpg` is `.jpg`
/// (Jellyfin 12 saves `splashscreen-upload.jpg`), and image types outside the
/// override table take the base table's extension, as upstream's
/// `ImageControllerTests` expect for `image/apng` and `image/avif`.
#[rstest::rstest]
#[case("image/jpg", "splashscreen-upload.jpg")]
#[case("image/avif", "splashscreen-upload.avif")]
#[case("image/apng", "splashscreen-upload.apng")]
#[tokio::test]
async fn splashscreen_upload_names_the_file_from_the_content_type(
    #[case] content_type: &str,
    #[case] file_name: &str,
) {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let config = make_config(default_branding(), &dir.to_string_lossy());

    let (status, _) = send(
        config.clone(),
        "POST",
        "/Branding/Splashscreen",
        Some((content_type, "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let location = config
        .get_branding()
        .await
        .expect("branding")
        .splashscreen_location
        .expect("location");
    let location = std::path::Path::new(&location);
    assert_eq!(
        location.file_name().and_then(|name| name.to_str()),
        Some(file_name)
    );
    assert!(location.is_file());

    let _ = std::fs::remove_dir_all(&dir);
}

/// gl-24: `UploadCustomSplashscreen` streams the base64 body under Kestrel's
/// 30,000,000-byte default, so a 3 MB image (4 MB of base64) saves instead of
/// tripping axum's implicit 2 MiB limit, and a body past the limit is a `413`
/// that writes nothing.
#[tokio::test]
async fn splashscreen_upload_takes_bodies_up_to_kestrels_default_limit() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let config = make_config(default_branding(), &dir.to_string_lossy());

    let large = "A".repeat(4_000_000);
    let (status, _) = send(
        config.clone(),
        "POST",
        "/Branding/Splashscreen",
        Some(("image/png", &large)),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let location = config
        .get_branding()
        .await
        .expect("branding")
        .splashscreen_location
        .expect("location");
    assert_eq!(
        std::fs::metadata(&location).expect("splashscreen").len(),
        3_000_000
    );
    std::fs::remove_file(&location).expect("remove");

    let too_large = "A".repeat(30_000_004);
    let (status, _) = send(
        config,
        "POST",
        "/Branding/Splashscreen",
        Some(("image/png", &too_large)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!std::path::Path::new(&location).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// An image subtype `ToExtension` does not know is still `400`.
#[tokio::test]
async fn splashscreen_upload_unknown_image_subtype_is_400() {
    let config = make_config(default_branding(), "/nonexistent-ferrofin-data-dir");
    let (status, _) = send(
        config.clone(),
        "POST",
        "/Branding/Splashscreen",
        Some(("image/x-unknown", "aGk=")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        config
            .get_branding()
            .await
            .expect("branding")
            .splashscreen_location
            .is_none()
    );
}

#[tokio::test]
async fn splashscreen_delete_no_location_is_204() {
    let config = make_config(default_branding(), "/tmp");
    let (status, _) = send(config, "DELETE", "/Branding/Splashscreen", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// `GetSplashscreen` and `DeleteCustomSplashscreen` test `SplashscreenLocation`
/// with `File.Exists`, which is false for a directory: GET falls back to
/// `{DataPath}/splashscreen.png` and DELETE leaves the directory and the
/// recorded location alone, as for a missing file.
#[tokio::test]
async fn a_splashscreen_location_that_is_not_a_file_is_ignored() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("splashscreen.png"), b"default-bytes").expect("splash");
    for location in [dir.clone(), dir.join("missing.png")] {
        let location = location.to_string_lossy().into_owned();
        let mut branding = default_branding();
        branding.splashscreen_enabled = true;
        branding.splashscreen_location = Some(location.clone());
        let config = make_config(branding, &dir.to_string_lossy());

        let (status, _, body) =
            get_as(config.clone(), &Caller::Anonymous, "/Branding/Splashscreen").await;
        assert_eq!(
            (status, body.as_slice()),
            (StatusCode::OK, &b"default-bytes"[..]),
            "{location}"
        );

        let (status, _) = send(config.clone(), "DELETE", "/Branding/Splashscreen", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{location}");
        let recorded = config.get_branding().await.expect("branding");
        assert_eq!(recorded.splashscreen_location, Some(location));
    }
    assert!(dir.is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Drives one GET as `caller`, returning the status, headers and body.
async fn get_as(
    config: Arc<StubConfig>,
    caller: &Caller,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", "Bearer token")
        .body(Body::empty())
        .expect("request");
    let response = create_router(state_as(config, caller))
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, bytes)
}

/// `GetSplashscreen`: `!SplashscreenEnabled && !User.IsInRole(Administrator)`
/// is a 404, so an administrator (or API key) previews a disabled splashscreen
/// that a signed-in user or an anonymous caller cannot see.
#[tokio::test]
async fn a_disabled_splashscreen_is_visible_to_administrators_only() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("splashscreen.png"), b"png-bytes").expect("splash");
    let data_path = dir.to_string_lossy().into_owned();
    let config = make_config(default_branding(), &data_path);

    let (status, _, body) = get_as(config.clone(), &Caller::Key, "/Branding/Splashscreen").await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, &b"png-bytes"[..])
    );
    for caller in [Caller::Regular, Caller::Anonymous] {
        let (status, ..) = get_as(config.clone(), &caller, "/Branding/Splashscreen").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // Enabled, everyone sees it.
    let mut enabled = default_branding();
    enabled.splashscreen_enabled = true;
    let config = make_config(enabled, &data_path);
    for caller in [Caller::Regular, Caller::Anonymous] {
        let (status, ..) = get_as(config.clone(), &caller, "/Branding/Splashscreen").await;
        assert_eq!(status, StatusCode::OK);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `GetSplashscreen` passes `GetImageResult` no response headers, so it sends
/// neither of the DLNA headers every other image route carries (gl-1118).
#[tokio::test]
async fn a_splashscreen_sends_no_dlna_headers() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("splashscreen.png"), b"png-bytes").expect("splash");
    let mut branding = default_branding();
    branding.splashscreen_enabled = true;
    let config = make_config(branding, &dir.to_string_lossy());

    let (status, headers, _) = get_as(config, &Caller::Anonymous, "/Branding/Splashscreen").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        headers.get("transfermode.dlna.org").is_none(),
        "{headers:?}"
    );
    assert!(
        headers.get("realtimeinfo.dlna.org").is_none(),
        "{headers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `tag` earns the 365-day immutable caching of `GetImageResult`; without
/// one the response is plain `public`.
#[tokio::test]
async fn a_tagged_splashscreen_is_immutable_cacheable() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("splashscreen.png"), b"png-bytes").expect("splash");
    let mut branding = default_branding();
    branding.splashscreen_enabled = true;
    let config = make_config(branding, &dir.to_string_lossy());

    let (_, plain, _) = get_as(config.clone(), &Caller::Anonymous, "/Branding/Splashscreen").await;
    assert_eq!(plain["cache-control"], "public");
    let (_, tagged, _) = get_as(config, &Caller::Anonymous, "/Branding/Splashscreen?tag=abc").await;
    assert_eq!(
        tagged["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(tagged["etag"], "\"abc\"");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the Single Sign-On plugin's sign-in buttons ------------------------------

/// The marker comments around the buttons appended to the login disclaimer.
const BUTTONS_BEGIN: &str = "<!-- ferrofin:sso-login-buttons -->";
const BUTTONS_END: &str = "<!-- /ferrofin:sso-login-buttons -->";

/// An [`SsoManager`] that answers [`SsoManager::login_buttons`] with its
/// value, or an error for `Err`, and nothing else.
struct LoginButtons(Result<Option<&'static str>, ()>);

fn unused() -> ServiceError {
    ServiceError::backend("not used by the branding routes")
}

#[async_trait]
impl SsoManager for LoginButtons {
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        Err(unused())
    }
    async fn oidc_providers(&self) -> Result<OidcProviderList, ServiceError> {
        Err(unused())
    }
    async fn save_oidc_provider(
        &self,
        _key: &str,
        _provider: OidcProvider,
    ) -> Result<OidcProviderInfo, ServiceError> {
        Err(unused())
    }
    async fn delete_oidc_provider(&self, _key: &str) -> Result<(), ServiceError> {
        Err(unused())
    }
    async fn test_oidc_provider(
        &self,
        _provider: &OidcProvider,
    ) -> Result<OidcDiscovery, ServiceError> {
        Err(unused())
    }
    async fn start_oidc_sign_in(
        &self,
        _key: &str,
        _client: std::net::IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        Err(unused())
    }
    async fn finish_oidc_sign_in(
        &self,
        _key: &str,
        _callback: OidcCallback,
    ) -> Result<SsoReply, ServiceError> {
        Err(unused())
    }
    async fn exchange_oidc_sign_in(
        &self,
        _code: Secret,
        _flow_cookie: Option<Secret>,
        _request: AuthenticationRequest,
    ) -> Result<OidcExchangeReply, ServiceError> {
        Err(unused())
    }
    async fn login_buttons(&self) -> Result<Option<String>, ServiceError> {
        self.0
            .map(|buttons| buttons.map(str::to_owned))
            .map_err(|()| ServiceError::backend("the plugin configuration does not read"))
    }
}

/// `GET /Branding/Configuration` as the login page asks it, anonymously, from
/// a server with `sso` wired: the body's bytes.
async fn branding_with(config: Arc<StubConfig>, sso: Option<LoginButtons>) -> Vec<u8> {
    let mut state = state_as(config, &Caller::Anonymous);
    if let Some(sso) = sso {
        state = state.with_sso(Arc::new(sso));
    }
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .uri("/Branding/Configuration")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec()
}

fn stored_branding(login_disclaimer: Option<&str>) -> BrandingOptions {
    BrandingOptions {
        login_disclaimer: login_disclaimer.map(str::to_owned),
        custom_css: Some("body{color:red}".to_owned()),
        splashscreen_enabled: true,
        splashscreen_location: Some("/keep.png".to_owned()),
    }
}

/// B3: the stored disclaimer, a blank line (so markdown reads what follows
/// as an HTML block), then the plugin's forms between the markers; the
/// other members as stored.
#[tokio::test]
async fn the_sign_in_buttons_follow_the_stored_disclaimer_after_a_blank_line() {
    let forms = "<form action=\"https://media.example.org/sso/oidc/kc/start\"></form>";
    for (stored, served) in [
        (Some("Welcome"), "Welcome\n\n"),
        (Some(""), "\n\n"),
        (None, "\n\n"),
    ] {
        let config = make_config(stored_branding(stored), "/tmp");
        let body = json(&branding_with(config, Some(LoginButtons(Ok(Some(forms))))).await);
        assert_eq!(
            body,
            serde_json::json!({
                "LoginDisclaimer": format!("{served}{BUTTONS_BEGIN}{forms}{BUTTONS_END}"),
                "CustomCss": "body{color:red}",
                "SplashscreenEnabled": true,
            }),
            "{stored:?}"
        );
    }
}

/// B1: with no buttons to show (the plugin disabled, no provider, the switch
/// off, no public URL), with a plugin whose configuration does not read, and
/// on a server without the plugin, the response is the stored configuration
/// byte for byte.
#[tokio::test]
async fn without_buttons_the_branding_is_the_stored_one_byte_for_byte() {
    for stored in [Some("Welcome"), None] {
        let config = make_config(stored_branding(stored), "/tmp");
        let without = branding_with(config.clone(), None).await;
        let expected = match stored {
            Some(text) => format!(
                r#"{{"LoginDisclaimer":"{text}","CustomCss":"body{{color:red}}","SplashscreenEnabled":true}}"#
            ),
            None => r#"{"CustomCss":"body{color:red}","SplashscreenEnabled":true}"#.to_owned(),
        };
        assert_eq!(String::from_utf8_lossy(&without), expected);
        for sso in [LoginButtons(Ok(None)), LoginButtons(Err(()))] {
            assert_eq!(
                branding_with(config.clone(), Some(sso)).await,
                without,
                "{stored:?}"
            );
        }
    }
}

/// The dashboard's branding page saves the disclaimer it was served, in `\n`
/// line breaks (or `\r\n`, as a form submitted natively sends them), through
/// either branding save: the buttons and the blank line before them are
/// dropped, so the stored text is the admin's, and a second save adds
/// nothing.
#[tokio::test]
async fn a_saved_disclaimer_never_keeps_the_sign_in_buttons() {
    let block = format!("{BUTTONS_BEGIN}<form><button>Sign in with A</button></form>{BUTTONS_END}");
    for route in [
        "/System/Configuration/Branding",
        "/System/Configuration/branding",
    ] {
        for (saved, stored) in [
            (format!("Welcome\n\n{block}"), "Welcome"),
            (
                format!("Line 1\r\nLine 2\r\n\r\n{block}"),
                "Line 1\r\nLine 2",
            ),
            (format!("\n\n{block}"), ""),
            // Text the admin added after the block stays; so does a block
            // served twice by an earlier save.
            (format!("Welcome\n\n{block}\n\nBye"), "Welcome\n\nBye"),
            (format!("Welcome\n\n{block}\n\n{block}"), "Welcome"),
            // Without both markers, nothing is the plugin's.
            (
                format!("Welcome\n\n{BUTTONS_BEGIN}<form>"),
                "Welcome\n\n<!-- ferrofin:sso-login-buttons --><form>",
            ),
            (
                format!("x{BUTTONS_END}y"),
                "x<!-- /ferrofin:sso-login-buttons -->y",
            ),
            (
                format!("x{BUTTONS_END}y\n\n{block}"),
                "x<!-- /ferrofin:sso-login-buttons -->y",
            ),
            ("Welcome\n\n".to_owned(), "Welcome\n\n"),
        ] {
            let config = make_config(stored_branding(Some("old")), "/tmp");
            // Then the page is served the stored text with the buttons
            // again, and saves that.
            for body in [saved.clone(), format!("{stored}\n\n{block}")] {
                let (status, _) = send(
                    config.clone(),
                    "POST",
                    route,
                    Some((
                        "application/json",
                        &serde_json::json!({
                            "LoginDisclaimer": body,
                            "CustomCss": "body{color:red}",
                            "SplashscreenEnabled": true,
                        })
                        .to_string(),
                    )),
                )
                .await;
                assert_eq!(status, StatusCode::NO_CONTENT, "{route}");
                let branding = config.branding.lock().expect("lock").clone();
                assert_eq!(
                    branding.login_disclaimer.as_deref(),
                    Some(stored),
                    "{route} {body:?}"
                );
                assert_eq!(branding.custom_css.as_deref(), Some("body{color:red}"));
            }
        }
    }
}

/// `UpdateBrandingConfiguration` is a synchronous action, so a client that
/// hangs up once its request is in does not stop the save: through either
/// branding route, the branding is saved after the request is dropped.
#[tokio::test]
async fn a_branding_save_finishes_for_a_client_that_hangs_up() {
    for route in [
        "/System/Configuration/Branding",
        "/System/Configuration/branding",
    ] {
        let gate = Arc::new(SaveGate::default());
        let config = Arc::new(StubConfig {
            data_path: "/tmp".to_owned(),
            branding: Mutex::new(stored_branding(Some("old"))),
            gate: Some(Arc::clone(&gate)),
        });
        let request = tokio::spawn(send(
            config.clone(),
            "POST",
            route,
            Some(("application/json", r#"{"LoginDisclaimer":"New"}"#)),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), gate.started.notified())
            .await
            .unwrap_or_else(|_| panic!("{route}: the save never started"));
        // The client hangs up: the request's future is dropped mid-save.
        request.abort();
        assert!(request.await.is_err_and(|e| e.is_cancelled()), "{route}");
        gate.release.notify_one();
        let mut saved = None;
        for _ in 0..200 {
            saved = config
                .branding
                .lock()
                .expect("lock")
                .login_disclaimer
                .clone();
            if saved.as_deref() == Some("New") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(saved.as_deref(), Some("New"), "{route}");
    }
}

/// `DeleteCustomSplashscreen` is a synchronous action and
/// `UploadCustomSplashscreen` copies with `CancellationToken.None`: a client
/// that hangs up mid-save leaves neither a deleted file still recorded nor a
/// written file unrecorded.
#[tokio::test]
async fn a_splashscreen_save_finishes_for_a_client_that_hangs_up() {
    let dir = std::env::temp_dir().join(format!("ferrofin-splash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let existing = dir.join("splashscreen-upload.png");
    std::fs::write(&existing, b"png-bytes").expect("splash");
    let existing = existing.to_string_lossy().into_owned();
    for (method, body, recorded) in [
        ("DELETE", None, None),
        ("POST", Some(("image/png", "aGk=")), Some(existing.as_str())),
    ] {
        let gate = Arc::new(SaveGate::default());
        let config = Arc::new(StubConfig {
            data_path: dir.to_string_lossy().into_owned(),
            branding: Mutex::new(BrandingOptions {
                splashscreen_location: Some(existing.clone()),
                ..default_branding()
            }),
            gate: Some(Arc::clone(&gate)),
        });
        if method == "POST" {
            config.branding.lock().expect("lock").splashscreen_location = None;
        }
        let request = tokio::spawn(send(config.clone(), method, "/Branding/Splashscreen", body));
        tokio::time::timeout(std::time::Duration::from_secs(10), gate.started.notified())
            .await
            .unwrap_or_else(|_| panic!("{method}: the save never started"));
        request.abort();
        assert!(request.await.is_err_and(|e| e.is_cancelled()), "{method}");
        gate.release.notify_one();
        let mut location = Some(String::new());
        for _ in 0..200 {
            location = config
                .branding
                .lock()
                .expect("lock")
                .splashscreen_location
                .clone();
            if location.as_deref() == recorded {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(location.as_deref(), recorded, "{method}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
