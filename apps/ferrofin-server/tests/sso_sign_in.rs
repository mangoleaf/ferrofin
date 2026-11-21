//! An OIDC sign-in through the composition root and the real router (`tower`
//! oneshot), as a browser and its completion page make it, against the fake
//! identity provider: `/sso/oidc/{provider}/start`, the provider's
//! authorization endpoint, the callback with the flow cookie start set, then
//! the completion page's `POST /sso/oidc/exchange`. It ends with a token that
//! opens an authenticated route; the same code again, and the callback
//! without the cookie, are refused. Each account rule of a password sign-in
//! refuses the exchange and writes the activity log, and that account refuses
//! every password at `POST /Users/AuthenticateByName` without its lockout
//! counter moving. It still does after a save of its user page gives it the
//! default provider (the plugin disabled, every extension off, or an admin's
//! choice), until an admin sets it a password.

#[path = "support/sso.rs"]
mod sso;

use std::net::{IpAddr, Ipv4Addr};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_core::auth_providers::DEFAULT_AUTH_PROVIDER_ID;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_extensions::sso::EXTENSION_ID;
use ferrofin_extensions::sso::accounts::{PROVIDER_ID, PROVIDER_NAME};
use ferrofin_extensions::sso::fake_idp::{FakeIdentityProvider, USERNAME};
use ferrofin_model::activity::ActivityLogEntry;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek, UserPolicy};
use ferrofin_server::config::Config;
use ferrofin_traits::activity::ActivityLogQuery;
use ferrofin_traits::devices::DeviceQuery;
use sso::{SsoServer, exchange_code, header_text, text};
use uuid::Uuid;

const PUBLISHED_URL: &str = "https://media.example.org/jf";

/// The flow cookie, cleared.
const CLEARED: &str = "ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure";

/// The server with `fake` saved as the provider `fake`.
async fn server_with_provider(fake: &FakeIdentityProvider, temp: &tempfile::TempDir) -> SsoServer {
    let server = sso::server(temp, PUBLISHED_URL).await;
    // The admin's step, through the manager: `sso_settings` covers the route.
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
                client_secret: Some(Secret::new("sso-sign-in-client-secret")),
                ..OidcProvider::default()
            },
        )
        .await
        .expect("saved");
    server
}

/// A browser starts a sign-in, signs in at `fake` and is sent back: the
/// callback's path under the published URL's base path, as the router sees
/// it, and the flow cookie's `name=value`.
async fn signed_in_at(server: &SsoServer, fake: &FakeIdentityProvider) -> (String, String) {
    let start = server.get("/sso/oidc/fake/start", None).await;
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
}

/// A whole sign-in at `fake` through the completion page's exchange, by the
/// device `device_id` at `peer`: the exchange's answer.
async fn sign_in(
    server: &SsoServer,
    fake: &FakeIdentityProvider,
    device_id: &str,
    peer: Option<IpAddr>,
) -> axum::response::Response {
    let (callback, cookie) = signed_in_at(server, fake).await;
    let page = server.get(&callback, Some(&cookie)).await;
    assert_eq!(page.status(), StatusCode::OK);
    let code = exchange_code(&text(page).await);
    server.exchange(&code, Some(&cookie), device_id, peer).await
}

/// The account the fake's identity signs in to.
async fn account(server: &SsoServer) -> UserEntity {
    server
        .state
        .users
        .get_user_by_name(USERNAME)
        .await
        .expect("lookup")
        .expect("the account")
}

/// The activity log, newest first.
async fn activity(server: &SsoServer) -> Vec<ActivityLogEntry> {
    server
        .state
        .activity
        .get_paged_result(&ActivityLogQuery::default())
        .await
        .expect("the log")
        .items
}

/// How many device rows (signed-in sessions) the account has.
async fn devices(server: &SsoServer) -> usize {
    let user = account(server).await;
    server
        .state
        .devices
        .get_devices(&DeviceQuery {
            user_id: Some(Uuid::parse_str(&user.id).expect("an id")),
            ..DeviceQuery::default()
        })
        .await
        .expect("devices")
        .items
        .len()
}

