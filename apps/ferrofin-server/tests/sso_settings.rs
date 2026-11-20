//! The Single Sign-On plugin's provider settings, through the composition root
//! and the real router (`tower` oneshot): the plugin is listed off by
//! default, its routes answer only while it is enabled, a provider's client
//! secret is kept in the plugin's private file and returned by no route
//! (D16), and a save tests the provider's discovery against the fake
//! identity provider.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use ferrofin_db::Database;
use ferrofin_extensions::sso::fake_idp::{FakeIdentityProvider, FakeResponse};
use ferrofin_extensions::sso::{EXTENSION_ID, SETTINGS_PAGE_NAME};
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "sso-settings-pw";
const SECRET: &str = "keycloak-client-secret-5d1e";

const CLIENT_AUTH: &str = "MediaBrowser Client=\"sso-settings-test\", Device=\"test\", \
                           DeviceId=\"sso-settings-test-1\", Version=\"1.0\"";

async fn body_bytes(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec()
}

#[tokio::test]
// One admin's visit: enable, add, edit, delete, disable; split, it would
// only boot the server again for each step.
#[allow(clippy::too_many_lines)]
async fn an_admin_saves_providers_with_a_secret_no_route_returns() {
    let fake = FakeIdentityProvider::start().await;
    let issuer = fake.issuer();
    let temp = tempfile::tempdir().expect("temp dir");
    let config = Config {
        server_name: "sso-settings-test".to_owned(),
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
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    let router = ferrofin_api::create_router(wired.state.clone());

    let auth = router
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
    assert_eq!(auth.status(), StatusCode::OK);
    let token: serde_json::Value = serde_json::from_slice(&body_bytes(auth).await).expect("json");
    let admin = format!(
        "{CLIENT_AUTH}, Token=\"{}\"",
        token["AccessToken"].as_str().expect("access token")
    );
    // Every answer of every request, to check no secret is in any.
    let answers = std::sync::Mutex::new(String::new());
    let send = |method: &str, uri: &str, body: Option<serde_json::Value>| {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, admin.as_str());
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .expect("request");
        let router = router.clone();
        let answers = &answers;
        async move {
            let response = router.oneshot(request).await.expect("response");
            let status = response.status();
            let bytes = body_bytes(response).await;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            answers.lock().expect("answers").push_str(&text);
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            )
        }
    };
    let providers = "/sso/admin/oidc/providers";
    let keycloak = "/sso/admin/oidc/providers/keycloak";
    let plugin = EXTENSION_ID.simple().to_string();
    // What the login page reads, byte for byte, as an anonymous client asks.
    let branding = || {
        let router = router.clone();
        async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/Branding/Configuration")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("branding");
            assert_eq!(response.status(), StatusCode::OK);
            body_bytes(response).await
        }
    };
    let branding_before = branding().await;

    // Listed, disabled by default, with its routes and page absent.
    let (status, plugins) = send("GET", "/Plugins", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = plugins
        .as_array()
        .expect("plugins")
        .iter()
        .find(|p| p["Name"] == "Single Sign-On")
        .unwrap_or_else(|| panic!("no Single Sign-On: {plugins}"))
        .clone();
    assert_eq!(listed["Status"], "Disabled", "{listed}");
    assert_eq!(
        listed["Id"].as_str().map(|id| id.replace('-', "")),
        Some(plugin.clone()),
        "{listed}"
    );
    let version = listed["Version"].as_str().expect("version").to_owned();
    assert_eq!(send("GET", providers, None).await.0, StatusCode::NOT_FOUND);

    // Enabled: no restart, the page is listed and the routes answer.
    let (status, _) = send("POST", &format!("/Plugins/{plugin}/{version}/Enable"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, pages) = send("GET", "/web/ConfigurationPages", None).await;
    assert!(
        pages
            .as_array()
            .expect("pages")
            .iter()
            .any(|page| page["Name"] == SETTINGS_PAGE_NAME
                && page["DisplayName"] == "Single Sign-On"),
        "{pages}"
    );
    let (status, list) = send("GET", providers, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list, serde_json::json!({"PublicUrl": "", "Providers": []}));

    // Added with a secret, tested, then listed without it.
    let provider = serde_json::json!({
        "Name": "Keycloak",
        "Issuer": issuer,
        "AllowHttpIssuer": true,
        "ClientId": "ferrofin",
        "ClientSecret": SECRET,
    });
    let (status, saved) = send("PUT", keycloak, Some(provider.clone())).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["HasClientSecret"], true);
    assert_eq!(
        saved["CallbackUrl"],
        "https://media.example.org/jf/sso/oidc/keycloak/callback"
    );
    assert_eq!(
        saved["Discovery"],
        serde_json::json!({
            "AuthorizationEndpoint": format!("{issuer}/protocol/openid-connect/auth"),
            "TokenEndpoint": format!("{issuer}/protocol/openid-connect/token"),
            "UserinfoEndpoint": format!("{issuer}/protocol/openid-connect/userinfo"),
            "JwksUri": format!("{issuer}/protocol/openid-connect/certs"),
            "SigningKeys": 1,
        }),
        "{saved}"
    );
    assert!(list_has_no_discovery(&send("GET", providers, None).await.1));
    let (_, list) = send("GET", providers, None).await;
    assert_eq!(list["Providers"][0]["Key"], "keycloak");
    assert_eq!(list["Providers"][0]["HasClientSecret"], true);
    let (status, configuration) =
        send("GET", &format!("/Plugins/{plugin}/Configuration"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(configuration["Providers"][0]["Name"], "Keycloak");

    // The private file holds it, readable by the server's user only.
    let private = config
        .config_dir
        .join("plugins")
        .join(EXTENSION_ID.to_string())
        .join("private.json");
    assert!(
        std::fs::read_to_string(&private)
            .expect("private file")
            .contains(SECRET)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&private)
            .expect("mode")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    // An edit without the secret keeps it.
    let mut edit = provider.clone();
    edit.as_object_mut().expect("object").remove("ClientSecret");
    edit["Name"] = serde_json::json!("Home Keycloak");
    let (status, saved) = send("PUT", keycloak, Some(edit)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["Name"], "Home Keycloak");
    assert_eq!(saved["HasClientSecret"], true);
    assert!(
        std::fs::read_to_string(&private)
            .expect("private file")
            .contains(SECRET)
    );
    // An http issuer is refused unless its switch is on, and is not fetched.
    let asked = fake.requests().len();
    let mut http = provider.clone();
    http["AllowHttpIssuer"] = serde_json::json!(false);
    let (status, refused) = send("PUT", keycloak, Some(http)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(fake.requests().len(), asked);

    // A failed test still saves, and says why.
    fake.set(
        &fake.path("/.well-known/openid-configuration"),
        FakeResponse::status(404),
    );
    let mut renamed = provider.clone();
    renamed["Name"] = serde_json::json!("Renamed Keycloak");
    let (status, saved) = send("PUT", keycloak, Some(renamed)).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert!(
        saved["Discovery"]["Error"]
            .as_str()
            .is_some_and(|why| why.contains("HTTP status code 404")),
        "{saved}"
    );
    assert_eq!(saved["Discovery"].get("AuthorizationEndpoint"), None);
    let (_, list) = send("GET", providers, None).await;
    assert_eq!(list["Providers"][0]["Name"], "Renamed Keycloak");
    fake.set(
        &fake.path("/.well-known/openid-configuration"),
        FakeResponse::json(&fake.discovery_document()),
    );

    // Deleted, with its secret.
    assert_eq!(
        send("DELETE", keycloak, None).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, list) = send("GET", providers, None).await;
    assert_eq!(list["Providers"], serde_json::json!([]));
    assert!(
        !std::fs::read_to_string(&private)
            .expect("private file")
            .contains(SECRET)
    );

    // Disabled again with a provider configured: the routes are gone, and
    // the login page's branding is what it was (B1).
    assert_eq!(
        send("PUT", keycloak, Some(provider.clone())).await.0,
        StatusCode::OK
    );
    let (status, _) = send(
        "POST",
        &format!("/Plugins/{plugin}/{version}/Disable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let asked = fake.requests().len();
    for (method, uri) in [("GET", providers), ("PUT", keycloak), ("DELETE", keycloak)] {
        let body = (method == "PUT").then(|| provider.clone());
        assert_eq!(
            send(method, uri, body).await.0,
            StatusCode::NOT_FOUND,
            "{method} {uri}"
        );
    }
    assert_eq!(branding().await, branding_before);
    assert_eq!(
        fake.requests().len(),
        asked,
        "a disabled plugin calls out to no one"
    );

    // No answer held the secret, and discovery never sent it.
    let answered = answers.lock().expect("answers").clone();
    assert!(!answered.contains(SECRET), "{answered}");
    for request in fake.requests() {
        assert!(
            !String::from_utf8_lossy(&request.body).contains(SECRET)
                && !request
                    .headers
                    .iter()
                    .any(|(_, value)| value.contains(SECRET)),
            "{request:?}"
        );
    }
}

/// Whether no listed provider carries a discovery result: only a save's
/// answer does.
fn list_has_no_discovery(list: &serde_json::Value) -> bool {
    list["Providers"]
        .as_array()
        .is_some_and(|providers| providers.iter().all(|p| p.get("Discovery").is_none()))
}
