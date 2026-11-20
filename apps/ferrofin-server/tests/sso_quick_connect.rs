//! A native app's Quick Connect code approved through Single Sign-On (B13,
//! D8), through the composition root and the real router (`tower` oneshot),
//! against the fake identity provider: the app's `POST /QuickConnect/Initiate`,
//! the browser's code-entry page `GET /sso/quickconnect`, its
//! `GET /sso/oidc/{provider}/start?quickConnect=CODE`, the provider's
//! authorization endpoint, the callback's confirmation page and its
//! `POST /sso/quickconnect`. The app then sees its request authorized and
//! swaps its secret for a session of the SSO account; the browser gets none.
//! Nothing is approved without the confirmation, a confirmation works once
//! and only with its cookie, and Quick Connect turned off refuses.

#[path = "support/sso.rs"]
mod sso;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use ferrofin_extensions::sso::fake_idp::{FakeIdentityProvider, USERNAME};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_traits::devices::DeviceQuery;
use sso::{SsoServer, header_text, text};

const PUBLISHED_URL: &str = "https://media.example.org/jf";

/// The flow cookie, cleared.
const CLEARED: &str = "ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure";

/// The native app's client header, as Swiftfin sends it.
const APP: &str =
    r#"MediaBrowser Client="Swiftfin", Device="iPhone", DeviceId="qc-app-1", Version="1.6.1""#;

/// The server with `fake` saved as the provider `fake`.
async fn server_with_provider(fake: &FakeIdentityProvider, temp: &tempfile::TempDir) -> SsoServer {
    let server = sso::server(temp, PUBLISHED_URL).await;
    server
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
                client_secret: Some(Secret::new("sso-quick-connect-client-secret")),
                ..OidcProvider::default()
            },
        )
        .await
        .expect("saved");
    server
}

/// `response`'s body, as JSON.
async fn json(response: Response) -> serde_json::Value {
    serde_json::from_str(&text(response).await).expect("a JSON body")
}

