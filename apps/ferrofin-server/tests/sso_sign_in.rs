//! An OIDC sign-in through the composition root and the real router (`tower`
//! oneshot), as a browser makes it, against the fake identity provider:
//! `/sso/oidc/{provider}/start`, the provider's authorization endpoint, then
//! the callback with the flow cookie start set. It reaches the page naming the
//! account it created; the same callback again, and one without the cookie,
//! are refused. That account then refuses every password at
//! `POST /Users/AuthenticateByName`, and its lockout counter never moves.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use ferrofin_db::Database;
use ferrofin_extensions::sso::EXTENSION_ID;
use ferrofin_extensions::sso::accounts::{PROVIDER_ID, PROVIDER_NAME};
use ferrofin_extensions::sso::fake_idp::{FakeIdentityProvider, USERNAME};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::library::UserManager;
use tower::ServiceExt as _;

const PUBLISHED_URL: &str = "https://media.example.org/jf";

async fn text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `response`'s header `name`, as text.
fn header_text(response: &Response, name: header::HeaderName) -> String {
    response.headers()[name].to_str().expect("text").to_owned()
}

/// The server, wired by the composition root, with the Single Sign-On plugin
/// enabled and `fake` saved as the provider `fake`: its router, its user
/// manager, and the shutdown channel's receiver, held while the router serves.
async fn server_with_provider(
    fake: &FakeIdentityProvider,
    temp: &tempfile::TempDir,
) -> (
    axum::Router,
    Arc<dyn UserManager>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let config = Config {
        server_name: "sso-sign-in-test".to_owned(),
        published_url: Some(PUBLISHED_URL.to_owned()),
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
    // The admin's steps, through the managers: `sso_settings` covers the routes.
    wired
        .state
        .plugins
        .enable_plugin(EXTENSION_ID)
        .await
        .expect("enable");
    wired
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service")
        .save_oidc_provider(
            "fake",
            OidcProvider {
                name: "Fake".to_owned(),
                issuer: fake.issuer(),
                allow_http_issuer: true,
                client_id: "ferrofin".to_owned(),
                client_secret: Some(Secret::new("sso-sign-in-client-secret")),
                ..OidcProvider::default()
            },
        )
        .await
        .expect("saved");
    let router = ferrofin_api::create_router(wired.state.clone());
    (router, Arc::clone(&wired.state.users), shutdown_rx)
}

#[tokio::test]
async fn a_browser_signs_in_through_the_router_once_and_only_with_its_cookie() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let (router, users, _shutdown) = server_with_provider(&fake, &temp).await;
    let send = |request: Request<Body>| {
        let router = router.clone();
        async move { router.oneshot(request).await.expect("response") }
    };
    let get = |uri: &str, cookie: Option<&str>| {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        request.body(Body::empty()).expect("request")
    };

    // A browser starts, signs in at the provider, and comes back with its
    // cookie: the path under the published URL's base path, as the router
    // sees it.
    let sign_in = || async {
        let start = send(get("/sso/oidc/fake/start", None)).await;
        assert_eq!(start.status(), StatusCode::FOUND);
        let cookie = header_text(&start, header::SET_COOKIE);
        assert!(
            cookie.ends_with("; Path=/jf/sso; Max-Age=600; HttpOnly; SameSite=Lax; Secure"),
            "{cookie}"
        );
        let cookie = cookie.split(';').next().expect("name=value").to_owned();
        let back = fake.authorize(&header_text(&start, header::LOCATION)).await;
        let callback = back
            .strip_prefix(PUBLISHED_URL)
            .unwrap_or_else(|| panic!("back to the published URL: {back}"))
            .to_owned();
        (callback, cookie)
    };

    let (callback, cookie) = sign_in().await;
    let page = send(get(&callback, Some(&format!("theme=dark; {cookie}")))).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        header_text(&page, header::SET_COOKIE),
        "ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
    );
    let html = text(page).await;
    assert!(
        html.contains(&format!("<strong>{USERNAME}</strong> is ready")),
        "{html}"
    );

    // The same callback again: its flow is used up.
    let again = send(get(&callback, Some(&cookie))).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(text(again).await.contains("Sign-in failed. Try again."));

    // Finished in another browser, without the cookie.
    let (callback, _) = sign_in().await;
    let elsewhere = send(get(&callback, None)).await;
    assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);

    // The account belongs to the provider that refuses passwords, which the
    // composition root registered and `GET /Auth/Providers` lists.
    let account = users
        .get_user_by_name(USERNAME)
        .await
        .expect("lookup")
        .expect("created");
    assert_eq!(account.authentication_provider_id, PROVIDER_ID);
    let listed = users.get_authentication_providers().await.expect("list");
    assert!(
        listed
            .iter()
            .any(|pair| pair.name.as_deref() == Some(PROVIDER_NAME)),
        "{listed:?}"
    );

    // No password signs in to it, not even an empty one, and the refusals
    // leave its lockout counter alone (D20).
    for password in ["", "guess"] {
        let attempt = Request::builder()
            .method("POST")
            .uri("/Users/AuthenticateByName")
            .header(
                header::AUTHORIZATION,
                r#"MediaBrowser Client="sso-sign-in-test", Device="fixture", DeviceId="sso-sign-in-test", Version="1""#,
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "Username": USERNAME, "Pw": password }).to_string(),
            ))
            .expect("request");
        assert_eq!(
            send(attempt).await.status(),
            StatusCode::UNAUTHORIZED,
            "{password:?}"
        );
    }
    let account = users
        .get_user_by_name(USERNAME)
        .await
        .expect("lookup")
        .expect("kept");
    assert_eq!(account.invalid_login_attempt_count, 0);
}
