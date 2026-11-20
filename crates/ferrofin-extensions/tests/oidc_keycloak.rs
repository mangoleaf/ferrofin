//! The Single Sign-On extension against a real identity provider: a Keycloak
//! holding the test realm of `contrib/oidc-test-realm/`.
//!
//! `#[ignore]`d, so a default run lists it as skipped; CI does not run it
//! (D12). It reads the realm's values from the environment and returns early
//! with a message when they are unset:
//!
//! ```bash
//! set -a; . ~/.config/ferrofin/oidc-test.env; set +a   # or export them by hand
//! cargo test -p ferrofin-extensions --test oidc_keycloak -- --include-ignored --nocapture
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

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_core::{FerrofinPluginManager, FerrofinUserManager};
use ferrofin_extensions::sso::accounts::{PROVIDER_ID, SsoAuthProvider};
use ferrofin_extensions::sso::{EXTENSION_ID, SsoExtension, SsoService};
use ferrofin_extensions::{Extension, registered_plugins};
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcDiscovery, OidcProvider};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::sso::{OidcCallback, SsoManager, SsoReply};

/// The realm's values, or `None` while any is unset or empty.
struct Realm {
    issuer: String,
    client_id: String,
    client_secret: String,
}

fn realm() -> Option<Realm> {
    let read = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    Some(Realm {
        issuer: read("FERROFIN_OIDC_TEST_ISSUER")?,
        client_id: read("FERROFIN_OIDC_TEST_CLIENT_ID")?,
        client_secret: read("FERROFIN_OIDC_TEST_CLIENT_SECRET")?,
    })
}

/// The Single Sign-On service over a real plugin manager in `dir` and a fresh
/// database's accounts, with the plugin enabled, its provider registered with
/// the accounts, and the realm's redirect URIs' host as its published URL;
/// and the accounts.
async fn service(dir: &tempfile::TempDir) -> (SsoService, Arc<FerrofinUserManager>) {
    let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(SsoExtension)];
    let plugins = Arc::new(FerrofinPluginManager::new(
        registered_plugins(&extensions),
        dir.path().to_path_buf(),
    ));
    plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
    let db = ferrofin_db::Database::connect_in_memory()
        .await
        .expect("connect");
    db.run_migrations().await.expect("migrations");
    let users = Arc::new(FerrofinUserManager::new(db.clone()));
    users.set_extension_providers(vec![Arc::new(SsoAuthProvider::new(
        Arc::clone(&plugins) as Arc<dyn PluginManager>
    ))]);
    let sso = SsoService::new(
        plugins as Arc<dyn PluginManager>,
        Arc::clone(&users) as Arc<dyn UserManager>,
        Arc::new(FerrofinExternalIdentityStore::new(db)),
        Some(PUBLISHED_URL.to_owned()),
    );
    (sso, users)
}

/// The published URL the callback URL is built on: an address the realm's
/// redirect URIs (`http://127.0.0.1:*`) accept. Nothing listens there: the
/// test reads the callback from Keycloak's redirect.
const PUBLISHED_URL: &str = "http://127.0.0.1:8096";

/// Where a browser signing in sits.
const BROWSER: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Saves the realm's client as the provider `key` with `issuer`, and returns
/// what testing it found, as a `PUT` does.
async fn save_and_test(sso: &SsoService, realm: &Realm, key: &str, issuer: &str) -> OidcDiscovery {
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
    let dir = tempfile::tempdir().expect("temp dir");
    let (sso, _users) = service(&dir).await;

    let found = save_and_test(&sso, &realm, "keycloak", &realm.issuer).await;
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
    let found = save_and_test(&sso, &realm, "missing", &missing).await;
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
    let found = save_and_test(&sso, &realm, "slashed", &slashed).await;
    assert!(
        found
            .error
            .as_deref()
            .is_some_and(|why| why.contains("another issuer")),
        "{found:?}"
    );
    println!("issuer with a trailing slash: {:?}", found.error);
}

/// A browser without a cookie jar: Keycloak's cookies are carried by hand.
fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client")
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

/// The value of the flow cookie `reply` sets.
fn flow_cookie(reply: &SsoReply) -> String {
    reply.set_cookies[0]
        .strip_prefix("ferrofin_sso_flow=")
        .and_then(|rest| rest.split(';').next())
        .expect("the flow cookie")
        .to_owned()
}