/// The app's `POST /QuickConnect/Initiate`: the code it shows and its
/// secret.
async fn initiate(server: &SsoServer) -> (String, String) {
    let response = server
        .send(
            Request::builder()
                .method("POST")
                .uri("/QuickConnect/Initiate")
                .header(header::AUTHORIZATION, APP)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result = json(response).await;
    (
        result["Code"].as_str().expect("a code").to_owned(),
        result["Secret"].as_str().expect("a secret").to_owned(),
    )
}

/// The app's poll, `GET /QuickConnect/Connect`: whether its request is
/// authorized.
async fn authorized(server: &SsoServer, secret: &str) -> bool {
    let response = server
        .get(&format!("/QuickConnect/Connect?secret={secret}"), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await["Authenticated"]
        .as_bool()
        .expect("a flag")
}

/// The browser submits `code` on the code-entry page's button for `fake`,
/// signs in at `fake` and is sent back: the callback's page, and the flow
/// cookie's `name=value`.
async fn confirmation_page(
    server: &SsoServer,
    fake: &FakeIdentityProvider,
    code: &str,
) -> (Response, String) {
    let page = server.get("/sso/quickconnect", None).await;
    assert_eq!(page.status(), StatusCode::OK);
    let html = text(page).await;
    // The form's button for `fake` starts there, relative to the page.
    assert!(html.contains("formaction=\"oidc/fake/start\""), "{html}");
    assert!(html.contains("name=\"quickConnect\""), "{html}");
    let start = server
        .get(&format!("/sso/oidc/fake/start?quickConnect={code}"), None)
        .await;
    assert_eq!(start.status(), StatusCode::FOUND);
    let cookie = header_text(&start, header::SET_COOKIE);
    let cookie = cookie.split(';').next().expect("name=value").to_owned();
    let back = fake.authorize(&header_text(&start, header::LOCATION)).await;
    let callback = back
        .strip_prefix(PUBLISHED_URL)
        .unwrap_or_else(|| panic!("back to the published URL: {back}"));
    (server.get(callback, Some(&cookie)).await, cookie)
}

/// The confirmation value a confirmation page's form posts.
fn confirmation_of(html: &str) -> String {
    html.split_once("name=\"confirmation\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(
            || panic!("no confirmation in {html}"),
            |(value, _)| value.to_owned(),
        )
}

/// The browser's `POST /sso/quickconnect`: the confirmation page's form,
/// with `cookie` (`name=value`) when given.
async fn confirm(server: &SsoServer, confirmation: &str, cookie: Option<&str>) -> Response {
    let mut request = Request::builder()
        .method("POST")
        .uri("/sso/quickconnect")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    server
        .send(
            request
                .body(Body::from(format!("confirmation={confirmation}")))
                .expect("request"),
        )
        .await
}

/// The device rows (signed-in sessions) the server holds.
async fn devices(server: &SsoServer) -> Vec<String> {
    server
        .state
        .devices
        .get_devices(&DeviceQuery::default())
        .await
        .expect("devices")
        .items
        .into_iter()
        .map(|device| device.device_id)
        .collect()
}

#[tokio::test]
async fn an_app_is_signed_in_as_the_sso_user_once_the_browser_confirms_its_code() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    let (code, secret) = initiate(&server).await;

    // The callback asks, naming the code and the account, and keeps the
    // cookie for the confirmation's minute.
    let (page, cookie) = confirmation_page(&server, &fake, &code).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        header_text(&page, header::SET_COOKIE),
        format!("{cookie}; Path=/jf/sso; Max-Age=60; HttpOnly; SameSite=Lax; Secure")
    );
    assert_eq!(
        header_text(&page, header::CONTENT_SECURITY_POLICY),
        "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    );
    let html = text(page).await;
    assert!(
        html.contains(&format!(
            "Sign in the device showing code <strong>{code}</strong> as <strong>{USERNAME}</strong>?"
        )),
        "{html}"
    );
    assert!(
        html.contains("<form method=\"post\" action=\"../../quickconnect\">"),
        "{html}"
    );
    let confirmation = confirmation_of(&html);

    // Nothing is approved until the browser confirms.
    assert!(!authorized(&server, &secret).await);
    assert!(devices(&server).await.is_empty(), "no session yet");

    // The confirmation is no browser sign-in: the exchange refuses it, and
    // that uses it up.
    let exchanged = server
        .exchange(&confirmation, Some(&cookie), "sso-browser-1", None)
        .await;
    assert_eq!(exchanged.status(), StatusCode::BAD_REQUEST);
    assert!(devices(&server).await.is_empty(), "no browser session");
    let used = confirm(&server, &confirmation, Some(&cookie)).await;
    assert_eq!(used.status(), StatusCode::BAD_REQUEST);
    assert!(!authorized(&server, &secret).await);

    // Start again: this time the browser confirms.
    let (page, cookie) = confirmation_page(&server, &fake, &code).await;
    let confirmation = confirmation_of(&text(page).await);
    let approved = confirm(&server, &confirmation, Some(&cookie)).await;
    assert_eq!(approved.status(), StatusCode::OK);
    assert_eq!(header_text(&approved, header::SET_COOKIE), CLEARED);
    assert_eq!(header_text(&approved, header::CACHE_CONTROL), "no-store");
    assert!(
        text(approved)
            .await
            .contains("Your app is signed in. You can close this page."),
    );

    // The app sees it, and swaps its secret for a session of the SSO
    // account on its own device; the browser has none.
    assert!(authorized(&server, &secret).await);
    let session = server
        .send(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateWithQuickConnect")
                .header(header::AUTHORIZATION, APP)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Secret": secret }).to_string(),
                ))
                .expect("request"),
        )
        .await;
    assert_eq!(session.status(), StatusCode::OK);
    let session = json(session).await;
    assert_eq!(session["User"]["Name"], USERNAME);
    assert_eq!(session["SessionInfo"]["DeviceId"], "qc-app-1");
    assert_eq!(session["SessionInfo"]["Client"], "Swiftfin");
    assert!(
        session["AccessToken"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );
    assert_eq!(devices(&server).await, ["qc-app-1"]);

    // A replayed confirmation does nothing.
    let replayed = confirm(&server, &confirmation, Some(&cookie)).await;
    assert_eq!(replayed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(devices(&server).await, ["qc-app-1"]);
}

#[tokio::test]
async fn a_confirmation_without_its_cookie_approves_nothing() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    let (code, secret) = initiate(&server).await;
    let (page, _) = confirmation_page(&server, &fake, &code).await;
    let confirmation = confirmation_of(&text(page).await);

    let stolen = confirm(&server, &confirmation, None).await;
    assert_eq!(stolen.status(), StatusCode::BAD_REQUEST);
    let other = confirm(&server, &confirmation, Some("ferrofin_sso_flow=other")).await;
    assert_eq!(other.status(), StatusCode::BAD_REQUEST);
    assert!(!authorized(&server, &secret).await);
    assert_eq!(devices(&server).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_code_that_is_not_a_quick_connect_code_never_reaches_the_identity_provider() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    for code in ["", "12345", "1234567", "12a456", "%3Cb%3E123456"] {
        let start = server
            .get(&format!("/sso/oidc/fake/start?quickConnect={code}"), None)
            .await;
        assert_eq!(start.status(), StatusCode::BAD_REQUEST, "{code:?}");
        assert!(
            start.headers().get(header::SET_COOKIE).is_none(),
            "{code:?}"
        );
        let html = text(start).await;
        assert!(html.contains("six digits"), "{code:?}: {html}");
        assert!(!html.contains("<b>"), "{code:?}: {html}");
    }
}

#[tokio::test]
async fn quick_connect_turned_off_refuses_the_page_the_start_and_the_confirmation() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    let (code, _) = initiate(&server).await;
    let (page, cookie) = confirmation_page(&server, &fake, &code).await;
    let confirmation = confirmation_of(&text(page).await);

    let mut config = (*server.state.config.configuration().await.expect("config")).clone();
    config.quick_connect_available = false;
    server
        .state
        .config
        .update_configuration(&config)
        .await
        .expect("saved");

    for response in [
        server.get("/sso/quickconnect", None).await,
        server
            .get(&format!("/sso/oidc/fake/start?quickConnect={code}"), None)
            .await,
        confirm(&server, &confirmation, Some(&cookie)).await,
    ] {
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            text(response)
                .await
                .contains("Quick Connect is turned off on this server."),
        );
    }
    assert_eq!(devices(&server).await, Vec::<String>::new());
}
