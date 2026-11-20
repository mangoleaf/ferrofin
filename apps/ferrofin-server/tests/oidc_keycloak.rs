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
//!   form.
//!
//! The other test users (`FERROFIN_OIDC_TEST_*_USER` and `…_PASSWORD`, listed
//! in the realm's README) are for the later sign-in phases.

#[path = "support/sso.rs"]
mod sso;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_extensions::sso::accounts::PROVIDER_ID;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcDiscovery, OidcProvider};
use sso::{SsoServer, exchange_code, header_text, text};

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

/// A sign-in by `user` from start to the completion page: the page's
/// response, its callback path, and the flow cookie's `name=value`.
async fn completion_page(
    server: &SsoServer,
    user: &str,
    password: &str,
    cookie: bool,
) -> (axum::response::Response, String, String) {
    let start = server.get("/sso/oidc/keycloak/start", None).await;
    assert_eq!(start.status(), StatusCode::FOUND, "start redirects");
    let flow = header_text(&start, header::SET_COOKIE)
        .split(';')
        .next()
        .expect("the flow cookie")
        .to_owned();
    let back = log_in(&header_text(&start, header::LOCATION), user, password).await;
    let callback = back
        .strip_prefix(PUBLISHED_URL)
        .unwrap_or_else(|| panic!("back to the callback: {back}"))
        .to_owned();
    assert!(
        callback.starts_with("/sso/oidc/keycloak/callback?"),
        "back to the callback"
    );
    let page = server.get(&callback, cookie.then_some(flow.as_str())).await;
    (page, callback, flow)
}

/// The account `user` signed in to was created under the provider that
/// refuses passwords: not even the realm password, nor an empty one, signs in
/// to it, and the refusals do not count toward its lockout.
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

/// The realm member signs in through Keycloak's login form, lands with a
/// token that opens an authenticated route, and can sign in only once per
/// code and only in the browser that started.
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
    let temp = tempfile::tempdir().expect("temp dir");
    let server = sso::server(&temp, PUBLISHED_URL).await;
    let found = save_and_test(&server, &realm, "keycloak", &realm.issuer).await;
    assert_eq!(found.error, None, "{found:?}");

    let (page, callback, flow) = completion_page(&server, &user, &password, true).await;
    assert_eq!(page.status(), StatusCode::OK, "the callback is accepted");
    let code = exchange_code(&text(page).await);
    let exchanged = server
        .exchange(&code, Some(&flow), "keycloak-live-test", None)
        .await;
    assert_eq!(exchanged.status(), StatusCode::OK, "the code is swapped");
    let result: serde_json::Value =
        serde_json::from_str(&text(exchanged).await).expect("a JSON body");
    assert_eq!(result["User"]["Name"], user.as_str());
    let token = result["AccessToken"].as_str().expect("a token").to_owned();

    // The token opens an authenticated route.
    let me = server
        .send(
            Request::builder()
                .uri("/Users/Me")
                .header(
                    header::AUTHORIZATION,
                    format!(
                        r#"MediaBrowser Client="Jellyfin Web", Device="Web Browser", DeviceId="keycloak-live-test", Version="12.4.0", Token="{token}""#
                    ),
                )
                .body(Body::empty())
                .expect("request"),
        )
        .await;
    assert_eq!(me.status(), StatusCode::OK, "GET /Users/Me with the token");
    let me: serde_json::Value = serde_json::from_str(&text(me).await).expect("a user");
    assert_eq!(me["Name"], user.as_str());
    println!("verified: the realm user signed in and the token opens GET /Users/Me");

    refuses_passwords(&server, &user, &password).await;

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
    let (elsewhere, _, _) = completion_page(&server, &user, &password, false).await;
    assert_eq!(
        elsewhere.status(),
        StatusCode::BAD_REQUEST,
        "another browser is refused"
    );

    // A second sign-in finds the same account by its link.
    let (page, _, flow) = completion_page(&server, &user, &password, true).await;
    assert_eq!(page.status(), StatusCode::OK, "signed in again");
    let code = exchange_code(&text(page).await);
    let second = server
        .exchange(&code, Some(&flow), "keycloak-live-test-2", None)
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    let accounts = server.state.users.get_users().await.expect("users");
    assert_eq!(
        accounts
            .iter()
            .filter(|account| account.authentication_provider_id == PROVIDER_ID)
            .count(),
        1,
        "one account"
    );
}
