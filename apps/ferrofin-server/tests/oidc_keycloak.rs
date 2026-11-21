//! The Single Sign-On plugin against a real identity provider: a Keycloak
//! holding the test realm of `contrib/oidc-test-realm/`, through the server
//! the composition root wires and its real router.
//!
//! `#[ignore]`d, so a default run lists it as skipped; CI does not run it
//! (D12). It reads the realm's values from the environment and returns early
//! with a message when they are unset:
//!
//! ```bash
//! set -a; . ~/.config/ferrofin/oidc-test.env; set +a   # or export them by hand
//! cargo test -p ferrofin-server --test oidc_keycloak -- --include-ignored --nocapture
//! ```
//!
//! - `FERROFIN_OIDC_TEST_ISSUER`: the realm's issuer, such as
//!   `https://keycloak.example.org/realms/ferrofin-test` (an `http` one turns
//!   the provider's "allow an http issuer" switch on, for a local container);
//! - `FERROFIN_OIDC_TEST_CLIENT_ID` and `FERROFIN_OIDC_TEST_CLIENT_SECRET`:
//!   the realm's confidential client;
//! - `FERROFIN_OIDC_TEST_MEMBER_USER` and `FERROFIN_OIDC_TEST_MEMBER_PASSWORD`:
//!   the realm user the sign-in test signs in, through Keycloak's own login
//!   form;
//! - the other test users (`FERROFIN_OIDC_TEST_ADMIN_*`, `…_GUEST_*`) and the
//!   groups (`FERROFIN_OIDC_TEST_ADMIN_GROUP`, `…_MEMBER_GROUP`), listed in
//!   the realm's README: the role and mapping tests sign all three users in,
//!   the link test the guest, and the provisioning and Quick Connect tests
//!   the member.
//!
//! Every test captures what the server logs meanwhile, at `debug` and its own
//! crates at `trace`, with each span's fields as the OTLP exporter would send
//! them (it adds only `otel.name` and the response status), and ends by
//! checking that no secret it handled is in it (B15): the client secret, also
//! as the token request's `Basic` credentials, the users' passwords, the
//! authorization code, `state`, the nonce, the flow cookie, the exchange code,
//! the session's token, a JWT, an identity's subject.

#[path = "support/sso.rs"]
mod sso;

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use ferrofin_extensions::sso::accounts::PROVIDER_ID;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcDiscovery, OidcFolderRole, OidcProvider};
use sso::{SsoServer, exchange_code, header_text, text};

/// Everything logged in this test process since its first test started:
/// one subscriber for the whole process, so the test's thread, blocking
/// tasks and the database's worker threads all write here.
static LOG: OnceLock<Log> = OnceLock::new();

/// The secrets one test saw on the way, which the process's log must not
/// hold.
#[derive(Default)]
struct Captured {
    secrets: Mutex<Vec<String>>,
}

impl Captured {
    /// Starts capturing, once per process, and answers this test's record.
    fn install() -> Self {
        LOG.get_or_init(|| {
            use tracing_subscriber::fmt::format::FmtSpan;
            let log = Log::default();
            // As the server's own subscriber, with the `log` bridge, so
            // dependencies logging through `log` are captured too.
            tracing_subscriber::fmt()
                .with_writer(log.clone())
                .with_env_filter(
                    "debug,ferrofin_extensions=trace,ferrofin_api=trace,ferrofin_core=trace,\
                     ferrofin_server=trace",
                )
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_ansi(false)
                .try_init()
                .expect("the test process's only subscriber");
            log
        });
        Self::default()
    }

    /// The process's log so far.
    fn log() -> String {
        let log = LOG.get().expect("installed");
        String::from_utf8_lossy(&log.0.lock().expect("lock")).into_owned()
    }

    /// Records `value` as a secret the log must not hold; a value shorter
    /// than 8 characters would match by chance and is not recorded.
    fn secret(&self, value: impl Into<String>) {
        let value = value.into();
        if value.len() >= 8 {
            self.secrets.lock().expect("lock").push(value);
        }
    }

    /// Records the secret query values of `url`, an authorization URL or a
    /// callback: the code, `state`, the nonce, the PKCE challenge, and
    /// Keycloak's session id.
    fn url_secrets(&self, url: &str) {
        let query = url.split_once('?').map_or("", |(_, query)| query);
        for (name, value) in form_urlencoded::parse(query.as_bytes()) {
            if ["code", "state", "nonce", "code_challenge", "session_state"]
                .contains(&name.as_ref())
            {
                self.secret(value);
            }
        }
    }

    /// Fails unless the log holds each of `lines`: the events that show a
    /// step was refused, or kept, for the reason the test means.
    fn assert_logged(lines: &[&str]) {
        let log = Self::log();
        for line in lines {
            assert!(log.contains(line), "logged: {line}");
        }
    }