/// Signs `user` in at Keycloak's login form, from the authorization URL
/// `location` start sent the browser to, and answers the callback URL
/// Keycloak sends the browser back to.
async fn log_in(location: &str, user: &str, password: &str) -> String {
    let browser = browser();
    let page = browser.get(location).send().await.expect("the login page");
    assert_eq!(page.status(), 200, "the login page");
    let jar = cookies(&page).join("; ");
    let html = page.text().await.expect("the login page");
    let form = regex::Regex::new(r#"<form\b[^>]*\bid="kc-form-login"[^>]*>"#)
        .expect("a regex")
        .find(&html)
        .expect("the login form")
        .as_str();
    let action = regex::Regex::new(r#"\baction="([^"]+)""#)
        .expect("a regex")
        .captures(form)
        .expect("the form's action")[1]
        .replace("&amp;", "&");
    let answer = browser
        .post(action)
        .header("cookie", jar)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(
            url::form_urlencoded::Serializer::new(String::new())
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

/// The callback `url` and the flow `cookie` make, as the route reads them.
fn callback(url: &str, cookie: Option<String>) -> OidcCallback {
    let query: std::collections::HashMap<String, String> = url::Url::parse(url)
        .expect("a URL")
        .query_pairs()
        .into_owned()
        .collect();
    OidcCallback {
        state: query.get("state").cloned(),
        code: query.get("code").map(Secret::new),
        iss: query.get("iss").cloned(),
        error: query.get("error").cloned(),
        flow_cookie: cookie.map(Secret::new),
    }
}

/// The account `user` signed in to was created under the provider that
/// refuses passwords: not even the realm password, nor an empty one, signs in
/// to it, and the refusals do not count toward its lockout.
async fn refuses_passwords(users: &FerrofinUserManager, user: &str, password: &str) {
    let account = users
        .get_user_by_name(user)
        .await
        .expect("lookup")
        .expect("the account was created");
    assert_eq!(account.authentication_provider_id, PROVIDER_ID);
    for attempt in ["", password] {
        let answer = users
            .authenticate_user(user, attempt, "127.0.0.1", true)
            .await
            .expect("an answer");
        assert!(answer.is_none(), "a password is refused");
    }
    let account = users
        .get_user_by_name(user)
        .await
        .expect("lookup")
        .expect("kept");
    assert_eq!(account.invalid_login_attempt_count, 0, "not counted");
}

#[tokio::test]
#[ignore = "needs a Keycloak holding the ferrofin-test realm; run with --ignored"]
async fn a_realm_user_signs_in_through_the_login_form_and_only_once() {
    let read = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
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
    let dir = tempfile::tempdir().expect("temp dir");
    let (sso, users) = service(&dir).await;
    let found = save_and_test(&sso, &realm, "keycloak", &realm.issuer).await;
    assert_eq!(found.error, None, "{found:?}");
    let callback_url = format!("{PUBLISHED_URL}/sso/oidc/keycloak/callback?");

    let start = sso
        .start_oidc_sign_in("keycloak", BROWSER)
        .await
        .expect("starts");
    assert_eq!(start.status, 302, "start redirects");
    let location = start.location.clone().expect("to Keycloak");
    let back = log_in(&location, &user, &password).await;
    assert!(back.starts_with(&callback_url), "back to the callback");
    let signed_in = callback(&back, Some(flow_cookie(&start)));
    assert_eq!(
        signed_in.iss.as_deref(),
        Some(realm.issuer.as_str()),
        "RFC 9207"
    );
    let page = sso
        .finish_oidc_sign_in("keycloak", signed_in.clone())
        .await
        .expect("an answer");
    assert_eq!(page.status, 200, "the callback is accepted");
    assert!(
        page.html
            .contains(&format!("<strong>{user}</strong> is ready")),
        "the page names the account"
    );
    refuses_passwords(&users, &user, &password).await;
    println!("verified: the realm user's account was created and refuses passwords");

    // The same callback again: its flow is used up.
    let again = sso
        .finish_oidc_sign_in("keycloak", signed_in)
        .await
        .expect("an answer");
    assert_eq!(again.status, 400, "a repeated callback is refused");

    // Started in one browser, finished in another (no flow cookie).
    let start = sso
        .start_oidc_sign_in("keycloak", BROWSER)
        .await
        .expect("starts");
    let back = log_in(
        start.location.as_deref().expect("to Keycloak"),
        &user,
        &password,
    )
    .await;
    let elsewhere = sso
        .finish_oidc_sign_in("keycloak", callback(&back, None))
        .await
        .expect("an answer");
    assert_eq!(elsewhere.status, 400, "another browser is refused");

    // A second sign-in finds the same account by its link.
    let start = sso
        .start_oidc_sign_in("keycloak", BROWSER)
        .await
        .expect("starts");
    let back = log_in(
        start.location.as_deref().expect("to Keycloak"),
        &user,
        &password,
    )
    .await;
    let page = sso
        .finish_oidc_sign_in("keycloak", callback(&back, Some(flow_cookie(&start))))
        .await
        .expect("an answer");
    assert_eq!(page.status, 200, "signed in again");
    assert_eq!(
        users.get_users().await.expect("users").len(),
        1,
        "one account"
    );
}