#[tokio::test]
async fn a_browser_signs_in_through_the_router_once_and_only_with_its_cookie() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;

    // The callback answers the completion page, which keeps the cookie for
    // the code's minute and may run its own script only.
    let (callback, cookie) = signed_in_at(&server, &fake).await;
    let page = server
        .get(&callback, Some(&format!("theme=dark; {cookie}")))
        .await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers().get(header::LOCATION).is_none());
    assert_eq!(
        header_text(&page, header::SET_COOKIE),
        format!("{cookie}; Path=/jf/sso; Max-Age=60; HttpOnly; SameSite=Lax; Secure")
    );
    let policy = header_text(&page, header::CONTENT_SECURITY_POLICY);
    assert!(
        policy.starts_with("default-src 'none'; script-src 'sha256-")
            && policy.ends_with("; frame-ancestors 'none'"),
        "{policy}"
    );
    assert_eq!(
        header_text(&page, header::X_CONTENT_TYPE_OPTIONS),
        "nosniff"
    );
    assert_eq!(header_text(&page, header::CACHE_CONTROL), "no-store");
    assert_eq!(header_text(&page, header::REFERRER_POLICY), "no-referrer");
    let code = exchange_code(&text(page).await);

    // The same callback again: its flow is used up.
    let again = server.get(&callback, Some(&cookie)).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(text(again).await.contains("Sign-in failed. Try again."));

    // The page's script swaps the code for `AuthenticateByName`'s answer.
    let exchanged = server
        .exchange(&code, Some(&cookie), "sso-browser-1", None)
        .await;
    assert_eq!(exchanged.status(), StatusCode::OK);
    assert_eq!(header_text(&exchanged, header::SET_COOKIE), CLEARED);
    assert_eq!(header_text(&exchanged, header::CACHE_CONTROL), "no-store");
    let result: serde_json::Value =
        serde_json::from_str(&text(exchanged).await).expect("a JSON body");
    assert_eq!(result["User"]["Name"], USERNAME);
    assert_eq!(result["SessionInfo"]["DeviceId"], "sso-browser-1");
    assert_eq!(result["SessionInfo"]["Client"], "Jellyfin Web");
    assert_eq!(result["SessionInfo"]["DeviceName"], "Web Browser");
    assert!(result["ServerId"].as_str().is_some_and(|id| !id.is_empty()));
    let token = result["AccessToken"].as_str().expect("a token").to_owned();

    // The token opens an authenticated route, as jellyfin-web uses it.
    let me = server
        .send(
            Request::builder()
                .uri("/Users/Me")
                .header(
                    header::AUTHORIZATION,
                    format!(
                        r#"MediaBrowser Client="Jellyfin Web", Device="Firefox", DeviceId="sso-browser-1", Version="12.4.0", Token="{token}""#
                    ),
                )
                .body(Body::empty())
                .expect("request"),
        )
        .await;
    assert_eq!(me.status(), StatusCode::OK);
    let me: serde_json::Value = serde_json::from_str(&text(me).await).expect("a user");
    assert_eq!(me["Name"], USERNAME);
    assert_eq!(devices(&server).await, 1, "one device");
    let log = activity(&server).await;
    assert!(
        log.iter()
            .any(|entry| entry.type_ == "AuthenticationSucceeded"
                && entry.name == format!("{USERNAME} successfully authenticated")),
        "{log:?}"
    );

    // The code works once.
    let replayed = server
        .exchange(&code, Some(&cookie), "sso-browser-2", None)
        .await;
    assert_eq!(replayed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(text(replayed).await, "Sign-in failed. Try again.");

    // Finished in another browser, without the cookie: no page, no code.
    let (callback, _) = signed_in_at(&server, &fake).await;
    let elsewhere = server.get(&callback, None).await;
    assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);
    // And a code posted without its cookie is refused.
    let (callback, cookie) = signed_in_at(&server, &fake).await;
    let code = exchange_code(&text(server.get(&callback, Some(&cookie)).await).await);
    let stolen = server.exchange(&code, None, "elsewhere", None).await;
    assert_eq!(stolen.status(), StatusCode::BAD_REQUEST);
    assert_eq!(devices(&server).await, 1, "no other session");
}