    /// Fails when the log holds any recorded secret, the realm's client
    /// secret (as it is and as the token request's `Basic` credentials), a
    /// realm user's password or a JWT (a signed token's header starts
    /// `{"alg":`). Says which, by index, never the value.
    fn assert_no_secret_logged(&self, realm: &Realm) {
        let log = Self::log();
        assert!(
            log.contains("oidc_provider="),
            "the plugin's events were captured"
        );
        let mut secrets = self.secrets.lock().expect("lock").clone();
        let encoded =
            |value: &str| form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
        secrets.push(realm.client_secret.clone());
        secrets.push(encoded(&realm.client_secret));
        secrets.push(base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            encoded(&realm.client_id),
            encoded(&realm.client_secret)
        )));
        for role in ["ADMIN", "MEMBER", "GUEST"] {
            secrets.extend(read(&format!("FERROFIN_OIDC_TEST_{role}_PASSWORD")));
        }
        secrets.push("eyJhbGciOi".to_owned());
        let found: Vec<usize> = secrets
            .iter()
            .enumerate()
            .filter(|(_, secret)| log.contains(secret.as_str()))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            found,
            Vec::<usize>::new(),
            "secrets found in the log, by their index among the {} checked",
            secrets.len()
        );
        println!(
            "verified: none of {} secrets is in the {} bytes logged",
            secrets.len(),
            log.len()
        );
    }
}

/// The process's log, as the subscriber writes it.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The realm's values, or `None` while any is unset or empty.
struct Realm {
    issuer: String,
    client_id: String,
    client_secret: String,
}

/// The environment variable `name`, unless it is unset or empty.
fn read(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn realm() -> Option<Realm> {
    Some(Realm {
        issuer: read("FERROFIN_OIDC_TEST_ISSUER")?,
        client_id: read("FERROFIN_OIDC_TEST_CLIENT_ID")?,
        client_secret: read("FERROFIN_OIDC_TEST_CLIENT_SECRET")?,
    })
}

/// The published URL the callback URL is built on: an address the realm's
/// redirect URIs (`http://127.0.0.1:*`) accept. Nothing listens there: the
/// test reads the callback from Keycloak's redirect and hands it to the
/// router.
const PUBLISHED_URL: &str = "http://127.0.0.1:8096";

/// Saves the realm's client as the provider `key` with `issuer`, and returns
/// what testing it found, as a `PUT` does.
async fn save_and_test(
    server: &SsoServer,
    realm: &Realm,
    key: &str,
    issuer: &str,
) -> OidcDiscovery {
    let sso = server
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service");
    let saved = sso
        .save_oidc_provider(
            key,
            OidcProvider {
                name: "Keycloak".to_owned(),
                issuer: issuer.to_owned(),
                allow_http_issuer: issuer.starts_with("http://"),
                client_id: realm.client_id.clone(),
                client_secret: Some(Secret::new(realm.client_secret.clone())),
                ..OidcProvider::default()
            },
        )
        .await
        .expect("the provider saves");
    sso.test_oidc_provider(&saved.provider)
        .await
        .expect("a test result")
}

#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn discovery_finds_the_realm_and_refuses_a_wrong_issuer() {
    let Some(realm) = realm() else {
        println!(
            "skipped: set FERROFIN_OIDC_TEST_ISSUER, FERROFIN_OIDC_TEST_CLIENT_ID and \
             FERROFIN_OIDC_TEST_CLIENT_SECRET (contrib/oidc-test-realm/README.md)"
        );
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;

    let found = save_and_test(&server, &realm, "keycloak", &realm.issuer).await;
    assert_eq!(found.error, None, "{found:?}");
    let endpoint = format!("{}/protocol/openid-connect", realm.issuer);
    assert_eq!(
        found.authorization_endpoint,
        Some(format!("{endpoint}/auth"))
    );
    assert_eq!(found.token_endpoint, Some(format!("{endpoint}/token")));
    assert_eq!(
        found.userinfo_endpoint,
        Some(format!("{endpoint}/userinfo"))
    );
    assert_eq!(found.jwks_uri, Some(format!("{endpoint}/certs")));
    assert!(
        found.signing_keys.is_some_and(|keys| keys >= 1),
        "{found:?}"
    );
    assert_eq!(found.warnings, Vec::<String>::new());
    println!("discovery: {found:?}");

    // A realm that does not exist: Keycloak answers 404.
    let missing = format!("{}-missing", realm.issuer);
    let found = save_and_test(&server, &realm, "missing", &missing).await;
    assert!(
        found
            .error
            .as_deref()
            .is_some_and(|why| why.contains("404")),
        "{found:?}"
    );
    assert_eq!(found.authorization_endpoint, None);
    println!("missing realm: {:?}", found.error);

    // The realm's issuer with a trailing `/`: its document names the issuer
    // without one, and the issuer must match exactly.
    let slashed = format!("{}/", realm.issuer);
    let found = save_and_test(&server, &realm, "slashed", &slashed).await;
    assert!(
        found
            .error
            .as_deref()
            .is_some_and(|why| why.contains("another issuer")),
        "{found:?}"
    );
    println!("issuer with a trailing slash: {:?}", found.error);
    seen.assert_no_secret_logged(&realm);
}

/// The `name=value` of each cookie `answer` sets.
fn cookies(answer: &reqwest::Response) -> Vec<String> {
    answer
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next().map(str::to_owned))
        .collect()
}

/// The value of attribute `name` in the HTML tag `tag`.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].replace("&amp;", "&"))
}

