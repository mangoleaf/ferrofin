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
    server_with(fake, temp, |_| {}).await
}

/// [`server`], with `edit` applied to its configuration first.
async fn server_with(
    fake: &FakeIdentityProvider,
    temp: &tempfile::TempDir,
    edit: impl FnOnce(&mut ferrofin_server::config::Config),
) -> SsoServer {
    let server = sso::server_with(temp, PUBLISHED_URL, edit).await;
    save(&server, "fake", fake, false).await;
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

/// Saves `fake` as the provider `key`, named after it, creating accounts on
/// a first sign-in when `create_users`.
async fn save(server: &SsoServer, key: &str, fake: &FakeIdentityProvider, create_users: bool) {
    let mut name = key.to_owned();
    name[..1].make_ascii_uppercase();
    server
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service")
        .save_oidc_provider(
            key,
            OidcProvider {
                name,
                issuer: fake.issuer(),
                allow_http_issuer: true,
                client_id: "ferrofin".to_owned(),
                client_secret: Some(Secret::new("sso-links-client-secret")),
                create_users,
                ..OidcProvider::default()
            },
        )
        .await
        .expect("saved");
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

/// `POST /sso/oidc/{provider}/link` with `token`, as the link page sends it:
/// `password` in its JSON body when given, no body at all otherwise.
async fn start_link(
    server: &SsoServer,
    provider: &str,
    token: &str,
    password: Option<&str>,
) -> axum::response::Response {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/sso/oidc/{provider}/link"))
        .header(
            header::AUTHORIZATION,
            format!(r#"MediaBrowser Token="{token}""#),
        );
    let request = match password {
        Some(password) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "Password": password }).to_string(),
            )),
        None => request.body(Body::empty()),
    };
    server.send(request.expect("request")).await
}

/// A link started with `password`: where its callback comes back to and its
/// flow cookie, once `fake` signed in.
async fn started(
    server: &SsoServer,
    fake: &FakeIdentityProvider,
    provider: &str,
    token: &str,
) -> (String, String) {
    let started = start_link(server, provider, token, Some(PASSWORD)).await;
    assert_eq!(started.status(), StatusCode::OK, "the link starts");
    let set_cookie = header_text(&started, header::SET_COOKIE);
    let url = json(started).await["Url"]
        .as_str()
        .expect("a URL")
        .to_owned();
    back_from(fake, &url, &set_cookie).await
}

/// A start that was refused: its status and body. No flow cookie rode on it.
async fn refused_start(response: axum::response::Response) -> (StatusCode, String) {
    let status = response.status();
    assert!(
        response.headers().get(header::SET_COOKIE).is_none(),
        "no flow cookie ({status})"
    );
    (status, text(response).await)
}

/// `POST /sso/admin/links` as the admin `admin`: links `user` to `subject`
/// at `provider`.
async fn admin_link(
    server: &SsoServer,
    admin: &str,
    user: &str,
    provider: &str,
    subject: &str,
) -> StatusCode {
    let id = server
        .state
        .users
        .get_user_by_name(user)
        .await
        .expect("lookup")
        .expect("the account")
        .id;
    let id = uuid::Uuid::parse_str(&id).expect("id");
    server
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
                        "UserId": id.simple().to_string(),
                        "Provider": provider,
                        "Subject": subject,
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .status()
}

/// Every link, as the settings page lists them: account name and subject.
async fn links(server: &SsoServer) -> Vec<(String, String)> {
    server
        .state
        .sso
        .as_ref()
        .expect("the Single Sign-On service")
        .all_oidc_links()
        .await
        .expect("links")
        .into_iter()
        .map(|link| (link.user_name, link.subject))
        .collect()
}