#[tokio::test]
async fn the_account_a_sign_in_creates_refuses_every_password() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    let signed_in = sign_in(&server, &fake, "sso-browser-1", None).await;
    assert_eq!(signed_in.status(), StatusCode::OK);

    // The account belongs to the provider that refuses passwords, which the
    // composition root registered and `GET /Auth/Providers` lists.
    let users = &server.state.users;
    assert_eq!(
        account(&server).await.authentication_provider_id,
        PROVIDER_ID
    );
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
        assert_eq!(
            password_sign_in(&server, USERNAME, password).await.status(),
            StatusCode::UNAUTHORIZED,
            "{password:?}"
        );
    }
    assert_eq!(account(&server).await.invalid_login_attempt_count, 0);
}

const ADMIN_PASSWORD: &str = "sso-sign-in-admin-pw";

/// Seeds the administrator `admin`, before any other account exists.
async fn seed_admin(server: &SsoServer, temp: &tempfile::TempDir) {
    let config = Config {
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    ferrofin_server::seed::seed_default_admin(server.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
}

/// `POST /Users/AuthenticateByName` as `name` with `password`.
async fn password_sign_in(
    server: &SsoServer,
    name: &str,
    password: &str,
) -> axum::response::Response {
    server
        .send(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    r#"MediaBrowser Client="sso-sign-in-test", Device="fixture", DeviceId="sso-sign-in-test", Version="1""#,
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": name, "Pw": password }).to_string(),
                ))
                .expect("request"),
        )
        .await
}

/// The `Authorization` header of the administrator, signed in with their
/// password.
async fn admin(server: &SsoServer) -> String {
    let answer = password_sign_in(server, "admin", ADMIN_PASSWORD).await;
    assert_eq!(answer.status(), StatusCode::OK);
    let answer: serde_json::Value = serde_json::from_str(&text(answer).await).expect("JSON");
    format!(
        r#"MediaBrowser Client="sso-sign-in-test", Device="admin", DeviceId="sso-sign-in-admin", Version="1", Token="{}""#,
        answer["AccessToken"].as_str().expect("a token")
    )
}

/// `method uri` as `admin`, with `body` as JSON: the status and the answer
/// as JSON (`null` when it is none).
async fn as_admin(
    server: &SsoServer,
    admin: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, admin)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("request");
    let answer = server.send(request).await;
    let status = answer.status();
    (
        status,
        serde_json::from_str(&text(answer).await).unwrap_or(serde_json::Value::Null),
    )
}