/// Signs `user` in at Keycloak's login form, from the authorization URL
/// `location` start sent the browser to, and answers the callback URL
/// Keycloak sends the browser back to. A browser without a cookie jar:
/// Keycloak's cookies are carried by hand.
async fn log_in(location: &str, user: &str, password: &str) -> String {
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client");
    let page = browser.get(location).send().await.expect("the login page");
    assert_eq!(page.status(), 200, "the login page");
    let jar = cookies(&page).join("; ");
    let html = page.text().await.expect("the login page");
    let form = html
        .split("<form")
        .find(|tag| tag.contains("id=\"kc-form-login\""))
        .and_then(|form| form.split_once('>'))
        .map(|(tag, _)| tag)
        .expect("the login form");
    let action = attribute(form, "action").expect("the form's action");
    let answer = browser
        .post(action)
        .header("cookie", jar)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(
            form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("username", user),
                    ("password", password),
                    ("credentialId", ""),
                ])
                .finish(),
        )
        .send()
        .await
        .expect("the login form answers");
    assert_eq!(answer.status(), 302, "Keycloak signs the user in");
    answer.headers()["location"]
        .to_str()
        .expect("a location")
        .to_owned()
}

/// The callback path Keycloak sends the browser back to for a flow `start`
/// answered, once `user` signed in, and the flow cookie's `name=value`, each
/// recorded in `seen`.
async fn callback_of(
    start: &axum::response::Response,
    user: &str,
    password: &str,
    seen: &Captured,
) -> (String, String) {
    let flow = header_text(start, header::SET_COOKIE)
        .split(';')
        .next()
        .expect("the flow cookie")
        .to_owned();
    seen.secret(flow.split_once('=').map_or("", |(_, value)| value));
    let location = header_text(start, header::LOCATION);
    seen.url_secrets(&location);
    let back = log_in(&location, user, password).await;
    let callback = back
        .strip_prefix(PUBLISHED_URL)
        .unwrap_or_else(|| panic!("back to the callback: {back}"))
        .to_owned();
    seen.url_secrets(&callback);
    (callback, flow)
}

/// A sign-in by `user` from start to the completion page: the page's
/// response, its callback path, and the flow cookie's `name=value`.
async fn completion_page(
    server: &SsoServer,
    user: &str,
    password: &str,
    cookie: bool,
    seen: &Captured,
) -> (axum::response::Response, String, String) {
    let start = server.get("/sso/oidc/keycloak/start", None).await;
    assert_eq!(start.status(), StatusCode::FOUND, "start redirects");
    let (callback, flow) = callback_of(&start, user, password, seen).await;
    assert!(
        callback.starts_with("/sso/oidc/keycloak/callback?"),
        "back to the callback"
    );
    let page = server.get(&callback, cookie.then_some(flow.as_str())).await;
    (page, callback, flow)
}

/// `callback` with its query parameter `name` set to `value`.
fn with_parameter(callback: &str, name: &str, value: &str) -> String {
    let (path, query) = callback.split_once('?').expect("a query");
    let mut changed = form_urlencoded::Serializer::new(String::new());
    for (key, old) in form_urlencoded::parse(query.as_bytes()) {
        changed.append_pair(&key, if key == name { value } else { &old });
    }
    format!("{path}?{}", changed.finish())
}

/// `POST /Users/AuthenticateByName` as `name` from the device `device_id`:
/// its status and token.
async fn password_sign_in(
    server: &SsoServer,
    name: &str,
    password: &str,
    device_id: &str,
) -> (StatusCode, Option<String>) {
    let response = server
        .send(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    format!(
                        r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="{device_id}", Version="12.4.0""#
                    ),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": name, "Pw": password }).to_string(),
                ))
                .expect("request"),
        )
        .await;
    let status = response.status();
    let token = serde_json::from_str::<serde_json::Value>(&text(response).await)
        .ok()
        .and_then(|body| body["AccessToken"].as_str().map(str::to_owned));
    (status, token)
}

/// `method uri` with `token`, as the link page and a native app send it.
async fn call(
    server: &SsoServer,
    method: &str,
    uri: &str,
    token: &str,
) -> axum::response::Response {
    server
        .send(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(
                    header::AUTHORIZATION,
                    format!(r#"MediaBrowser Token="{token}""#),
                )
                .body(Body::empty())
                .expect("request"),
        )
        .await
}

/// `response`'s body, as JSON.
async fn json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&text(response).await).expect("a JSON body")
}

/// The account `user` signed in to was created under the provider that
/// refuses passwords: not even the realm password, nor an empty one, signs in
/// to it, through the user manager or `POST /Users/AuthenticateByName`, and
/// the refusals do not count toward its lockout (B11, D20).
async fn refuses_passwords(server: &SsoServer, user: &str, password: &str) {
    let account = server
        .state
        .users
        .get_user_by_name(user)
        .await
        .expect("lookup")
        .expect("the account was created");
    assert_eq!(account.authentication_provider_id, PROVIDER_ID);
    for attempt in ["", password] {
        let answer = server
            .state
            .users
            .authenticate_user(user, attempt, "127.0.0.1", true)
            .await
            .expect("an answer");
        assert!(answer.is_none(), "a password is refused");
        let (status, _) = password_sign_in(server, user, attempt, "password-browser").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a password is refused");
    }
    let account = server
        .state
        .users
        .get_user_by_name(user)
        .await
        .expect("lookup")
        .expect("kept");
    assert_eq!(account.invalid_login_attempt_count, 0, "not counted");
}