/// The activity log's entries of `kind`, as the dashboard reads them: each
/// one's name, short overview and overview.
async fn activity(server: &SsoServer, admin: &str, kind: &str) -> Vec<serde_json::Value> {
    let entries = json(call(server, "GET", "/System/ActivityLog/Entries", admin).await).await;
    entries["Items"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|entry| entry["Type"] == kind)
        .cloned()
        .collect()
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

    // The link page starts a link with the token and the account's password,
    // and goes where the answer says, the flow cookie set: host-only on this
    // `https` public URL (D23).
    let started = start_link(&server, "fake", &token, Some(PASSWORD)).await;
    assert_eq!(started.status(), StatusCode::OK);
    let set_cookie = header_text(&started, header::SET_COOKIE);
    assert!(
        set_cookie.starts_with("__Host-ferrofin_sso_flow=")
            && set_cookie.ends_with("; Path=/; Max-Age=600; HttpOnly; SameSite=Lax; Secure"),
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

/// The security review's reproduction: a session token alone starts no
/// link, nor does a wrong password, and neither leaves a flow cookie; a link
/// started with the password whose session then signs out links nothing when
/// another identity completes it, and the account's own sign-in still works.
#[tokio::test]
async fn a_token_alone_starts_no_link_and_a_signed_out_flow_links_nothing() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    let (_, token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    let token = token.expect("a token");

    // The token alone: no body, or no password in it.
    let (status, _) = refused_start(start_link(&server, "fake", &token, None).await).await;
    assert!(
        status.is_client_error() && status != StatusCode::UNAUTHORIZED,
        "{status}"
    );
    let (status, _) = refused_start(start_link(&server, "fake", &token, Some("")).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) =
        refused_start(start_link(&server, "fake", &token, Some("not-her-password")).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // With the password the link starts; the session signs out; someone
    // else finishes the flow at the provider.
    fake.sign_in(|sign_in| sign_in.subject = "mallory-sub".to_owned());
    let (callback, cookie) = started(&server, &fake, "fake", &token).await;
    let out = call(&server, "POST", "/Sessions/Logout", &token).await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);
    let page = server.get(&callback, Some(&cookie)).await;
    assert_eq!(page.status(), StatusCode::BAD_REQUEST);
    assert!(!text(page).await.contains("Sign-in linked"));
    assert_eq!(links(&server).await, []);

    // That identity reaches no account, and alice's password still does.
    assert_eq!(
        provider_sign_in(&server, &fake, "mallory-browser").await.0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    assert_eq!(status, StatusCode::OK);
}

/// A link flow ends with the session that started it, whichever way that
/// session ends: a sign-out, an admin removing its device, a password change
/// from another session, or the removal of another of the account's links.
#[tokio::test]
async fn a_link_flow_ends_with_the_session_that_started_it() {
    for ending in ["sign-out", "device removed", "password changed", "unlinked"] {
        let fake = FakeIdentityProvider::start().await;
        let other = FakeIdentityProvider::start().await;
        let temp = tempfile::tempdir().expect("temp dir");
        let server = server(&fake, &temp).await;
        save(&server, "other", &other, false).await;
        let (_, admin) = password_sign_in(&server, "root", PASSWORD, "admin-browser").await;
        let admin = admin.expect("a token");
        let (_, token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
        let token = token.expect("a token");
        // Linked at the other provider: removing that link signs her out.
        assert_eq!(
            admin_link(&server, &admin, "alice", "other", "alice-other").await,
            StatusCode::OK
        );
        let (callback, cookie) = started(&server, &fake, "fake", &token).await;

        let ended = match ending {
            "sign-out" => call(&server, "POST", "/Sessions/Logout", &token).await,
            "device removed" => call(&server, "DELETE", "/Devices?id=alice-browser", &admin).await,
            "password changed" => {
                let (_, second) =
                    password_sign_in(&server, "alice", PASSWORD, "alice-second").await;
                server
                    .send(
                        Request::builder()
                            .method("POST")
                            .uri("/Users/Password")
                            .header(
                                header::AUTHORIZATION,
                                format!(r#"MediaBrowser Token="{}""#, second.expect("a token")),
                            )
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::json!({
                                    "CurrentPw": PASSWORD,
                                    "NewPw": "alice-new-password",
                                })
                                .to_string(),
                            ))
                            .expect("request"),
                    )
                    .await
            }
            _ => call(&server, "DELETE", "/sso/links/other", &token).await,
        };
        assert!(ended.status().is_success(), "{ending}: {}", ended.status());
        assert_eq!(
            call(&server, "GET", "/Users/Me", &token).await.status(),
            StatusCode::UNAUTHORIZED,
            "{ending}: the session ended"
        );

        let page = server.get(&callback, Some(&cookie)).await;
        assert_eq!(page.status(), StatusCode::BAD_REQUEST, "{ending}");
        let kept: Vec<(String, String)> = if ending == "unlinked" {
            Vec::new()
        } else {
            vec![("alice".to_owned(), "alice-other".to_owned())]
        };
        assert_eq!(links(&server).await, kept, "{ending}: nothing linked");
    }
}

/// A self-service link never replaces one: of two flows started before
/// either links, only the first called back links, and an account linked at
/// the issuer cannot start another. The admin's route still replaces a link.
/// (The fake names its subject when it issues the token, so the flows are
/// called back one after the other; the extension's tests race them.)
#[tokio::test]
async fn a_self_service_link_never_replaces_one_and_the_admin_still_can() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    let (_, admin) = password_sign_in(&server, "root", PASSWORD, "admin-browser").await;
    let admin = admin.expect("a token");
    let (_, token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    let token = token.expect("a token");

    let first = started(&server, &fake, "fake", &token).await;
    let second = started(&server, &fake, "fake", &token).await;
    let page = server.get(&first.0, Some(&first.1)).await;
    assert_eq!(page.status(), StatusCode::OK);
    fake.sign_in(|sign_in| sign_in.subject = "alice-second".to_owned());
    let page = server.get(&second.0, Some(&second.1)).await;
    assert_eq!(page.status(), StatusCode::CONFLICT);
    let refusal = text(page).await;
    assert!(refusal.contains("Remove it first"), "{refusal}");
    let linked = links(&server).await;
    assert_eq!(
        linked,
        [(
            "alice".to_owned(),
            ferrofin_extensions::sso::fake_idp::SUBJECT.to_owned()
        )]
    );

    // A third start is refused before the provider is reached.
    let (status, body) =
        refused_start(start_link(&server, "fake", &token, Some(PASSWORD)).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("remove it first"), "{body}");
    assert_eq!(links(&server).await, linked);

    // The admin's link replaces it, as before.
    assert_eq!(
        admin_link(&server, &admin, "alice", "fake", "alice-by-admin").await,
        StatusCode::OK
    );
    assert_eq!(
        links(&server).await,
        [("alice".to_owned(), "alice-by-admin".to_owned())]
    );
}

/// An account Single Sign-On created has a password no one knows: it cannot
/// link itself whatever it sends, and is told that an administrator adds the
/// link; the admin's route links it.
#[tokio::test]
async fn an_account_single_sign_on_created_cannot_link_itself() {
    let fake = FakeIdentityProvider::start().await;
    let other = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    save(&server, "fake", &fake, true).await;
    save(&server, "other", &other, false).await;
    let (_, admin) = password_sign_in(&server, "root", PASSWORD, "admin-browser").await;
    let admin = admin.expect("a token");
    fake.sign_in(|sign_in| {
        sign_in.subject = "carol-sub".to_owned();
        sign_in
            .claims
            .insert("preferred_username".to_owned(), "carol".into());
    });
    let (status, session) = provider_sign_in(&server, &fake, "carol-browser").await;
    assert_eq!(status, StatusCode::OK);
    let token = session.expect("a session")["AccessToken"]
        .as_str()
        .expect("a token")
        .to_owned();
    assert_eq!(
        json(call(&server, "GET", "/sso/links", &token).await).await["SingleSignOnAccount"],
        true
    );

    for password in ["", "anything"] {
        let (status, body) =
            refused_start(start_link(&server, "other", &token, Some(password)).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{password:?}");
        assert!(body.contains("an administrator"), "{body}");
    }
    assert_eq!(
        admin_link(&server, &admin, "carol", "other", "carol-other").await,
        StatusCode::OK
    );
}

/// A wrong password at the start of a link counts as a failed sign-in does:
/// the account's failed attempts go up, and the login backoff paces the next
/// sign-in from the same address. An empty one is no attempt, so an account
/// without a password cannot link until it sets one. The account here has
/// no password, so no attempt spends time hashing one: what the clock shows
/// is the backoff's wait.
#[tokio::test]
async fn a_wrong_password_counts_and_is_paced_as_at_sign_in() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server_with(&fake, &temp, |config| config.login_backoff = true).await;
    server
        .state
        .users
        .create_user("carl")
        .await
        .expect("created");
    let (_, token) = password_sign_in(&server, "carl", "", "carl-browser").await;
    let token = token.expect("a token");
    let failures = || async {
        server
            .state
            .users
            .get_user_by_name("carl")
            .await
            .expect("lookup")
            .expect("carl")
            .invalid_login_attempt_count
    };

    let (status, _) = refused_start(start_link(&server, "fake", &token, Some("")).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(failures().await, 0, "an empty password is no attempt");
    // The backoff's default: five failures, then the next attempt waits a
    // second from the fifth's admission.
    let mut fifth = None;
    for attempt in 1..=5 {
        if attempt == 5 {
            fifth = Some(std::time::Instant::now());
        }
        let (status, _) =
            refused_start(start_link(&server, "fake", &token, Some("wrong")).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{attempt}");
        assert_eq!(failures().await, attempt, "counted");
    }
    let (status, _) = password_sign_in(&server, "carl", "", "carl-later").await;
    assert_eq!(status, StatusCode::OK);
    let waited = fifth.expect("five attempts").elapsed();
    assert!(
        waited >= std::time::Duration::from_secs(1),
        "paced: {waited:?}"
    );
    assert_eq!(failures().await, 0, "a success clears the count");
}

/// Linking and unlinking write activity entries naming the account, the
/// provider and who did it, by the user and by an admin; the subject is in
/// the entry's overview only, never in its short text.
#[tokio::test]
async fn link_and_unlink_are_in_the_activity_log() {
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    let (_, admin) = password_sign_in(&server, "root", PASSWORD, "admin-browser").await;
    let admin = admin.expect("a token");
    let (_, token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    let token = token.expect("a token");

    let (callback, cookie) = started(&server, &fake, "fake", &token).await;
    assert_eq!(
        server.get(&callback, Some(&cookie)).await.status(),
        StatusCode::OK
    );
    let removed = call(&server, "DELETE", "/sso/links/fake", &token).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        admin_link(&server, &admin, "alice", "fake", "alice-by-admin").await,
        StatusCode::OK
    );
    let alice = server
        .state
        .users
        .get_user_by_name("alice")
        .await
        .expect("lookup")
        .expect("alice");
    let alice_id = uuid::Uuid::parse_str(&alice.id).expect("id");
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

    let subjects = [
        ferrofin_extensions::sso::fake_idp::SUBJECT,
        "alice-by-admin",
    ];
    // Newest first: the admin's, then alice's.
    for (kind, name) in [
        (
            "UserIdentityLinked",
            "A sign-in at Fake was linked to alice",
        ),
        (
            "UserIdentityUnlinked",
            "A sign-in at Fake was unlinked from alice",
        ),
    ] {
        let entries = activity(&server, &admin, kind).await;
        assert_eq!(entries.len(), 2, "{kind}: {entries:?}");
        for (entry, (by, subject)) in entries.iter().zip([
            ("By the administrator root", subjects[1]),
            ("By alice", subjects[0]),
        ]) {
            assert_eq!(entry["Name"], name, "{entry}");
            assert_eq!(entry["ShortOverview"], by, "{entry}");
            assert_eq!(entry["UserId"], alice_id.simple().to_string(), "{entry}");
            let overview = entry["Overview"].as_str().expect("an overview");
            assert!(overview.contains(subject), "{entry}");
            assert!(overview.contains(&fake.issuer()), "{entry}");
        }
    }
}

/// What the test's subscriber writes: everything logged on its thread.
#[derive(Clone, Default)]
struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(buf);
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

/// No password and no session token reaches a log line or a span through
/// the whole HTTP stack (the request span, the body binder, the error
/// answer, the plugin), wrong or right, while the wrong one's failure is
/// logged.
#[tokio::test]
async fn no_password_is_logged_through_the_http_stack() {
    let log = Log::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
            .with_ansi(false)
            .finish(),
    );
    let fake = FakeIdentityProvider::start().await;
    let temp = tempfile::tempdir().expect("temp dir");
    let server = server(&fake, &temp).await;
    let (_, token) = password_sign_in(&server, "alice", PASSWORD, "alice-browser").await;
    let token = token.expect("a token");
    let wrong = "wrong-link-password-5e1d";
    let (status, body) =
        refused_start(start_link(&server, "fake", &token, Some(wrong)).await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!body.contains(wrong), "never echoed: {body}");
    started(&server, &fake, "fake", &token).await;

    let text = String::from_utf8_lossy(&log.0.lock().expect("log")).into_owned();
    assert!(
        text.contains("/sso/oidc/{provider}/link"),
        "the requests were captured"
    );
    assert!(
        text.contains("login failed"),
        "the wrong password is logged"
    );
    for secret in [wrong, PASSWORD, token.as_str()] {
        assert!(!text.contains(secret), "a secret is in the log");
    }
}
