//! Linking an account and an OIDC identity (B12) through the composition
//! root and the real router (`tower` oneshot), as the link page and the
//! settings page make it, against the fake identity provider: a local user
//! signed in with a password links their account, the identity then signs in
//! to it while the password still does, and removing the link ends every
//! session of the account; an admin lists every link and removes any, and a
//! plain user cannot.

#[path = "support/sso.rs"]
mod sso;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_extensions::sso::fake_idp::FakeIdentityProvider;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use sso::{SsoServer, exchange_code, header_text, text};

const PUBLISHED_URL: &str = "https://media.example.org/jf";
const PASSWORD: &str = "alice-local-password";

/// The server with `fake` saved as the provider `fake`, which creates no
/// accounts, the local account `alice` with a password, and the
/// administrator `root`, with the same password.
async fn server(fake: &FakeIdentityProvider, temp: &tempfile::TempDir) -> SsoServer {
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
                client_secret: Some(Secret::new("sso-links-client-secret")),
                create_users: false,
                ..OidcProvider::default()
            },
        )
        .await
        .expect("saved");
    let users = &server.state.users;
    for name in ["alice", "root"] {
        let user = users.create_user(name).await.expect("created");
        let id = uuid::Uuid::parse_str(&user.id).expect("id");
        users.change_password(id, PASSWORD).await.expect("password");
        if name == "root" {
            let mut policy = users
                .get_user_dto(&user, None)
                .await
                .expect("a DTO")
                .policy
                .expect("a policy");
            policy.is_administrator = true;
            users.update_policy(id, &policy).await.expect("admin");
        }
    }
    server
}

/// `POST /Users/AuthenticateByName` as `name`, from the device `device_id`:
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

/// `method uri` with `token` alone, as the link page sends it.
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

async fn json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&text(response).await).expect("a JSON body")
}

/// The callback's path under the published URL and its flow cookie, for a
/// flow that sent the browser to `location` with `set_cookie`.
async fn back_from(
    fake: &FakeIdentityProvider,
    location: &str,
    set_cookie: &str,
) -> (String, String) {
    let back = fake.authorize(location).await;
    let callback = back
        .strip_prefix(PUBLISHED_URL)
        .unwrap_or_else(|| panic!("back to the published URL: {back}"))
        .to_owned();
    (
        callback,
        set_cookie.split(';').next().expect("name=value").to_owned(),
    )
}

/// A sign-in at `fake` from its start: the callback's status, and the
/// exchange's answer when the callback answered the completion page.
async fn provider_sign_in(
    server: &SsoServer,
    fake: &FakeIdentityProvider,
    device_id: &str,
) -> (StatusCode, Option<serde_json::Value>) {
    let start = server.get("/sso/oidc/fake/start", None).await;
    let (callback, cookie) = back_from(
        fake,
        &header_text(&start, header::LOCATION),
        &header_text(&start, header::SET_COOKIE),
    )
    .await;
    let page = server.get(&callback, Some(&cookie)).await;
    let status = page.status();
    if status != StatusCode::OK {
        return (status, None);
    }
    let code = exchange_code(&text(page).await);
    let exchanged = server.exchange(&code, Some(&cookie), device_id, None).await;
    (status, Some(json(exchanged).await))
}