/// A callback whose `state` or `iss` is not what Keycloak sent is refused.
async fn refuses_tampered_callbacks(
    server: &SsoServer,
    user: &str,
    password: &str,
    seen: &Captured,
) {
    let start = server.get("/sso/oidc/keycloak/start", None).await;
    let (callback, flow) = callback_of(&start, user, password, seen).await;
    let tampered = with_parameter(&callback, "state", "tampered-state-value");
    let refused = server.get(&tampered, Some(&flow)).await;
    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "a tampered state"
    );
    assert!(callback.contains("iss="), "Keycloak sends iss (RFC 9207)");
    let tampered = with_parameter(&callback, "iss", "https://attacker.example.org/realms/x");
    let refused = server.get(&tampered, Some(&flow)).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "a tampered iss");
}

/// The realm member signs in through Keycloak's login form, lands with a
/// token that opens an authenticated route, and can sign in only once per
/// code, only in the browser that started, and only with the callback
/// Keycloak sent: a tampered `state` or `iss` is refused. No password signs
/// in to the account it created, over the route either.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn a_realm_user_signs_in_through_the_login_form_to_a_session() {
    let (Some(realm), Some(user), Some(password)) = (
        realm(),
        read("FERROFIN_OIDC_TEST_MEMBER_USER"),
        read("FERROFIN_OIDC_TEST_MEMBER_PASSWORD"),
    ) else {
        println!(
            "skipped: set FERROFIN_OIDC_TEST_ISSUER, FERROFIN_OIDC_TEST_CLIENT_ID, \
             FERROFIN_OIDC_TEST_CLIENT_SECRET, FERROFIN_OIDC_TEST_MEMBER_USER and \
             FERROFIN_OIDC_TEST_MEMBER_PASSWORD (contrib/oidc-test-realm/README.md)"
        );
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    let found = save_and_test(&server, &realm, "keycloak", &realm.issuer).await;
    assert_eq!(found.error, None, "{found:?}");

    let (page, callback, flow) = completion_page(&server, &user, &password, true, &seen).await;
    assert_eq!(page.status(), StatusCode::OK, "the callback is accepted");
    let code = exchange_code(&text(page).await);
    seen.secret(&code);
    let exchanged = server
        .exchange(&code, Some(&flow), "keycloak-live-test", None)
        .await;
    assert_eq!(exchanged.status(), StatusCode::OK, "the code is swapped");
    let result = json(exchanged).await;
    assert_eq!(result["User"]["Name"], user.as_str());
    let token = result["AccessToken"].as_str().expect("a token").to_owned();
    seen.secret(&token);

    // The token opens an authenticated route.
    let me = call(&server, "GET", "/Users/Me", &token).await;
    assert_eq!(me.status(), StatusCode::OK, "GET /Users/Me with the token");
    assert_eq!(json(me).await["Name"], user.as_str());
    println!("verified: the realm user signed in and the token opens GET /Users/Me");

    refuses_passwords(&server, &user, &password).await;
    println!("verified: no password signs in to the SSO account, and none counts toward lockout");

    // The same callback again, and the same code again, are refused.
    let again = server.get(&callback, Some(&flow)).await;
    assert_eq!(
        again.status(),
        StatusCode::BAD_REQUEST,
        "a repeated callback"
    );
    let replayed = server
        .exchange(&code, Some(&flow), "keycloak-live-test", None)
        .await;
    assert_eq!(
        replayed.status(),
        StatusCode::BAD_REQUEST,
        "a replayed code"
    );

    // Started in one browser, finished in another (no flow cookie).
    let (elsewhere, _, _) = completion_page(&server, &user, &password, false, &seen).await;
    assert_eq!(
        elsewhere.status(),
        StatusCode::BAD_REQUEST,
        "another browser is refused"
    );

    refuses_tampered_callbacks(&server, &user, &password, &seen).await;
    Captured::assert_logged(&[
        "http_request",
        "OIDC sign-in started",
        "unknown_state",
        "missing_cookie",
        "wrong_iss",
        "unknown_code",
    ]);
    println!(
        "verified: a replayed callback, a replayed code, another browser, a tampered state and a tampered iss are refused"
    );

    // A second sign-in finds the same account by its link.
    let (page, _, flow) = completion_page(&server, &user, &password, true, &seen).await;
    assert_eq!(page.status(), StatusCode::OK, "signed in again");
    let code = exchange_code(&text(page).await);
    seen.secret(&code);
    let second = server
        .exchange(&code, Some(&flow), "keycloak-live-test-2", None)
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    seen.secret(json(second).await["AccessToken"].as_str().expect("a token"));
    let accounts = server.state.users.get_users().await.expect("users");
    assert_eq!(
        accounts
            .iter()
            .filter(|account| account.authentication_provider_id == PROVIDER_ID)
            .count(),
        1,
        "one account"
    );
    println!("verified: a second sign-in finds the same account");
    identity_secrets(&server, &seen).await;
    seen.assert_no_secret_logged(&realm);
}

/// Records each linked identity's subject in `seen`: one claim value of the
/// claim set B15 keeps out of the log.
async fn identity_secrets(server: &SsoServer, seen: &Captured) {
    let sso = server
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service");
    for link in sso.all_oidc_links().await.expect("links") {
        seen.secret(link.subject);
    }
}