/// The ids `GET uri` lists.
async fn listed_ids(server: &SsoServer, admin: &str, uri: &str) -> Vec<String> {
    let (status, list) = as_admin(server, admin, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    list.as_array()
        .expect("a list")
        .iter()
        .map(|pair| pair["Id"].as_str().expect("an id").to_owned())
        .collect()
}

/// What jellyfin-web 12.4's user page (`Profile.tsx`) posts when an admin
/// saves the fake's account with `login` chosen: the policy `GET /Users/{id}`
/// returned, with each provider select's value. A list of one provider is
/// hidden with that one selected; a longer one keeps the account's provider,
/// or the browser selects the first option when that is not listed.
async fn save_user_page(server: &SsoServer, admin: &str, login: &str) {
    let id = Uuid::parse_str(&account(server).await.id)
        .expect("an id")
        .simple();
    let (status, user) = as_admin(server, admin, "GET", &format!("/Users/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let mut policy = user["Policy"].clone();
    let resets = listed_ids(server, admin, "/Auth/PasswordResetProviders").await;
    let reset = policy["PasswordResetProviderId"].as_str().expect("an id");
    let reset = if resets.len() > 1 && resets.iter().any(|listed| listed == reset) {
        reset.to_owned()
    } else {
        resets[0].clone()
    };
    policy["AuthenticationProviderId"] = login.into();
    policy["PasswordResetProviderId"] = reset.into();
    let (status, _) = as_admin(
        server,
        admin,
        "POST",
        &format!("/Users/{id}/Policy"),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(account(server).await.authentication_provider_id, login);
}

/// No password signs in to the fake's account, not even an empty one.
async fn assert_no_password_signs_in(server: &SsoServer, when: &str) {
    for password in ["", "guess", USERNAME] {
        assert_eq!(
            password_sign_in(server, USERNAME, password).await.status(),
            StatusCode::UNAUTHORIZED,
            "{when}: {password:?}"
        );
    }
}

/// B11 and D21: with the plugin disabled, the server lists the default
/// provider only, so the user page hides its choice and saves that one.
/// The account Single Sign-On created still has no password anyone knows,
/// then and once the plugin is enabled again.
#[tokio::test]
async fn the_user_page_saved_while_the_plugin_is_disabled_opens_no_password() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    seed_admin(&server, &temp).await;
    let admin = admin(&server).await;
    let signed_in = sign_in(&server, &fake, "sso-browser-1", None).await;
    assert_eq!(signed_in.status(), StatusCode::OK);

    server
        .state
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");
    assert_eq!(
        listed_ids(&server, &admin, "/Auth/Providers").await,
        [DEFAULT_AUTH_PROVIDER_ID]
    );
    save_user_page(&server, &admin, DEFAULT_AUTH_PROVIDER_ID).await;
    assert_no_password_signs_in(&server, "plugin disabled").await;

    server
        .state
        .plugins
        .enable_plugin(EXTENSION_ID)
        .await
        .expect("enable");
    assert_no_password_signs_in(&server, "plugin enabled again").await;
}

/// The same with every extension off (`FERROFIN_DISABLE_EXTENSIONS`), on a
/// server booted again over the database the sign-in wrote.
#[tokio::test]
async fn the_user_page_saved_with_extensions_disabled_opens_no_password() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    seed_admin(&server, &temp).await;
    let signed_in = sign_in(&server, &fake, "sso-browser-1", None).await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    drop(server);

    let server = sso::boot(&Config {
        disable_extensions: true,
        ..sso::config(&temp, PUBLISHED_URL)
    })
    .await;
    let admin = admin(&server).await;
    assert_eq!(
        listed_ids(&server, &admin, "/Auth/Providers").await,
        [DEFAULT_AUTH_PROVIDER_ID]
    );
    save_user_page(&server, &admin, DEFAULT_AUTH_PROVIDER_ID).await;
    assert_no_password_signs_in(&server, "extensions disabled").await;
}

/// D5: an admin may choose the default provider for the account on its page,
/// the plugin enabled. No password signs in to it until the admin sets one,
/// and then that one only.
#[tokio::test]
async fn an_account_moved_to_the_default_provider_signs_in_with_the_password_an_admin_sets() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    seed_admin(&server, &temp).await;
    let admin = admin(&server).await;
    let signed_in = sign_in(&server, &fake, "sso-browser-1", None).await;
    assert_eq!(signed_in.status(), StatusCode::OK);

    let providers = listed_ids(&server, &admin, "/Auth/Providers").await;
    assert!(
        providers.iter().any(|id| id == PROVIDER_ID),
        "{providers:?}"
    );
    save_user_page(&server, &admin, DEFAULT_AUTH_PROVIDER_ID).await;
    assert_no_password_signs_in(&server, "moved, no password set").await;

    let id = Uuid::parse_str(&account(&server).await.id)
        .expect("an id")
        .simple();
    let (status, _) = as_admin(
        &server,
        &admin,
        "POST",
        &format!("/Users/{id}/Password"),
        Some(serde_json::json!({ "NewPw": "chosen-by-the-admin" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        password_sign_in(&server, USERNAME, "chosen-by-the-admin")
            .await
            .status(),
        StatusCode::OK
    );
    assert_no_password_signs_in(&server, "the admin's password set").await;
}

/// A change to an account's policy that makes a sign-in rule refuse it.
type Rule = fn(&mut UserPolicy);

/// Saves `edit`ed policy of the fake's account.
async fn edit_policy(server: &SsoServer, edit: impl FnOnce(&mut UserPolicy)) {
    let user = account(server).await;
    let mut policy = server
        .state
        .users
        .get_user_dto(&user, None)
        .await
        .expect("a DTO")
        .policy
        .expect("a policy");
    edit(&mut policy);
    server
        .state
        .users
        .update_policy(Uuid::parse_str(&user.id).expect("an id"), &policy)
        .await
        .expect("saved");
}

/// B7: each account rule of a password sign-in refuses the exchange with
/// its message, opens no session, writes `AuthenticationFailed`, and leaves
/// the failed-login counter alone.
#[tokio::test]
async fn each_account_rule_refuses_the_sign_in_through_the_router() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with_provider(&fake, &temp).await;
    let first = sign_in(&server, &fake, "sso-first-device", None).await;
    assert_eq!(first.status(), StatusCode::OK);
    let remote = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8)));
    let rules: [(&str, Rule, Option<IpAddr>, &str); 5] = [
        (
            "disabled",
            |policy| policy.is_disabled = true,
            None,
            "The fake-user account is currently disabled. Please consult with your administrator.",
        ),
        (
            "remote access",
            |policy| policy.enable_remote_access = false,
            remote,
            "remote access is not permitted for this user",
        ),
        (
            "parental schedule",
            |policy| {
                policy.access_schedules = vec![AccessSchedule {
                    day_of_week: DynamicDayOfWeek::Everyday,
                    start_hour: 24.0,
                    end_hour: 24.0,
                    ..AccessSchedule::default()
                }];
            },
            None,
            "User is not allowed access at this time.",
        ),
        (
            "device access",
            |policy| {
                policy.enable_all_devices = false;
                policy.enabled_devices = vec!["sso-first-device".to_owned()];
            },
            None,
            "user is not allowed access from this device",
        ),
        (
            "session limit",
            |policy| policy.max_active_sessions = 1,
            None,
            "user is at their maximum number of sessions",
        ),
    ];
    let allowed = {
        let user = account(&server).await;
        server
            .state
            .users
            .get_user_dto(&user, None)
            .await
            .expect("a DTO")
            .policy
            .expect("a policy")
    };
    for (rule, edit, peer, message) in rules {
        edit_policy(&server, edit).await;
        let logged = activity(&server).await.len();
        let refused = sign_in(&server, &fake, "sso-second-device", peer).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{rule}");
        assert_eq!(header_text(&refused, header::SET_COOKIE), CLEARED, "{rule}");
        assert_eq!(text(refused).await, message, "{rule}");
        assert_eq!(devices(&server).await, 1, "{rule}: no session");
        let log = activity(&server).await;
        assert_eq!(log.len(), logged + 1, "{rule}");
        assert_eq!(log[0].type_, "AuthenticationFailed", "{rule}");
        assert_eq!(
            log[0].name,
            format!("Failed login attempt from {USERNAME}"),
            "{rule}"
        );
        assert_eq!(
            account(&server).await.invalid_login_attempt_count,
            0,
            "{rule}"
        );
        edit_policy(&server, |policy| *policy = allowed.clone()).await;
    }

    // With every rule lifted, the same device signs in.
    let again = sign_in(&server, &fake, "sso-second-device", remote).await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(devices(&server).await, 2);
}
