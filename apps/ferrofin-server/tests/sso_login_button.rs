//! The login page's "Sign in with" buttons (B3, D7), through the composition
//! root and the real router (`tower` oneshot): `GET /Branding/Configuration`
//! appends one form per enabled provider to the stored disclaimer while the
//! Single Sign-On plugin is enabled with a provider; the dashboard's branding
//! page, saving back the text it was served, stores only the admin's; and
//! without a provider, or with the plugin's switch off, the answer is the
//! stored branding byte for byte (B1).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use ferrofin_db::Database;
use ferrofin_extensions::sso::EXTENSION_ID;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "sso-login-button-pw";
const CLIENT_AUTH: &str = "MediaBrowser Client=\"sso-login-button-test\", Device=\"test\", \
                           DeviceId=\"sso-login-button-test-1\", Version=\"1.0\"";

/// The branding the admin stored, with a CRLF line break, as a form
/// submitted natively sends a textarea's.
const DISCLAIMER: &str = "Welcome.\r\nAsk Sam for an account.";

/// The one button of the provider `keycloak`, named `Home <Keycloak>`.
const BUTTON: &str = "<form action=\"https://media.example.org/jf/sso/oidc/keycloak/start\" \
                      method=\"get\"><button class=\"raised block emby-button button-submit\" \
                      type=\"submit\">Sign in with Home &lt;Keycloak&gt;</button></form>";

async fn body(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec()
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("JSON")
}

/// The server, published at `https://media.example.org/jf`, over a fresh
/// database in `temp`: its state, its router, its shutdown channel's
/// receiver (held while it serves), and a signed-in admin's
/// `Authorization` header.
async fn server(
    temp: &tempfile::TempDir,
) -> (
    ferrofin_api::state::AppState,
    axum::Router,
    tokio::sync::oneshot::Receiver<()>,
    String,
) {
    let config = Config {
        server_name: "sso-login-button-test".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        published_url: Some("https://media.example.org/jf".to_owned()),
        ..Config::test_stub(temp.path())
    };
    for dir in [&config.config_dir, &config.data_dir, &config.cache_dir] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    let state = wired.state;
    let router = ferrofin_api::create_router(state.clone());

    let signed_in = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, CLIENT_AUTH)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("authenticate");
    assert_eq!(signed_in.status(), StatusCode::OK);
    let token = json(&body(signed_in).await)["AccessToken"]
        .as_str()
        .expect("access token")
        .to_owned();
    let admin = format!("{CLIENT_AUTH}, Token=\"{token}\"");
    (state, router, shutdown_rx, admin)
}

#[tokio::test]
async fn the_login_page_shows_a_button_per_provider_and_the_branding_keeps_none() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (state, router, _shutdown, admin) = server(&temp).await;
    // An admin's request, or, with `as_admin` false, the login page's.
    let send = |method: &str, uri: &str, as_admin: bool, payload: Option<serde_json::Value>| {
        let mut request = Request::builder().method(method).uri(uri);
        if as_admin {
            request = request.header(header::AUTHORIZATION, admin.as_str());
        }
        if payload.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(payload.map_or_else(Body::empty, |p| Body::from(p.to_string())))
            .expect("request");
        let router = router.clone();
        async move {
            let response = router.oneshot(request).await.expect("response");
            (response.status(), body(response).await)
        }
    };
    let send = &send;
    let login_page = || async move {
        let (status, bytes) = send("GET", "/Branding/Configuration", false, None).await;
        assert_eq!(status, StatusCode::OK);
        bytes
    };
    let save_branding = |disclaimer: String| {
        // The dashboard's branding page: `updateNamedConfiguration('branding')`.
        // Without the server's path re-casing this router takes the
        // `{key}` route's branding branch; a server takes the literal
        // `/System/Configuration/Branding`, which `tests/branding.rs` in
        // ferrofin-api covers.
        send(
            "POST",
            "/System/Configuration/branding",
            true,
            Some(serde_json::json!({
                "LoginDisclaimer": disclaimer,
                "CustomCss": "",
                "SplashscreenEnabled": false,
            })),
        )
    };
    let stored = || async move {
        let (status, bytes) = send("GET", "/System/Configuration/branding", true, None).await;
        assert_eq!(status, StatusCode::OK);
        json(&bytes)["LoginDisclaimer"].clone()
    };

    let (status, _) = save_branding(DISCLAIMER.to_owned()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let stock = login_page().await;
    assert_eq!(
        String::from_utf8_lossy(&stock),
        r#"{"LoginDisclaimer":"Welcome.\r\nAsk Sam for an account.","CustomCss":"","SplashscreenEnabled":false}"#
    );

    // Enabled, but no provider yet: unchanged (B1).
    state
        .plugins
        .enable_plugin(EXTENSION_ID)
        .await
        .expect("enable");
    assert_eq!(login_page().await, stock);

    // A provider: its button, on the published URL, its name escaped.
    let sso = state.sso.as_ref().expect("the Single Sign-On service");
    sso.save_oidc_provider(
        "keycloak",
        OidcProvider {
            name: "Home <Keycloak>".to_owned(),
            issuer: "https://id.example.org/realms/home".to_owned(),
            client_id: "ferrofin".to_owned(),
            client_secret: Some(Secret::new("login-button-client-secret")),
            ..OidcProvider::default()
        },
    )
    .await
    .expect("saved");
    let served = json(&login_page().await)["LoginDisclaimer"]
        .as_str()
        .expect("a disclaimer")
        .to_owned();
    assert_eq!(
        served,
        format!(
            "{DISCLAIMER}\n\n<!-- ferrofin:sso-login-buttons -->{BUTTON}\
             <!-- /ferrofin:sso-login-buttons -->"
        )
    );

    // The dashboard saves what it was served, as jellyfin-web 12.4 sends it,
    // then with every line break a CRLF: the stored disclaimer is the
    // admin's, and the page shows one button.
    for saved in [
        served.clone(),
        served.replace("\r\n", "\n").replace('\n', "\r\n"),
    ] {
        let (status, _) = save_branding(saved).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(stored().await, DISCLAIMER);
        let again = json(&login_page().await)["LoginDisclaimer"].clone();
        assert_eq!(again, served.as_str());
    }

    // The switch off, as the settings page saves it: the stored branding
    // byte for byte.
    let plugin = format!("/Plugins/{}/Configuration", EXTENSION_ID.simple());
    let (status, bytes) = send("GET", &plugin, true, None).await;
    assert_eq!(status, StatusCode::OK);
    let mut configuration = json(&bytes);
    assert_eq!(configuration["ShowLoginButton"], true);
    configuration["ShowLoginButton"] = serde_json::json!(false);
    let (status, _) = send("POST", &plugin, true, Some(configuration)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(login_page().await, stock);
}