/// The realm user `FERROFIN_OIDC_TEST_{role}_USER` with its password.
fn realm_user(role: &str) -> Option<(String, String)> {
    Some((
        read(&format!("FERROFIN_OIDC_TEST_{role}_USER"))?,
        read(&format!("FERROFIN_OIDC_TEST_{role}_PASSWORD"))?,
    ))
}

/// A whole sign-in by `user` to a session of the device `device_id`: the
/// `AuthenticationResult`.
async fn signed_in(
    server: &SsoServer,
    (user, password): &(String, String),
    device_id: &str,
    seen: &Captured,
) -> serde_json::Value {
    let (page, _, flow) = completion_page(server, user, password, true, seen).await;
    assert_eq!(page.status(), StatusCode::OK, "{user} is let in");
    let code = exchange_code(&text(page).await);
    seen.secret(&code);
    let exchanged = server.exchange(&code, Some(&flow), device_id, None).await;
    assert_eq!(exchanged.status(), StatusCode::OK, "{user} gets a session");
    let result = json(exchanged).await;
    seen.secret(result["AccessToken"].as_str().expect("a token"));
    result
}

/// The realm's provider settings every test starts from: the realm's
/// client, under the key `keycloak`.
fn keycloak(realm: &Realm) -> OidcProvider {
    OidcProvider {
        name: "Keycloak".to_owned(),
        issuer: realm.issuer.clone(),
        allow_http_issuer: realm.issuer.starts_with("http://"),
        client_id: realm.client_id.clone(),
        client_secret: Some(Secret::new(realm.client_secret.clone())),
        ..OidcProvider::default()
    }
}

/// Saves `provider` as the provider `keycloak`.
async fn save(server: &SsoServer, provider: OidcProvider) {
    server
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service")
        .save_oidc_provider("keycloak", provider)
        .await
        .expect("the provider saves");
}

/// The test's realm, users and groups, or `None` (with the message saying
/// what to set) while any is unset.
struct Groups {
    realm: Realm,
    admin: (String, String),
    member: (String, String),
    guest: (String, String),
    admins: String,
    members: String,
}

fn groups() -> Option<Groups> {
    let found = (|| {
        Some(Groups {
            realm: realm()?,
            admin: realm_user("ADMIN")?,
            member: realm_user("MEMBER")?,
            guest: realm_user("GUEST")?,
            admins: read("FERROFIN_OIDC_TEST_ADMIN_GROUP")?,
            members: read("FERROFIN_OIDC_TEST_MEMBER_GROUP")?,
        })
    })();
    if found.is_none() {
        println!(
            "skipped: set FERROFIN_OIDC_TEST_ISSUER, _CLIENT_ID, _CLIENT_SECRET, the _USER and \
             _PASSWORD of ADMIN, MEMBER and GUEST, _ADMIN_GROUP and _MEMBER_GROUP \
             (contrib/oidc-test-realm/README.md)"
        );
    }
    found
}

/// B8 and B9 against the realm's groups, read from its top-level `groups`
/// claim: with the members' group required, the user in no group (whose ID
/// token has no `groups` claim at all) is refused and no account is made,
/// and the member signs in. With role sync on and the administrators' group
/// as the administrator role, the realm's administrator is an administrator
/// from their first session, and the member is not.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn the_realm_groups_gate_sign_in_and_set_the_administrator_flag() {
    let Some(Groups {
        realm,
        admin,
        member,
        guest,
        admins,
        members,
    }) = groups()
    else {
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    let provider = |required_role: &str, role_sync: bool| OidcProvider {
        required_role: required_role.to_owned(),
        role_sync,
        admin_role: admins.clone(),
        ..keycloak(&realm)
    };

    save(&server, provider(&members, false)).await;
    let (page, _, _) = completion_page(&server, &guest.0, &guest.1, true, &seen).await;
    assert_eq!(page.status(), StatusCode::FORBIDDEN, "the guest is refused");
    assert!(text(page).await.contains("does not have the role"));
    let users = &server.state.users;
    assert_eq!(
        users.get_user_by_name(&guest.0).await.expect("lookup"),
        None
    );
    let result = signed_in(&server, &member, "keycloak-member", &seen).await;
    assert_eq!(result["User"]["Policy"]["IsAdministrator"], false);
    println!("verified: the required group lets the member in and refuses the guest");

    save(&server, provider("", true)).await;
    let result = signed_in(&server, &admin, "keycloak-admin", &seen).await;
    assert_eq!(result["User"]["Policy"]["IsAdministrator"], true);
    let result = signed_in(&server, &member, "keycloak-member", &seen).await;
    assert_eq!(result["User"]["Policy"]["IsAdministrator"], false);
    println!("verified: role sync makes the realm's administrator one, and not the member");
    identity_secrets(&server, &seen).await;
    seen.assert_no_secret_logged(&realm);
}