#[tokio::test]
async fn a_user_links_signs_in_through_the_provider_and_unlinks() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    let (status, token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    assert_eq!(status, StatusCode::OK);
    let token = token.expect("a token");

    // Not linked: the provider finds no account, and creates none.
    assert_eq!(
        provider_sign_in(&server, &fake, "elsewhere").await.0,
        StatusCode::FORBIDDEN
    );

    // The link page starts a link with the token alone, and goes where the
    // answer says, the flow cookie set.
    let started = call(&server, "POST", "/sso/oidc/fake/link", &token).await;
    assert_eq!(started.status(), StatusCode::OK);
    let set_cookie = header_text(&started, header::SET_COOKIE);
    assert!(
        set_cookie.ends_with("; Path=/jf/sso; Max-Age=600; HttpOnly; SameSite=Lax; Secure"),
        "{set_cookie}"
    );
    let url = json(started).await["Url"]
        .as_str()
        .expect("a URL")
        .to_owned();
    let (callback, cookie) = back_from(&fake, &url, &set_cookie).await;
    let linked = server.get(&callback, Some(&cookie)).await;
    assert_eq!(linked.status(), StatusCode::OK);
    assert!(text(linked).await.contains("Sign-in linked"));

    let links = json(call(&server, "GET", "/sso/links", &token).await).await;
    assert_eq!(links["SingleSignOnAccount"], false);
    assert_eq!(links["Links"][0]["Provider"], "fake");
    assert_eq!(links["Links"][0]["Issuer"], fake.issuer());
    assert!(links["Links"][0].get("Subject").is_none(), "{links}");

    // The identity signs in to alice, and so does her password (D5).
    let (status, session) = provider_sign_in(&server, &fake, "provider-browser").await;
    assert_eq!(status, StatusCode::OK);
    let session = session.expect("a session");
    assert_eq!(session["User"]["Name"], "alice");
    let provider_token = session["AccessToken"].as_str().expect("a token").to_owned();
    let (status, second) = password_sign_in(&server, "alice", PASSWORD, "second-browser").await;
    assert_eq!(status, StatusCode::OK);
    let second = second.expect("a token");

    // A plain user sees no one else's links.
    let refused = call(&server, "GET", "/sso/admin/links", &token).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    // Unlinking ends every session of the account, this one included, and
    // the provider no longer finds it.
    let removed = call(&server, "DELETE", "/sso/links/fake", &token).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    for token in [&token, &provider_token, &second] {
        assert_eq!(
            call(&server, "GET", "/Users/Me", token).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        provider_sign_in(&server, &fake, "provider-browser").await.0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    assert_eq!(status, StatusCode::OK, "the password still signs in");
}

#[tokio::test]
async fn an_admin_lists_and_removes_any_link_and_a_deleted_account_has_none() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    let alice = server
        .state
        .users
        .get_user_by_name("alice")
        .await
        .expect("lookup")
        .expect("alice");
    let alice_id = uuid::Uuid::parse_str(&alice.id).expect("id");
    let bob = server.state.users.create_user("bob").await.expect("bob");
    let bob_id = uuid::Uuid::parse_str(&bob.id).expect("id");
    let (status, admin) = password_sign_in(&server, "root", PASSWORD, "admin-browser").await;
    assert_eq!(status, StatusCode::OK);
    let admin = admin.expect("a token");
    let (_, alice_token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    let alice_token = alice_token.expect("a token");

    for (user, subject) in [(alice_id, "alice-sub"), (bob_id, "bob-sub")] {
        let added = server
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/sso/admin/links")
                    .header(
                        header::AUTHORIZATION,
                        format!(r#"MediaBrowser Token="{admin}""#),
                    )
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "UserId": user.simple().to_string(),
                            "Provider": "fake",
                            "Subject": subject,
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await;
        assert_eq!(added.status(), StatusCode::OK, "{subject}");
    }
    let listed = json(call(&server, "GET", "/sso/admin/links", &admin).await).await;
    let subjects: Vec<&str> = listed
        .as_array()
        .expect("a list")
        .iter()
        .map(|link| link["Subject"].as_str().expect("a subject"))
        .collect();
    assert_eq!(subjects, ["alice-sub", "bob-sub"]);
    assert_eq!(listed[0]["UserName"], "alice");

    let removed = call(
        &server,
        "DELETE",
        &format!(
            "/sso/admin/links?userId={}&provider=fake",
            alice_id.simple()
        ),
        &admin,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        call(&server, "GET", "/Users/Me", &alice_token)
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "alice's sessions were signed out"
    );
    assert_eq!(
        call(&server, "GET", "/Users/Me", &admin).await.status(),
        StatusCode::OK,
        "the admin's were not"
    );

    // Deleting bob removes his link.
    server
        .state
        .users
        .delete_user(bob_id)
        .await
        .expect("deleted");
    let listed = json(call(&server, "GET", "/sso/admin/links", &admin).await).await;
    assert_eq!(listed, serde_json::json!([]));
}