/// B9 when the administrator role moves: with role sync on, a user loses
/// the administrator flag at the first sign-in whose roles lack the
/// administrator role, except the last administrator, who keeps it, and a
/// user who gains the role gains the flag. A demotion at the identity
/// provider (a user leaving the group) reaches Ferrofin the same way, as a
/// claim without the role; here the provider's administrator role moves from
/// one group to the other instead, so the test needs no admin login to
/// Keycloak.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn role_sync_moves_the_administrator_flag_and_keeps_the_last_administrator() {
    let Some(Groups {
        realm,
        admin,
        member,
        admins,
        members,
        ..
    }) = groups()
    else {
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    let synced = |admin_role: &str| OidcProvider {
        role_sync: true,
        admin_role: admin_role.to_owned(),
        all_folders_roles: vec![admins.clone(), members.clone()],
        ..keycloak(&realm)
    };
    let is_admin = |result: &serde_json::Value| result["User"]["Policy"]["IsAdministrator"].clone();
    let (yes, no) = (serde_json::json!(true), serde_json::json!(false));

    save(&server, synced(&admins)).await;
    let result = signed_in(&server, &admin, "keycloak-admin", &seen).await;
    assert_eq!(is_admin(&result), yes, "the administrators' group");
    assert_eq!(
        server.state.users.get_users().await.expect("users").len(),
        1,
        "the realm's administrator is the server's only account"
    );

    // The role moves to the members' group: the realm's administrator is the
    // last administrator, so keeps the flag.
    save(&server, synced(&members)).await;
    let result = signed_in(&server, &admin, "keycloak-admin", &seen).await;
    assert_eq!(is_admin(&result), yes, "the last administrator is kept");
    Captured::assert_logged(&["sign-in would demote the last administrator"]);
    let result = signed_in(&server, &member, "keycloak-member", &seen).await;
    assert_eq!(is_admin(&result), yes, "the member gains the flag");
    let result = signed_in(&server, &admin, "keycloak-admin", &seen).await;
    assert_eq!(is_admin(&result), no, "no longer the last: demoted");
    println!(
        "verified: role sync keeps the last administrator, promotes the member and then demotes the realm's administrator"
    );
    identity_secrets(&server, &seen).await;
    seen.assert_no_secret_logged(&realm);
}

/// The library the members' group is mapped to below, as
/// `GET /Library/MediaFolders` writes an id.
const LIBRARY: &str = "0b8e6c0e3c8d4a5f9e2b1d7c6a5f4e3d";

/// B10 against the realm's groups: with role sync on, the members' group
/// mapped to one library and the administrators' group to every library and
/// Live TV, each realm user's first session already has what their groups
/// give, and the user in no group gets no library and no Live TV. The
/// library ids need not exist: one no library has grants nothing.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn the_realm_groups_map_libraries_and_live_tv() {
    let Some(Groups {
        realm,
        admin,
        member,
        guest,
        admins,
        members,
    }) = groups()
    else {
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    save(
        &server,
        OidcProvider {
            role_sync: true,
            all_folders_roles: vec![admins.clone()],
            folder_roles: vec![OidcFolderRole {
                role: members,
                folders: vec![LIBRARY.parse().expect("a GUID")],
            }],
            live_tv_roles: vec![admins.clone()],
            live_tv_management_roles: vec![admins],
            ..keycloak(&realm)
        },
    )
    .await;

    let access = |result: &serde_json::Value| {
        let policy = &result["User"]["Policy"];
        (
            policy["EnableAllFolders"].clone(),
            policy["EnabledFolders"].clone(),
            policy["EnableLiveTvAccess"].clone(),
            policy["EnableLiveTvManagement"].clone(),
        )
    };
    let none = serde_json::json!([]);
    let (yes, no) = (serde_json::json!(true), serde_json::json!(false));
    let result = signed_in(&server, &member, "keycloak-member", &seen).await;
    assert_eq!(
        access(&result),
        (
            no.clone(),
            serde_json::json!([LIBRARY]),
            no.clone(),
            no.clone()
        ),
        "the member"
    );
    let result = signed_in(&server, &admin, "keycloak-admin", &seen).await;
    assert_eq!(
        access(&result),
        (yes.clone(), none.clone(), yes.clone(), yes),
        "the administrator"
    );
    let result = signed_in(&server, &guest, "keycloak-guest", &seen).await;
    assert_eq!(
        access(&result),
        (no.clone(), none, no.clone(), no),
        "the user in no group"
    );
    println!("verified: the realm's groups give the libraries and Live TV their mappings name");
    identity_secrets(&server, &seen).await;
    seen.assert_no_secret_logged(&realm);
}

/// B6 and D4: with provisioning off, a realm user no account is linked to is
/// refused and no account is made; with it on, a local account that has the
/// realm user's name makes that sign-in refused, and the local account is
/// neither linked nor changed, its password still signing in.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn provisioning_off_and_a_taken_name_are_refused_and_no_account_is_taken() {
    let (Some(realm), Some(member)) = (realm(), realm_user("MEMBER")) else {
        println!(
            "skipped: set FERROFIN_OIDC_TEST_ISSUER, _CLIENT_ID, _CLIENT_SECRET, _MEMBER_USER \
             and _MEMBER_PASSWORD (contrib/oidc-test-realm/README.md)"
        );
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    let users = &server.state.users;

    save(
        &server,
        OidcProvider {
            create_users: false,
            ..keycloak(&realm)
        },
    )
    .await;
    let (page, _, _) = completion_page(&server, &member.0, &member.1, true, &seen).await;
    assert_eq!(page.status(), StatusCode::FORBIDDEN, "provisioning off");
    assert!(
        text(page)
            .await
            .contains("this provider does not create accounts")
    );
    assert_eq!(users.get_users().await.expect("users"), Vec::new());
    println!("verified: with provisioning off the realm user is refused and no account is made");

    save(&server, keycloak(&realm)).await;
    let local = users.create_user(&member.0).await.expect("created");
    let local_id = uuid::Uuid::parse_str(&local.id).expect("id");
    let local_password = "a-local-password-for-the-same-name";
    users
        .change_password(local_id, local_password)
        .await
        .expect("password");
    let before = users
        .get_user_by_id(local_id)
        .await
        .expect("lookup")
        .expect("the local account");
    let (page, _, _) = completion_page(&server, &member.0, &member.1, true, &seen).await;
    assert_eq!(page.status(), StatusCode::FORBIDDEN, "a taken name");
    assert!(text(page).await.contains("already has your name"));
    assert_eq!(
        users.get_user_by_id(local_id).await.expect("lookup"),
        Some(before),
        "the local account is unchanged"
    );
    assert_eq!(users.get_users().await.expect("users").len(), 1);
    let sso = server
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service");
    assert_eq!(sso.all_oidc_links().await.expect("links"), Vec::new());
    let (status, token) = password_sign_in(&server, &member.0, local_password, "local").await;
    assert_eq!(status, StatusCode::OK, "the local password still signs in");
    seen.secret(token.expect("a token"));
    seen.secret(local_password);
    println!(
        "verified: a taken name is refused, and the local account is neither linked nor changed"
    );
    seen.assert_no_secret_logged(&realm);
}

/// B12 and D5: a local account signed in with its password links itself to
/// a realm identity at `/sso/oidc/{provider}/link`; the identity then signs
/// in to it, and so does its password; removing the link signs out every
/// session of the account, and the identity no longer signs in.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn a_local_account_links_a_realm_identity_and_unlinking_signs_it_out() {
    let (Some(realm), Some(guest)) = (realm(), realm_user("GUEST")) else {
        println!(
            "skipped: set FERROFIN_OIDC_TEST_ISSUER, _CLIENT_ID, _CLIENT_SECRET, _GUEST_USER \
             and _GUEST_PASSWORD (contrib/oidc-test-realm/README.md)"
        );
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    save(
        &server,
        OidcProvider {
            create_users: false,
            ..keycloak(&realm)
        },
    )
    .await;
    let users = &server.state.users;
    let local = users.create_user("linked-local").await.expect("created");
    let local_password = "the-linked-account-password";
    seen.secret(local_password);
    users
        .change_password(
            uuid::Uuid::parse_str(&local.id).expect("id"),
            local_password,
        )
        .await
        .expect("password");
    let (status, token) =
        password_sign_in(&server, "linked-local", local_password, "link-browser").await;
    assert_eq!(status, StatusCode::OK);
    let token = token.expect("a token");
    seen.secret(&token);

    // The link page's request, then Keycloak, then the callback.
    let started = call(&server, "POST", "/sso/oidc/keycloak/link", &token).await;
    assert_eq!(started.status(), StatusCode::OK, "the link starts");
    let set_cookie = header_text(&started, header::SET_COOKIE);
    let url = json(started).await["Url"]
        .as_str()
        .expect("a URL")
        .to_owned();
    let flow = set_cookie.split(';').next().expect("name=value").to_owned();
    seen.secret(flow.split_once('=').map_or("", |(_, value)| value));
    seen.url_secrets(&url);
    let back = log_in(&url, &guest.0, &guest.1).await;
    let callback = back.strip_prefix(PUBLISHED_URL).expect("back").to_owned();
    seen.url_secrets(&callback);
    let linked = server.get(&callback, Some(&flow)).await;
    assert_eq!(linked.status(), StatusCode::OK);
    assert!(text(linked).await.contains("Sign-in linked"));
    let links = json(call(&server, "GET", "/sso/links", &token).await).await;
    assert_eq!(links["Links"][0]["Provider"], "keycloak");
    assert_eq!(links["SingleSignOnAccount"], false);
    println!("verified: the local account linked itself to the realm identity");

    // The identity signs in to the local account, and its password still
    // does (D5).
    let result = signed_in(&server, &guest, "link-provider-browser", &seen).await;
    assert_eq!(result["User"]["Name"], "linked-local");
    let provider_token = result["AccessToken"].as_str().expect("a token").to_owned();
    let (status, second) =
        password_sign_in(&server, "linked-local", local_password, "link-second").await;
    assert_eq!(status, StatusCode::OK, "the password still signs in");
    let second = second.expect("a token");
    seen.secret(&second);
    println!("verified: the identity signs in to the linked account, and so does its password");

    // Unlinking ends every session of the account; the identity is refused.
    for token in [&token, &provider_token, &second] {
        assert_eq!(
            call(&server, "GET", "/Users/Me", token).await.status(),
            StatusCode::OK,
            "signed in before the removal"
        );
    }
    identity_secrets(&server, &seen).await;
    let removed = call(&server, "DELETE", "/sso/links/keycloak", &token).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    for token in [&token, &provider_token, &second] {
        assert_eq!(
            call(&server, "GET", "/Users/Me", token).await.status(),
            StatusCode::UNAUTHORIZED,
            "signed out"
        );
    }
    let (page, _, _) = completion_page(&server, &guest.0, &guest.1, true, &seen).await;
    assert_eq!(page.status(), StatusCode::FORBIDDEN, "no longer linked");
    let (status, _) =
        password_sign_in(&server, "linked-local", local_password, "link-browser").await;
    assert_eq!(status, StatusCode::OK, "the password still signs in");
    println!("verified: unlinking signed out all three sessions, and the identity is refused");
    seen.assert_no_secret_logged(&realm);
}

/// The native app's client header, as Swiftfin sends it.
const APP: &str = r#"MediaBrowser Client="Swiftfin", Device="iPhone", DeviceId="keycloak-qc-app", Version="1.6.1""#;

/// The native app's `POST uri`, with the JSON `body` when given.
async fn app_post(
    server: &SsoServer,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, APP)
        .header(header::CONTENT_TYPE, "application/json");
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    server.send(request.body(body).expect("request")).await
}

/// The device ids of the signed-in sessions the server holds.
async fn devices(server: &SsoServer) -> Vec<String> {
    server
        .state
        .devices
        .get_devices(&ferrofin_traits::devices::DeviceQuery::default())
        .await
        .expect("devices")
        .items
        .into_iter()
        .map(|device| device.device_id)
        .collect()
}

/// The browser's part of a Quick Connect approval of `code` by `user`, up to
/// the confirmation page: its confirmation value and the flow cookie's
/// `name=value`.
async fn confirmation_for(
    server: &SsoServer,
    code: &str,
    (user, password): &(String, String),
    seen: &Captured,
) -> (String, String) {
    let page = server.get("/sso/quickconnect", None).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        text(page)
            .await
            .contains("formaction=\"oidc/keycloak/start\"")
    );
    let start = server
        .get(
            &format!("/sso/oidc/keycloak/start?quickConnect={code}"),
            None,
        )
        .await;
    assert_eq!(start.status(), StatusCode::FOUND);
    let (callback, flow) = callback_of(&start, user, password, seen).await;
    let page = server.get(&callback, Some(&flow)).await;
    assert_eq!(page.status(), StatusCode::OK, "the confirmation page");
    let html = text(page).await;
    assert!(
        html.contains(&format!(
            "Sign in the device showing code <strong>{code}</strong> as <strong>{user}</strong>?"
        )),
        "{html}"
    );
    let confirmation = html
        .split_once("name=\"confirmation\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(value, _)| value.to_owned())
        .expect("a confirmation");
    seen.secret(&confirmation);
    (confirmation, flow)
}

/// B13 and D8: a native app's Quick Connect code is approved by a browser
/// that signs in at Keycloak through `/sso/quickconnect` and confirms; the
/// app is then signed in as the account the realm user signs in to, on its
/// own device, and the browser gets no session.
#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn a_native_apps_quick_connect_code_is_approved_through_keycloak() {
    let (Some(realm), Some(member)) = (realm(), realm_user("MEMBER")) else {
        println!(
            "skipped: set FERROFIN_OIDC_TEST_ISSUER, _CLIENT_ID, _CLIENT_SECRET, _MEMBER_USER \
             and _MEMBER_PASSWORD (contrib/oidc-test-realm/README.md)"
        );
        return;
    };
    let seen = Captured::install();
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    save(&server, keycloak(&realm)).await;

    // The app asks for a code.
    let initiated = app_post(&server, "/QuickConnect/Initiate", None).await;
    assert_eq!(initiated.status(), StatusCode::OK);
    let initiated = json(initiated).await;
    let code = initiated["Code"].as_str().expect("a code").to_owned();
    let secret = initiated["Secret"].as_str().expect("a secret").to_owned();
    seen.secret(&secret);
    let authorized = || async {
        json(
            server
                .get(&format!("/QuickConnect/Connect?secret={secret}"), None)
                .await,
        )
        .await["Authenticated"]
            .clone()
    };

    // The browser enters it on the code-entry page and signs in.
    let (confirmation, flow) = confirmation_for(&server, &code, &member, &seen).await;
    assert_eq!(
        authorized().await,
        false,
        "nothing approved before confirming"
    );

    // The browser confirms: the app is authorized.
    let approved = server
        .send(
            Request::builder()
                .method("POST")
                .uri("/sso/quickconnect")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, &flow)
                .body(Body::from(format!("confirmation={confirmation}")))
                .expect("request"),
        )
        .await;
    assert_eq!(approved.status(), StatusCode::OK);
    assert!(text(approved).await.contains("Your app is signed in"));
    assert_eq!(authorized().await, true, "approved");

    // The app swaps its secret for a session of the realm user's account.
    let session = app_post(
        &server,
        "/Users/AuthenticateWithQuickConnect",
        Some(serde_json::json!({ "Secret": secret })),
    )
    .await;
    assert_eq!(session.status(), StatusCode::OK);
    let session = json(session).await;
    assert_eq!(session["User"]["Name"], member.0.as_str());
    assert_eq!(session["SessionInfo"]["DeviceId"], "keycloak-qc-app");
    let token = session["AccessToken"].as_str().expect("a token").to_owned();
    seen.secret(&token);
    assert_eq!(
        call(&server, "GET", "/Users/Me", &token).await.status(),
        StatusCode::OK,
        "the app's token works"
    );
    assert_eq!(
        devices(&server).await,
        ["keycloak-qc-app"],
        "only the app is signed in"
    );
    println!(
        "verified: the app's Quick Connect code was approved through Keycloak, and only the app is signed in"
    );
    identity_secrets(&server, &seen).await;
    seen.assert_no_secret_logged(&realm);
}
