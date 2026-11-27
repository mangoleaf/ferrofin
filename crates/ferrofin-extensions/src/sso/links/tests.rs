//! Linking an account and an identity (B12, D22), through the sign-in
//! harness and the fake identity provider: the link flow from its start,
//! with the account's password, to its callback, each way a start or a
//! callback is refused, a flow ending with its session, the admin's links,
//! a removal signing the account out, and the activity log.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::secret::Secret;
use ferrofin_traits::activity::ActivityLogQuery;
use ferrofin_traits::devices::DeviceManager as _;
use ferrofin_traits::external_identities::ExternalIdentityStore;
use ferrofin_traits::session::{AuthenticationRequest, SessionManager as _};
use ferrofin_traits::sso::{OidcCallback, OidcExchangeRefusal, OidcExchangeReply};

use super::*;
use crate::sso::EXTENSION_ID;
use crate::sso::fake_idp::{FakeIdentityProvider, SUBJECT, USERNAME};
use crate::sso::sign_in::tests::{Harness, PUBLIC_URL, device, fake_provider};

const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9));
const PASSWORD: &str = "alice-local-password";

/// A local account `name` with a password.
async fn local(harness: &Harness, name: &str) -> UserEntity {
    let user = harness.users.create_user(name).await.expect("created");
    harness
        .users
        .change_password(id(&user), PASSWORD)
        .await
        .expect("password set");
    reread(harness, &user).await
}

fn id(user: &UserEntity) -> Uuid {
    Uuid::parse_str(&user.id).expect("a user id")
}

async fn reread(harness: &Harness, user: &UserEntity) -> UserEntity {
    harness
        .users
        .get_user_by_id(id(user))
        .await
        .expect("lookup")
        .expect("the account")
}

/// A new session of `user`, signed in with the password from a device of
/// its own (a second sign-in from one device would end the first): its
/// access token and device id.
async fn signed_in(harness: &Harness, user: &UserEntity) -> (Secret, String) {
    static DEVICES: AtomicUsize = AtomicUsize::new(0);
    let device_id = format!("link-browser-{}", DEVICES.fetch_add(1, Ordering::Relaxed));
    let session = harness
        .sessions
        .authenticate_new_session(&AuthenticationRequest {
            username: Some(user.username.clone()),
            password: Some(Secret::new(PASSWORD)),
            ..device(&device_id)
        })
        .await
        .expect("signed in");
    (session.access_token, device_id)
}

/// What the browser keeps of a link start: where it is sent, and its flow
/// cookie's value.
fn started(reply: SsoReply) -> (String, String) {
    assert_eq!(reply.status, 302, "{reply:?}");
    let cookie = crate::sso::sign_in::tests::flow_cookie_value(&reply.set_cookies[0]);
    (reply.location.expect("a redirect"), cookie)
}

/// A link flow of `user` at `key` started from the session `token`, with
/// `password`: the start's answer.
async fn start_with(
    harness: &Harness,
    key: &str,
    user: &UserEntity,
    password: &str,
    token: &Secret,
) -> Result<SsoReply, ServiceError> {
    harness
        .sso
        .start_oidc_link(key, id(user), &Secret::new(password), token, CLIENT)
        .await
}

/// A link flow of `user` at `fake` started from a new session: where the
/// browser is sent, its flow cookie's value, and the session's token.
async fn start_link(harness: &Harness, user: &UserEntity) -> (String, String, Secret) {
    let (token, _) = signed_in(harness, user).await;
    let (location, cookie) = started(
        start_with(harness, "fake", user, PASSWORD, &token)
            .await
            .expect("starts"),
    );
    (location, cookie, token)
}

/// Where `fake` sends the browser back to from `location`, as the callback
/// route reads it with `cookie`.
async fn callback_at(
    fake: &FakeIdentityProvider,
    location: &str,
    cookie: Option<&str>,
) -> OidcCallback {
    let url = url::Url::parse(&fake.authorize(location).await).expect("a URL");
    let query = |name: &str| {
        url.query_pairs()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.into_owned())
    };
    OidcCallback {
        state: query("state"),
        code: query("code").map(Secret::new),
        iss: query("iss"),
        error: query("error"),
        flow_cookie: cookie.map(Secret::new),
    }
}

/// [`callback_at`] the harness's `fake`.
async fn callback(harness: &Harness, location: &str, cookie: Option<&str>) -> OidcCallback {
    callback_at(&harness.fake, location, cookie).await
}

/// The callback of a flow at `key` that sent the browser to `location` with
/// `cookie`, through `fake`: its page.
async fn finish_at(
    harness: &Harness,
    fake: &FakeIdentityProvider,
    key: &str,
    location: &str,
    cookie: &str,
) -> SsoReply {
    let back = callback_at(fake, location, Some(cookie)).await;
    harness
        .sso
        .finish_oidc_sign_in(key, back)
        .await
        .expect("an answer")
}

/// A whole link flow of `user` at `fake`, through the callback: its page.
async fn link(harness: &Harness, user: &UserEntity) -> SsoReply {
    let (location, cookie, _) = start_link(harness, user).await;
    finish_at(harness, &harness.fake, "fake", &location, &cookie).await
}

/// The account the fake's `subject` is linked to.
async fn owner(harness: &Harness, subject: &str) -> Option<Uuid> {
    owner_at(harness, &harness.fake.issuer(), subject).await
}

/// The account `subject` at `issuer` is linked to.
async fn owner_at(harness: &Harness, issuer: &str, subject: &str) -> Option<Uuid> {
    harness
        .identities
        .find(ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer,
            subject,
        })
        .await
        .expect("find")
        .map(|link| link.user_id)
}

/// A second provider, `other`, at its own fake identity provider.
async fn other_provider(harness: &Harness) -> FakeIdentityProvider {
    let other = FakeIdentityProvider::start().await;
    harness
        .sso
        .save_oidc_provider(
            "other",
            fake_provider(&other, |provider| "Other".clone_into(&mut provider.name)),
        )
        .await
        .expect("the provider saves");
    other
}

/// The activity log's entries of `kind`, newest first.
async fn entries(harness: &Harness, kind: &str) -> Vec<ferrofin_model::activity::ActivityLogEntry> {
    harness
        .activity
        .get_paged_result(&ActivityLogQuery::default())
        .await
        .expect("the log")
        .items
        .into_iter()
        .filter(|entry| entry.type_ == kind)
        .collect()
}

#[tokio::test]
async fn a_user_links_their_account_then_signs_in_to_it_and_keeps_their_password() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;

    let page = link(&harness, &alice).await;
    assert_eq!(page.status, 200, "{}", page.html);
    assert!(page.html.contains("Sign-in linked"), "{}", page.html);
    assert!(
        page.html.contains(r#"<a href="../../link">"#),
        "{}",
        page.html
    );
    assert!(
        page.html.contains("at Fake &lt;IdP&gt;,"),
        "escaped: {}",
        page.html
    );
    // Its own page: no exchange code, so no session, and the cookie cleared.
    assert!(!page.html.contains("data-code"), "{}", page.html);
    assert!(harness.sso.exchanges.is_empty());
    assert_eq!(
        page.set_cookies,
        ["__Host-ferrofin_sso_flow=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"]
    );
    assert_eq!(owner(&harness, SUBJECT).await, Some(id(&alice)));

    // D5: the account's provider and password are as they were.
    let after = reread(&harness, &alice).await;
    assert_eq!(
        after.authentication_provider_id,
        alice.authentication_provider_id
    );
    assert_eq!(after.password, alice.password);
    assert!(
        harness
            .users
            .authenticate_user("alice", PASSWORD, "192.0.2.9", true)
            .await
            .expect("a password sign-in")
            .is_some(),
        "the password still signs in"
    );

    // The identity now signs in to alice, whose name is not the claim's: no
    // account is created.
    let session = harness.session("browser-1").await.expect("signed in");
    assert_eq!(session.user.id, id(&alice));
    assert!(
        harness
            .users
            .get_user_by_name(USERNAME)
            .await
            .expect("lookup")
            .is_none()
    );
}

/// D22: a start needs the account's current password. Without one nothing
/// is checked; a wrong one counts as a failed sign-in does. Neither leaves a
/// flow, so no cookie is set. The right one starts the flow without stamping
/// a sign-in.
#[tokio::test]
async fn a_link_starts_only_with_the_accounts_password() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let signed_in_at = reread(&harness, &alice).await.last_login_date;

    let empty = start_with(&harness, "fake", &alice, "", &token)
        .await
        .expect_err("no password");
    assert!(matches!(empty, ServiceError::InvalidInput(_)), "{empty:?}");
    assert_eq!(
        reread(&harness, &alice).await.invalid_login_attempt_count,
        0
    );

    let wrong = start_with(&harness, "fake", &alice, "not-her-password", &token)
        .await
        .expect_err("a wrong password");
    assert!(
        matches!(&wrong, ServiceError::Forbidden(message) if message == WRONG_PASSWORD),
        "{wrong:?}"
    );
    assert_eq!(
        reread(&harness, &alice).await.invalid_login_attempt_count,
        1,
        "counted as a failed sign-in"
    );
    assert!(harness.sso.flows.is_empty(), "no flow, so no cookie");

    let reply = start_with(&harness, "fake", &alice, PASSWORD, &token)
        .await
        .expect("starts");
    started(reply);
    assert_eq!(harness.sso.flows.len(), 1);
    let after = reread(&harness, &alice).await;
    assert_eq!(after.invalid_login_attempt_count, 0, "a success clears it");
    assert_eq!(after.last_login_date, signed_in_at, "no sign-in is stamped");
}

/// The check tells a caller no more than a sign-in does: an account rule
/// refusing the right password (here, a disabled account) answers exactly as
/// a wrong password does, and starts no flow.
#[tokio::test]
async fn an_account_rule_answers_as_a_wrong_password_does() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let mut policy = harness
        .users
        .get_user_dto(&alice, None)
        .await
        .expect("a DTO")
        .policy
        .expect("a policy");
    policy.is_disabled = true;
    harness
        .users
        .update_policy(id(&alice), &policy)
        .await
        .expect("disabled");

    for password in [PASSWORD, "not-her-password"] {
        let refused = start_with(&harness, "fake", &alice, password, &token)
            .await
            .expect_err("refused");
        assert!(
            matches!(&refused, ServiceError::Forbidden(message) if message == WRONG_PASSWORD),
            "{refused:?}"
        );
    }
    assert!(harness.sso.flows.is_empty());
}

/// D22: an account Single Sign-On created has a password no one knows, so it
/// cannot link itself whatever it sends; no password is tried for it.
#[tokio::test]
async fn an_account_single_sign_on_created_cannot_start_a_link() {
    let harness = Harness::new().await;
    harness.fake.sign_in(|s| s.subject = "carol-sub".to_owned());
    let session = harness.session("carol-browser").await.expect("signed in");
    let carol = reread_id(&harness, session.user.id).await;
    let _other = other_provider(&harness).await;

    for password in ["", "anything"] {
        let refused = start_with(&harness, "other", &carol, password, &session.access_token)
            .await
            .expect_err("refused");
        let ServiceError::Forbidden(message) = refused else {
            panic!("{refused:?}");
        };
        assert!(message.contains("an administrator adds"), "{message}");
    }
    assert!(harness.sso.flows.is_empty());
    assert_eq!(
        reread(&harness, &carol).await.invalid_login_attempt_count,
        0
    );
}

async fn reread_id(harness: &Harness, user_id: Uuid) -> UserEntity {
    harness
        .users
        .get_user_by_id(user_id)
        .await
        .expect("lookup")
        .expect("the account")
}

#[tokio::test]
async fn a_link_callback_without_its_cookie_or_replayed_links_nothing() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;

    for cookie in [None, Some("another-browser")] {
        let (location, _, _) = start_link(&harness, &alice).await;
        let back = callback(&harness, &location, cookie).await;
        let page = harness
            .sso
            .finish_oidc_sign_in("fake", back)
            .await
            .expect("an answer");
        assert_eq!(page.status, 400, "{cookie:?}");
        assert!(
            page.html.contains("Sign-in failed. Try again."),
            "{cookie:?}"
        );
        assert_eq!(owner(&harness, SUBJECT).await, None, "{cookie:?}");
    }

    // The same callback twice: the second finds no flow.
    let (location, cookie, _) = start_link(&harness, &alice).await;
    let back = callback(&harness, &location, Some(&cookie)).await;
    let first = harness
        .sso
        .finish_oidc_sign_in("fake", back.clone())
        .await
        .expect("an answer");
    assert_eq!(first.status, 200, "{}", first.html);
    harness
        .identities
        .unlink(ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &harness.fake.issuer(),
            subject: SUBJECT,
        })
        .await
        .expect("unlinked");
    let replayed = harness
        .sso
        .finish_oidc_sign_in("fake", back)
        .await
        .expect("an answer");
    assert_eq!(replayed.status, 400);
    assert_eq!(
        owner(&harness, SUBJECT).await,
        None,
        "the replay linked nothing"
    );
}

#[tokio::test]
async fn an_identity_another_account_has_is_refused_and_stays_where_it_is() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let bob = local(&harness, "bob").await;
    assert_eq!(link(&harness, &bob).await.status, 200);

    let page = link(&harness, &alice).await;
    assert_eq!(page.status, 409, "{}", page.html);
    assert!(
        page.html
            .contains("already linked to another Ferrofin account"),
        "{}",
        page.html
    );
    assert_eq!(
        owner(&harness, SUBJECT).await,
        Some(id(&bob)),
        "nothing moved"
    );

    // So does an admin's link.
    let refused = harness
        .sso
        .add_oidc_link(Uuid::nil(), id(&alice), "fake", SUBJECT)
        .await
        .expect_err("taken");
    assert!(matches!(refused, ServiceError::Conflict(_)), "{refused:?}");
    assert_eq!(
        owner(&harness, SUBJECT).await,
        Some(id(&bob)),
        "nothing moved"
    );
}

/// D22: a self-service link never replaces one. An account linked at the
/// issuer cannot start another link there, and a flow started before the
/// account was linked is refused at its callback, under the lock every link
/// write holds; nothing is replaced.
#[tokio::test]
async fn a_self_service_link_never_replaces_one() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (location, cookie, _) = start_link(&harness, &alice).await;
    assert_eq!(link(&harness, &alice).await.status, 200);

    // Linked since this flow started: its callback is refused.
    harness
        .fake
        .sign_in(|s| s.subject = "alice-second".to_owned());
    let page = finish_at(&harness, &harness.fake, "fake", &location, &cookie).await;
    assert_eq!(page.status, 409, "{}", page.html);
    assert!(page.html.contains("Remove it first"), "{}", page.html);
    assert_eq!(owner(&harness, "alice-second").await, None);
    assert_eq!(owner(&harness, SUBJECT).await, Some(id(&alice)));

    // And no new start, before any password is tried.
    let (token, _) = signed_in(&harness, &alice).await;
    let refused = start_with(&harness, "fake", &alice, "not-her-password", &token)
        .await
        .expect_err("linked already");
    assert!(matches!(refused, ServiceError::Conflict(_)), "{refused:?}");
    assert_eq!(
        reread(&harness, &alice).await.invalid_login_attempt_count,
        0
    );
    assert_eq!(
        harness
            .sso
            .oidc_links(id(&alice))
            .await
            .expect("links")
            .links
            .len(),
        1
    );
}

/// Two flows of one account whose checks passed, at two subjects of one
/// issuer, link at once: one links, the other is refused, as the lock the
/// link writes hold orders them. (The fake names its subject when it issues
/// the token, so two callbacks through it at once would be one identity.)
#[tokio::test]
async fn of_two_link_flows_at_once_one_links() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let session = session_hash(token.expose());
    let provider = harness.provider("fake").await;
    let identity = |subject: &str| VerifiedIdentity {
        provider_key: "fake".to_owned(),
        issuer: harness.fake.issuer(),
        subject: subject.to_owned(),
        username: None,
        claims: serde_json::Map::new(),
    };

    let (a, b) = tokio::join!(
        harness
            .sso
            .link_verified(&provider, id(&alice), &session, identity("alice-first")),
        harness
            .sso
            .link_verified(&provider, id(&alice), &session, identity("alice-second"))
    );
    let mut statuses = [a.expect("an answer").status, b.expect("an answer").status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409]);
    let links = harness.sso.all_oidc_links().await.expect("links");
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(entries(&harness, LINKED_ENTRY).await.len(), 1);
}

/// D22: a link flow ends with the session that started it, whichever way
/// it ends: a sign-out, its device removed (the dashboard's Devices page),
/// or every other session signed out (a password change). A flow of another
/// live session of the account still links.
#[tokio::test]
async fn a_link_flow_ends_with_the_session_that_started_it() {
    for ending in ["sign-out", "device removed", "password changed"] {
        let harness = Harness::new().await;
        let alice = local(&harness, "alice").await;
        let (location, cookie, token) = start_link(&harness, &alice).await;
        match ending {
            "sign-out" => harness
                .sessions
                .logout(token.expose())
                .await
                .expect("signed out"),
            "device removed" => {
                let devices = harness
                    .devices
                    .get_devices(&ferrofin_traits::devices::DeviceQuery {
                        user_id: Some(id(&alice)),
                        ..Default::default()
                    })
                    .await
                    .expect("devices");
                for device in &devices.items {
                    harness
                        .sessions
                        .logout_device(device)
                        .await
                        .expect("removed");
                }
            }
            _ => {
                // `POST /Users/Password` keeps the changing session only.
                let (keeper, _) = signed_in(&harness, &alice).await;
                harness
                    .sessions
                    .revoke_user_tokens(id(&alice), keeper.expose())
                    .await
                    .expect("revoked");
            }
        }
        harness
            .fake
            .sign_in(|s| s.subject = "mallory-sub".to_owned());
        let page = finish_at(&harness, &harness.fake, "fake", &location, &cookie).await;
        assert_eq!(page.status, 400, "{ending}: {}", page.html);
        assert!(page.html.contains("has ended"), "{ending}: {}", page.html);
        assert_eq!(owner(&harness, "mallory-sub").await, None, "{ending}");
        assert_eq!(entries(&harness, LINKED_ENTRY).await.len(), 0, "{ending}");

        // A flow of a session that is still live links.
        assert_eq!(link(&harness, &alice).await.status, 200, "{ending}");
        assert_eq!(
            owner(&harness, "mallory-sub").await,
            Some(id(&alice)),
            "{ending}"
        );
    }
}

/// The security review's reproduction, at the seam: a flow started with the
/// account's password, whose session then signs out, completed by another
/// identity, links nothing; the account keeps its own sign-in.
#[tokio::test]
async fn the_reviews_takeover_is_refused() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (location, cookie, token) = start_link(&harness, &alice).await;
    harness
        .sessions
        .logout(token.expose())
        .await
        .expect("signed out");
    harness
        .fake
        .sign_in(|s| s.subject = "mallory-sub".to_owned());
    let page = finish_at(&harness, &harness.fake, "fake", &location, &cookie).await;
    assert_eq!(page.status, 400, "{}", page.html);
    assert_eq!(harness.sso.all_oidc_links().await.expect("links"), []);
    let refused = harness.session("mallory-browser").await;
    assert!(
        !refused.is_ok_and(|session| session.user.id == id(&alice)),
        "the identity reaches no account of alice's"
    );
    assert!(
        harness
            .users
            .authenticate_user("alice", PASSWORD, "192.0.2.9", true)
            .await
            .expect("a password sign-in")
            .is_some()
    );
}

#[tokio::test]
async fn a_user_lists_their_own_links_and_the_providers_to_link_at() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let none = harness.sso.oidc_links(id(&alice)).await.expect("links");
    assert!(!none.single_sign_on_account);
    assert_eq!(none.links, []);
    assert_eq!(
        none.providers,
        [OidcLinkProvider {
            key: "fake".to_owned(),
            name: "Fake <IdP>".to_owned(),
            linked: false,
        }]
    );
    assert_eq!(link(&harness, &alice).await.status, 200);

    let links = harness.sso.oidc_links(id(&alice)).await.expect("links");
    assert_eq!(links.links.len(), 1);
    assert_eq!(links.links[0].provider.as_deref(), Some("fake"));
    assert_eq!(links.links[0].provider_name.as_deref(), Some("Fake <IdP>"));
    assert_eq!(links.links[0].issuer, harness.fake.issuer());
    assert!(links.providers[0].linked, "linked at its issuer");
    let json = serde_json::to_string(&links).expect("serializes");
    assert!(!json.contains(SUBJECT), "never a subject: {json}");
}

#[tokio::test]
async fn an_account_sign_in_created_is_marked_single_sign_on_only() {
    let harness = Harness::new().await;
    harness.fake.sign_in(|s| s.subject = "carol-sub".to_owned());
    let session = harness.session("browser-1").await.expect("signed in");
    let carol = session.user.id;
    let links = harness.sso.oidc_links(carol).await.expect("links");
    assert!(links.single_sign_on_account);
    assert_eq!(links.links.len(), 1);
    let listed = harness.sso.all_oidc_links().await.expect("all");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].single_sign_on_account);
    assert_eq!(listed[0].user_name, USERNAME);
}

#[tokio::test]
async fn removing_a_link_signs_every_session_of_the_account_out() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    let tokens = [
        harness.session("browser-1").await.expect("signed in"),
        harness.session("browser-2").await.expect("signed in"),
    ];

    harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");

    assert_eq!(owner(&harness, SUBJECT).await, None);
    for (session, device_id) in tokens.iter().zip(["browser-1", "browser-2"]) {
        let found = harness
            .sessions
            .get_session_by_authentication_token(
                session.access_token.expose(),
                device_id,
                "192.0.2.9",
            )
            .await;
        assert!(found.is_err(), "{device_id}: {found:?}");
    }
    // A second removal finds nothing, and the identity's sign-in no longer
    // reaches alice.
    let again = harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect_err("gone");
    assert!(matches!(again, ServiceError::NotFound(_)), "{again:?}");
    let signed_in = harness.session("browser-3").await;
    assert!(
        !signed_in.is_ok_and(|session| session.user.id == id(&alice)),
        "the sign-in no longer finds alice"
    );
}

#[tokio::test]
async fn an_admin_adds_lists_and_removes_any_link() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let added = harness
        .sso
        .add_oidc_link(Uuid::nil(), id(&alice), "fake", "alice-by-hand")
        .await
        .expect("linked");
    assert_eq!(added.user_id, id(&alice));
    assert_eq!(added.user_name, "alice");
    assert_eq!(added.provider.as_deref(), Some("fake"));
    assert_eq!(added.issuer, harness.fake.issuer());
    assert_eq!(added.subject, "alice-by-hand");
    assert!(!added.single_sign_on_account);
    assert_eq!(
        harness.sso.all_oidc_links().await.expect("all"),
        std::slice::from_ref(&added)
    );
    // D5 for an admin's link too.
    let after = reread(&harness, &alice).await;
    assert_eq!(
        after.authentication_provider_id,
        alice.authentication_provider_id
    );
    assert_eq!(after.password, alice.password);

    for (subject, why) in [
        ("", "empty"),
        ("a\nb", "a control character"),
        (" alice-by-hand", "a space before"),
    ] {
        let refused = harness
            .sso
            .add_oidc_link(Uuid::nil(), id(&alice), "fake", subject)
            .await
            .expect_err(why);
        assert!(matches!(refused, ServiceError::InvalidInput(_)), "{why}");
    }
    let long = "s".repeat(256);
    assert!(matches!(
        harness
            .sso
            .add_oidc_link(Uuid::nil(), id(&alice), "fake", &long)
            .await,
        Err(ServiceError::InvalidInput(_))
    ));
    // The admin's link replaces the account's link at the issuer.
    assert!(
        harness
            .sso
            .add_oidc_link(Uuid::nil(), id(&alice), "fake", &long[..255])
            .await
            .is_ok()
    );
    assert_eq!(owner(&harness, "alice-by-hand").await, None, "replaced");
    assert_eq!(owner(&harness, &long[..255]).await, Some(id(&alice)));
    assert!(matches!(
        harness
            .sso
            .add_oidc_link(Uuid::nil(), Uuid::nil(), "fake", "x")
            .await,
        Err(ServiceError::InvalidInput(_))
    ));
    for (user, key) in [(Uuid::from_u128(0xDEAD), "fake"), (id(&alice), "nope")] {
        assert!(
            matches!(
                harness.sso.add_oidc_link(Uuid::nil(), user, key, "x").await,
                Err(ServiceError::NotFound(_))
            ),
            "{user} {key}"
        );
    }

    // Removed by the link's issuer, which no provider may still have.
    let issuer = harness.fake.issuer();
    harness
        .sso
        .remove_oidc_link(Uuid::nil(), id(&alice), OidcLinkAt::Issuer(&issuer))
        .await
        .expect("removed");
    assert_eq!(harness.sso.all_oidc_links().await.expect("all"), []);
    for at in [
        OidcLinkAt::Issuer("https://other.example"),
        OidcLinkAt::Provider("nope"),
    ] {
        assert!(
            matches!(
                harness
                    .sso
                    .remove_oidc_link(Uuid::nil(), id(&alice), at)
                    .await,
                Err(ServiceError::NotFound(_))
            ),
            "{at:?}"
        );
    }
}

/// Each link made and removed writes an activity entry naming the account,
/// the provider and who did it: the user, an administrator, an API key. An
/// admin's replacement is a removal and a link. The subject is in the
/// overview only.
#[tokio::test]
async fn links_made_and_removed_are_in_the_activity_log() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let root = local(&harness, "root").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");
    harness
        .sso
        .add_oidc_link(id(&root), id(&alice), "fake", "alice-by-hand")
        .await
        .expect("linked");
    harness
        .sso
        .add_oidc_link(Uuid::nil(), id(&alice), "fake", "alice-replaced")
        .await
        .expect("replaced");
    // The same link again changes nothing, and writes nothing.
    harness
        .sso
        .add_oidc_link(Uuid::nil(), id(&alice), "fake", "alice-replaced")
        .await
        .expect("kept");
    harness
        .sso
        .remove_oidc_link(id(&root), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");

    let seen = |entries: Vec<ferrofin_model::activity::ActivityLogEntry>| {
        entries
            .into_iter()
            .map(|entry| {
                assert_eq!(entry.user_id, id(&alice));
                assert_eq!(
                    entry.severity,
                    ferrofin_model::activity::LogLevel::Information
                );
                let overview = entry.overview.expect("an overview");
                assert!(overview.starts_with(&format!("Issuer: {}\n", harness.fake.issuer())));
                let short = entry.short_overview.expect("a short overview");
                let subject = overview
                    .rsplit("Subject: ")
                    .next()
                    .expect("a subject")
                    .to_owned();
                assert!(!short.contains(&subject), "{short}");
                (entry.name, short, subject)
            })
            .collect::<Vec<_>>()
    };
    let linked = |subject: &str, by: &str| {
        (
            "A sign-in at Fake <IdP> was linked to alice".to_owned(),
            by.to_owned(),
            subject.to_owned(),
        )
    };
    let unlinked = |subject: &str, by: &str| {
        (
            "A sign-in at Fake <IdP> was unlinked from alice".to_owned(),
            by.to_owned(),
            subject.to_owned(),
        )
    };
    assert_eq!(
        seen(entries(&harness, LINKED_ENTRY).await),
        [
            linked("alice-replaced", "By an API key"),
            linked("alice-by-hand", "By the administrator root"),
            linked(SUBJECT, "By alice"),
        ]
    );
    assert_eq!(
        seen(entries(&harness, UNLINKED_ENTRY).await),
        [
            unlinked("alice-replaced", "By the administrator root"),
            unlinked("alice-by-hand", "By an API key"),
            unlinked(SUBJECT, "By alice"),
        ]
    );
}

/// The password is in no log line and no error, whether it is right or
/// wrong.
#[tokio::test]
async fn the_password_is_never_logged() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let logs = crate::tests::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let wrong = "wrong-pw-5d0b9a";
    let refused = start_with(&harness, "fake", &alice, wrong, &token)
        .await
        .expect_err("refused");
    assert!(!format!("{refused:?} {refused}").contains(wrong));
    started(
        start_with(&harness, "fake", &alice, PASSWORD, &token)
            .await
            .expect("starts"),
    );
    let text = logs.text();
    assert!(
        text.contains("login failed"),
        "the failure is logged: {text}"
    );
    for secret in [wrong, PASSWORD, token.expose()] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}

#[tokio::test]
async fn deleting_an_account_removes_its_links() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    // The last account cannot be deleted.
    local(&harness, "bob").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    harness
        .users
        .delete_user(id(&alice))
        .await
        .expect("deleted");
    assert_eq!(harness.sso.all_oidc_links().await.expect("all"), []);
    assert_eq!(owner(&harness, SUBJECT).await, None);
}

#[tokio::test]
async fn the_link_page_runs_its_own_script_only() {
    let harness = Harness::new().await;
    let page = harness.sso.oidc_link_page().await.expect("the page");
    assert_eq!(page.status, 200);
    assert!(page.html.contains(LINK_SCRIPT));
    assert!(page.html.contains(r#"<a href="../web/">"#));
    assert!(
        page.html
            .contains("cannot sign in at all once its last link is removed")
    );
    // It asks for the password, and says what an account without one does.
    assert!(
        page.html
            .contains(r#"<input id="password" type="password" autocomplete="current-password">"#)
    );
    assert!(
        page.html
            .contains(&escape_html(SINGLE_SIGN_ON_ACCOUNT_REFUSAL))
    );
    assert!(page.html.contains("Linking never replaces a link"));
    let policy = page.content_security_policy.expect("a policy");
    assert_eq!(policy, *LINK_POLICY);
    assert!(
        policy.contains(&format!(
            "script-src 'sha256-{}'",
            STANDARD.encode(Sha256::digest(LINK_SCRIPT))
        )),
        "{policy}"
    );
    assert_eq!(page.set_cookies, Vec::<String>::new());
    // The script sends the password once, in the start's body, and empties
    // the field once it is sent.
    assert!(LINK_SCRIPT.contains("{ Password: password }"));
    let (emptied, sent) = (
        LINK_SCRIPT.find("passwordField.value = '';"),
        LINK_SCRIPT.find("{ Password: password }"),
    );
    assert!(
        emptied < sent && emptied.is_some(),
        "emptied before it is sent"
    );
}

/// B1: while the plugin is disabled, no link route does anything.
#[tokio::test]
async fn a_disabled_plugin_has_no_links() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    harness
        .sso
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disabled");
    let user = id(&alice);
    let sso = &harness.sso;
    let not_found = |result: Result<(), ServiceError>| {
        assert!(
            matches!(result, Err(ServiceError::NotFound(_))),
            "{result:?}"
        );
    };
    not_found(sso.oidc_link_page().await.map(drop));
    not_found(sso.oidc_links(user).await.map(drop));
    not_found(
        start_with(&harness, "fake", &alice, PASSWORD, &token)
            .await
            .map(drop),
    );
    not_found(sso.all_oidc_links().await.map(drop));
    not_found(sso.add_oidc_link(user, user, "fake", "x").await.map(drop));
    not_found(
        sso.remove_oidc_link(user, user, OidcLinkAt::Provider("fake"))
            .await,
    );
    assert_eq!(
        reread(&harness, &alice).await.invalid_login_attempt_count,
        0,
        "no password was tried"
    );
}

/// The settings page's linked-accounts section calls the admin routes, warns
/// before a removal, and sets every string from a response as text.
#[test]
fn the_settings_page_lists_adds_and_removes_links() {
    use crate::Extension as _;
    let html = String::from_utf8(crate::sso::SsoExtension.config_pages().remove(0).bytes)
        .expect("the page is UTF-8");
    for call in [
        "ApiClient.getUrl('sso/admin/links')",
        "ApiClient.getUrl('sso/admin/links', { userId: link.UserId, issuer: link.Issuer })",
        "type: 'POST'",
        "ApiClient.getUsers()",
        "ApiClient.getUrl('sso/link')",
        "Dashboard.confirm(last ? LAST_LINK_MESSAGE : REMOVE_MESSAGE,",
        "data-sso-links-bound",
    ] {
        assert!(html.contains(call), "{call}");
    }
    assert!(
        html.contains("cannot sign in at all until it is linked"),
        "{html}"
    );
    assert!(html.contains("giving their password"), "{html}");
}

/// A link flow whose account was deleted before its callback, or whose
/// identity has an empty subject, links nothing.
#[tokio::test]
async fn a_link_to_a_deleted_account_or_an_empty_subject_links_nothing() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let bob = local(&harness, "bob").await;
    let (location, cookie, _) = start_link(&harness, &alice).await;
    harness
        .users
        .delete_user(id(&alice))
        .await
        .expect("deleted");
    let page = finish_at(&harness, &harness.fake, "fake", &location, &cookie).await;
    assert_eq!(page.status, 400, "{}", page.html);
    assert!(page.html.contains("Link refused"), "{}", page.html);
    assert_eq!(owner(&harness, SUBJECT).await, None);

    let provider =
        crate::sso::sign_in::sign_in_provider(&harness.sso.config().await.expect("config"), "fake")
            .expect("the provider");
    let empty = VerifiedIdentity {
        provider_key: "fake".to_owned(),
        issuer: harness.fake.issuer(),
        subject: String::new(),
        username: None,
        claims: serde_json::Map::new(),
    };
    let (token, _) = signed_in(&harness, &bob).await;
    let page = harness
        .sso
        .link_verified(&provider, id(&bob), &session_hash(token.expose()), empty)
        .await
        .expect("an answer");
    assert_eq!(page.status, 400);
    assert!(
        page.html.contains("Linking failed. Try again."),
        "{}",
        page.html
    );
    assert_eq!(
        harness.sso.oidc_links(id(&bob)).await.expect("links").links,
        []
    );
}

/// The completion page `completed` holds, swapped for a session by the
/// device `device_id`.
async fn swap(
    harness: &Harness,
    (page, cookie): &(SsoReply, String),
    device_id: &str,
) -> OidcExchangeReply {
    let code = crate::sso::sign_in::tests::exchange_code(&page.html);
    harness
        .exchange(&code, Some(cookie), device(device_id))
        .await
}

/// A sign-in verified just before the link was removed, whose completion
/// page has not yet swapped its code, opens no session after the removal.
#[tokio::test]
async fn a_code_issued_before_the_removal_opens_no_session() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let bob = local(&harness, "bob").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    // Two sign-ins verified for alice, their codes not swapped yet.
    let first = harness.completed().await;
    let second = harness.completed().await;
    for (page, _) in [&first, &second] {
        assert_eq!(page.status, 200, "{}", page.html);
    }
    let refused = |reply: OidcExchangeReply| {
        assert_eq!(
            reply.outcome.map(|session| session.user.name),
            Err(OidcExchangeRefusal {
                status: 400,
                message: "Sign-in failed. Try again.".to_owned()
            })
        );
    };
    harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");
    // Unlinked: no link names the identity.
    refused(swap(&harness, &first, "browser-late-1").await);
    // Linked to bob since: the code was alice's.
    harness
        .sso
        .add_oidc_link(Uuid::nil(), id(&bob), "fake", SUBJECT)
        .await
        .expect("bob linked");
    refused(swap(&harness, &second, "browser-late-2").await);
}

/// A link flow the account started at one provider before a removal of its
/// link at another links nothing when it is called back, whoever signs in:
/// the removal ends it with the account's sessions. Another account's link
/// flow is left alone.
#[tokio::test]
async fn a_link_flow_started_before_a_removal_links_nothing() {
    let harness = Harness::new().await;
    let other = other_provider(&harness).await;
    let alice = local(&harness, "alice").await;
    let bob = local(&harness, "bob").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    let (alice_token, _) = signed_in(&harness, &alice).await;
    let (alice_at, alice_cookie) = started(
        start_with(&harness, "other", &alice, PASSWORD, &alice_token)
            .await
            .expect("starts"),
    );
    let (bob_at, bob_cookie, _) = start_link(&harness, &bob).await;

    harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");

    other.sign_in(|s| s.subject = "mallory-sub".to_owned());
    let page = finish_at(&harness, &other, "other", &alice_at, &alice_cookie).await;
    assert_eq!(page.status, 400, "{}", page.html);
    assert_eq!(
        owner_at(&harness, &other.issuer(), "mallory-sub").await,
        None
    );
    assert_eq!(
        harness
            .sso
            .oidc_links(id(&alice))
            .await
            .expect("links")
            .links,
        []
    );

    harness.fake.sign_in(|s| s.subject = "bob-sub".to_owned());
    let page = finish_at(&harness, &harness.fake, "fake", &bob_at, &bob_cookie).await;
    assert_eq!(page.status, 200, "{}", page.html);
    assert_eq!(owner(&harness, "bob-sub").await, Some(id(&bob)));
}

/// A link whose provider was deleted is listed without one, and removed by
/// its issuer, which no provider key names any more.
#[tokio::test]
async fn a_link_whose_provider_is_gone_is_listed_and_removed_by_its_issuer() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    harness
        .sso
        .delete_oidc_provider("fake")
        .await
        .expect("deleted");

    let mine = harness.sso.oidc_links(id(&alice)).await.expect("links");
    assert_eq!(mine.links.len(), 1);
    assert_eq!(mine.links[0].provider, None);
    assert_eq!(mine.providers, []);
    let all = harness.sso.all_oidc_links().await.expect("all");
    assert_eq!(all.len(), 1);
    let listed = serde_json::to_value(&all[0]).expect("serializes");
    assert!(listed.get("Provider").is_none(), "{listed}");
    assert!(listed.get("ProviderName").is_none(), "{listed}");

    assert!(matches!(
        harness
            .sso
            .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
            .await,
        Err(ServiceError::NotFound(_))
    ));
    let issuer = harness.fake.issuer();
    harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Issuer(&issuer))
        .await
        .expect("removed");
    assert_eq!(harness.sso.all_oidc_links().await.expect("all"), []);
    // Its entry names the issuer, as no provider has it.
    assert_eq!(
        entries(&harness, UNLINKED_ENTRY).await[0].name,
        format!("A sign-in at {issuer} was unlinked from alice")
    );
}

/// A removal that starts while an exchange is minting its session waits for
/// it, then signs that session out: the exchange holds the lock the removal
/// unlinks under from its lookup of the link to its session.
#[tokio::test]
async fn a_removal_during_an_exchange_signs_its_session_out() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    let completed = harness.completed().await;
    // The session's success event stalls the exchange after its session is
    // minted, until released.
    let minting = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (entered, waiting) = (Arc::clone(&minting), Arc::clone(&release));
    harness.events.subscribe(
        "AuthenticationSucceeded",
        Arc::new(move |_: &str| -> ferrofin_traits::events::EventFuture {
            let (entered, waiting) = (Arc::clone(&entered), Arc::clone(&waiting));
            Box::pin(async move {
                entered.notify_one();
                waiting.notified().await;
                Ok(())
            })
        }),
    );

    let removal = async {
        tokio::time::timeout(std::time::Duration::from_secs(30), minting.notified())
            .await
            .expect("the exchange reaches its success event");
        let removing =
            harness
                .sso
                .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"));
        tokio::pin!(removing);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), &mut removing)
                .await
                .is_err(),
            "the removal waits for the exchange"
        );
        release.notify_one();
        removing.await
    };
    let (reply, removed) = tokio::join!(swap(&harness, &completed, "browser-race"), removal);
    removed.expect("removed");
    let session = reply.outcome.expect("minted before the removal");
    assert!(
        harness
            .sessions
            .get_session_by_authentication_token(
                session.access_token.expose(),
                "browser-race",
                "192.0.2.9",
            )
            .await
            .is_err(),
        "and signed out by it"
    );
}

/// A link flow called back while a removal is signing the account out waits
/// for the sign-out, then links nothing: the removal signs out under the
/// lock the callback takes for its session check and its link, so no
/// callback falls between the unlink and the sign-out.
#[tokio::test]
async fn a_link_flow_called_back_during_a_removal_waits_and_links_nothing() {
    let harness = Harness::new().await;
    let other = other_provider(&harness).await;
    let alice = local(&harness, "alice").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    let (token, _) = signed_in(&harness, &alice).await;
    let (at, cookie) = started(
        start_with(&harness, "other", &alice, PASSWORD, &token)
            .await
            .expect("starts"),
    );
    let back = callback_at(&other, &at, Some(&cookie)).await;
    // Each `SessionEnded` the removal's sign-out publishes stalls until
    // released.
    let ending = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (entered, waiting) = (Arc::clone(&ending), Arc::clone(&gate));
    harness.events.subscribe(
        "SessionEnded",
        Arc::new(move |_: &str| -> ferrofin_traits::events::EventFuture {
            let (entered, waiting) = (Arc::clone(&entered), Arc::clone(&waiting));
            Box::pin(async move {
                entered.notify_one();
                drop(waiting.acquire().await);
                Ok(())
            })
        }),
    );

    let removal =
        harness
            .sso
            .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"));
    let callback = async {
        tokio::time::timeout(std::time::Duration::from_secs(30), ending.notified())
            .await
            .expect("the removal reaches its sign-out");
        let finishing = harness.sso.finish_oidc_sign_in("other", back);
        tokio::pin!(finishing);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), &mut finishing)
                .await
                .is_err(),
            "the callback waits for the sign-out"
        );
        gate.add_permits(1);
        finishing.await
    };
    let (removed, page) = tokio::join!(removal, callback);
    removed.expect("removed");
    let page = page.expect("an answer");
    assert_eq!(page.status, 400, "{}", page.html);
    assert!(page.html.contains("has ended"), "{}", page.html);
    assert_eq!(owner_at(&harness, &other.issuer(), SUBJECT).await, None);
    assert_eq!(harness.sso.all_oidc_links().await.expect("links"), []);
}

/// The newest activity entry of `kind`, once one is written.
async fn entry_written(
    harness: &Harness,
    kind: &str,
) -> ferrofin_model::activity::ActivityLogEntry {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let Some(entry) = entries(harness, kind).await.into_iter().next() {
                return entry;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the entry is written")
}

/// A removal whose request is dropped while it signs the account out still
/// signs every session out and leaves its activity entry: the removal runs
/// in a task of its own, which outlives the request, and its entry does not
/// wait for the sign-out.
#[tokio::test]
async fn a_removal_dropped_during_its_sign_out_still_signs_every_session_out() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    let tokens = [
        harness.session("browser-1").await.expect("signed in"),
        harness.session("browser-2").await.expect("signed in"),
    ];
    // Each of the sign-out's `SessionEnded` stalls until released: the
    // request is dropped at the first.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let waiting = Arc::clone(&gate);
    harness.events.subscribe(
        "SessionEnded",
        Arc::new(move |_: &str| -> ferrofin_traits::events::EventFuture {
            let waiting = Arc::clone(&waiting);
            Box::pin(async move {
                drop(waiting.acquire().await);
                Ok(())
            })
        }),
    );
    let removing =
        harness
            .sso
            .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), removing)
            .await
            .is_err(),
        "the removal stalls in its sign-out, and is dropped"
    );
    assert_eq!(
        entry_written(&harness, UNLINKED_ENTRY).await.name,
        "A sign-in at Fake <IdP> was unlinked from alice"
    );

    gate.add_permits(16);
    let signed_out = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let mut live = 0;
            for (session, device_id) in tokens.iter().zip(["browser-1", "browser-2"]) {
                let found = harness
                    .sessions
                    .get_session_by_authentication_token(
                        session.access_token.expose(),
                        device_id,
                        "192.0.2.9",
                    )
                    .await;
                live += usize::from(found.is_ok());
            }
            if live == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        signed_out.is_ok(),
        "every session of the account is signed out"
    );
    assert_eq!(owner(&harness, SUBJECT).await, None);
}

/// What a test runs as the identity store works: before and after a link is
/// written (`"write"`, `"written"`).
type Step = Arc<dyn Fn(&'static str) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

/// The identity store with a test's [`Step`] at each of those points: what
/// another server, or a sign-out by a route that takes no lock, does there.
struct Interleaved {
    inner: Arc<FerrofinExternalIdentityStore>,
    step: Step,
}

#[async_trait::async_trait]
impl ExternalIdentityStore for Interleaved {
    async fn find(
        &self,
        identity: ExternalIdentityKey<'_>,
    ) -> Result<Option<ExternalIdentity>, ServiceError> {
        self.inner.find(identity).await
    }
    async fn list(&self, provider_id: &str) -> Result<Vec<ExternalIdentity>, ServiceError> {
        self.inner.list(provider_id).await
    }
    async fn list_for_user(
        &self,
        provider_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ExternalIdentity>, ServiceError> {
        self.inner.list_for_user(provider_id, user_id).await
    }
    async fn link(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<ExternalIdentity, ServiceError> {
        (self.step)("write").await;
        let link = self.inner.link(user_id, identity, avatar_hash).await;
        (self.step)("written").await;
        link
    }
    async fn link_unless_taken(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        by: LinkBy,
    ) -> Result<ConditionalLink, ServiceError> {
        (self.step)("write").await;
        let outcome = self.inner.link_unless_taken(user_id, identity, by).await;
        (self.step)("written").await;
        outcome
    }
    async fn set_avatar_hash(
        &self,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<bool, ServiceError> {
        self.inner.set_avatar_hash(identity, avatar_hash).await
    }
    async fn unlink(&self, identity: ExternalIdentityKey<'_>) -> Result<bool, ServiceError> {
        self.inner.unlink(identity).await
    }
}

/// Another server on the harness's database (a second pod in a rolling
/// update): its own service, lock and pending flows over the same accounts,
/// links, sessions and log, with `step` in its identity store.
fn server(harness: &Harness, step: &Step) -> SsoService {
    SsoService::new(
        Arc::clone(&harness.sso.plugins),
        Arc::clone(&harness.users) as Arc<dyn UserManager>,
        Arc::new(Interleaved {
            inner: Arc::clone(&harness.identities),
            step: Arc::clone(step),
        }),
        Arc::clone(&harness.sso.sessions),
        Arc::clone(&harness.sso.activity),
        Arc::clone(&harness.sso.localization),
        Some(PUBLIC_URL.to_owned()),
    )
}

/// A [`Step`] that does nothing but at `at`, where it runs `then`.
fn at<F>(name: &'static str, then: impl Fn() -> F + Send + Sync + 'static) -> Step
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    Arc::new(move |point| {
        if point == name {
            Box::pin(then())
        } else {
            Box::pin(std::future::ready(()))
        }
    })
}

/// The fake's identity `subject`, verified.
fn verified(harness: &Harness, subject: &str) -> VerifiedIdentity {
    VerifiedIdentity {
        provider_key: "fake".to_owned(),
        issuer: harness.fake.issuer(),
        subject: subject.to_owned(),
        username: None,
        claims: serde_json::Map::new(),
    }
}

/// Two servers, each with its own lock, link two flows of one account at
/// once: one links, the other is refused, and nothing is replaced. The
/// store holds each at its write until both are there, so any check made
/// before the write has passed on both.
#[tokio::test]
async fn two_servers_link_one_of_an_accounts_two_flows() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let session = session_hash(token.expose());
    let provider = harness.provider("fake").await;
    let together = Arc::new(tokio::sync::Barrier::new(2));
    let step = at("write", move || {
        let together = Arc::clone(&together);
        async move {
            together.wait().await;
        }
    });
    let (one, two) = (server(&harness, &step), server(&harness, &step));

    let (a, b) = tokio::join!(
        one.link_verified(
            &provider,
            id(&alice),
            &session,
            verified(&harness, "alice-first")
        ),
        two.link_verified(
            &provider,
            id(&alice),
            &session,
            verified(&harness, "alice-second")
        )
    );
    let mut statuses = [a.expect("an answer").status, b.expect("an answer").status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409]);
    let links = harness.sso.all_oidc_links().await.expect("links");
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(entries(&harness, LINKED_ENTRY).await.len(), 1);
    assert_eq!(
        entries(&harness, UNLINKED_ENTRY).await.len(),
        0,
        "nothing replaced"
    );
}

/// Two servers link one identity to two accounts at once, a flow on one and
/// an administrator on the other, held together as above: one links, the
/// other is refused, and the identity never moves.
#[tokio::test]
async fn two_servers_never_move_an_identity_between_accounts() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let bob = local(&harness, "bob").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let session = session_hash(token.expose());
    let provider = harness.provider("fake").await;
    let together = Arc::new(tokio::sync::Barrier::new(2));
    let step = at("write", move || {
        let together = Arc::clone(&together);
        async move {
            together.wait().await;
        }
    });
    let (one, two) = (server(&harness, &step), server(&harness, &step));

    let (flow, admin) = tokio::join!(
        one.link_verified(&provider, id(&alice), &session, verified(&harness, SUBJECT)),
        two.add_oidc_link(Uuid::nil(), id(&bob), "fake", SUBJECT)
    );
    let flow = flow.expect("an answer");
    let linked_to = match admin {
        Ok(_) => {
            assert_eq!(flow.status, 409, "{}", flow.html);
            id(&bob)
        }
        Err(refused) => {
            assert!(matches!(refused, ServiceError::Conflict(_)), "{refused:?}");
            assert_eq!(flow.status, 200, "{}", flow.html);
            id(&alice)
        }
    };
    assert_eq!(owner(&harness, SUBJECT).await, Some(linked_to));
    assert_eq!(entries(&harness, LINKED_ENTRY).await.len(), 1);
    assert_eq!(
        entries(&harness, UNLINKED_ENTRY).await.len(),
        0,
        "nothing moved"
    );
}

/// A sign-out by a route that takes no lock, landing just before a link
/// flow's link is written, links nothing: the session check is part of the
/// write.
#[tokio::test]
async fn a_sign_out_just_before_the_write_links_nothing() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let session = session_hash(token.expose());
    let provider = harness.provider("fake").await;
    let sessions = Arc::clone(&harness.sessions);
    let signed_out = token.expose().to_owned();
    let step = at("write", move || {
        let (sessions, token) = (Arc::clone(&sessions), signed_out.clone());
        async move {
            sessions.logout(&token).await.expect("signed out");
        }
    });

    let page = server(&harness, &step)
        .link_verified(&provider, id(&alice), &session, verified(&harness, SUBJECT))
        .await
        .expect("an answer");
    assert_eq!(page.status, 400, "{}", page.html);
    assert!(page.html.contains("has ended"), "{}", page.html);
    assert_eq!(owner(&harness, SUBJECT).await, None);
    assert_eq!(entries(&harness, LINKED_ENTRY).await.len(), 0);
}

/// A link whose request is dropped after its write, before its activity
/// entry, still ends with the entry: the write and the entry run in a task
/// of their own, which outlives the request.
#[tokio::test]
async fn a_link_dropped_after_its_write_still_leaves_its_entry() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let (token, _) = signed_in(&harness, &alice).await;
    let session = session_hash(token.expose());
    let provider = harness.provider("fake").await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let waiting = Arc::clone(&gate);
    let step = at("written", move || {
        let waiting = Arc::clone(&waiting);
        async move {
            drop(waiting.acquire().await);
        }
    });

    let sso = server(&harness, &step);
    let linking = sso.link_verified(&provider, id(&alice), &session, verified(&harness, SUBJECT));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), linking)
            .await
            .is_err(),
        "the link stalls after its write, and is dropped"
    );
    gate.add_permits(1);
    assert_eq!(
        entry_written(&harness, LINKED_ENTRY).await.name,
        "A sign-in at Fake <IdP> was linked to alice"
    );
    assert_eq!(owner(&harness, SUBJECT).await, Some(id(&alice)));
}

/// B8 applies to a link as to a sign-in: an identity without the provider's
/// required role is refused, with a page saying so, and nothing is linked;
/// with the role it links.
#[tokio::test]
async fn an_identity_without_the_required_role_links_nothing() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        "ferrofin-users".clone_into(&mut provider.required_role);
    })
    .await;
    let alice = local(&harness, "alice").await;
    let roles = |groups: serde_json::Value| {
        harness.fake.sign_in(|sign_in| {
            sign_in.claims.insert("groups".to_owned(), groups);
        });
    };
    roles(serde_json::json!(["another-group"]));
    let page = link(&harness, &alice).await;
    assert_eq!(page.status, 403, "{}", page.html);
    assert!(page.html.contains("Link refused"), "{}", page.html);
    assert!(
        page.html
            .contains("does not have the role this server requires"),
        "{}",
        page.html
    );
    assert!(!page.html.contains("ferrofin-users"), "no role on the page");
    assert_eq!(owner(&harness, SUBJECT).await, None);

    roles(serde_json::json!(["ferrofin-users"]));
    assert_eq!(link(&harness, &alice).await.status, 200);
    assert_eq!(owner(&harness, SUBJECT).await, Some(id(&alice)));
}

/// An exchange whose identity was unlinked since its callback is refused
/// before role sync: the account's rights are left as they were.
#[tokio::test]
async fn an_unlinked_exchange_syncs_no_roles() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.role_sync = true;
        "ferrofin-admins".clone_into(&mut provider.admin_role);
    })
    .await;
    let alice = local(&harness, "alice").await;
    harness.fake.sign_in(|sign_in| {
        sign_in
            .claims
            .insert("groups".to_owned(), serde_json::json!(["ferrofin-admins"]));
    });
    assert_eq!(link(&harness, &alice).await.status, 200);
    let completed = harness.completed().await;
    harness
        .sso
        .remove_oidc_link(id(&alice), id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");

    assert_eq!(
        swap(&harness, &completed, "browser-late")
            .await
            .outcome
            .map(|session| session.user.name),
        Err(OidcExchangeRefusal {
            status: 400,
            message: "Sign-in failed. Try again.".to_owned()
        })
    );
    let after = reread(&harness, &alice).await;
    let policy = harness
        .users
        .get_user_dto(&after, None)
        .await
        .expect("a DTO")
        .policy
        .expect("a policy");
    assert!(!policy.is_administrator, "no role was synced");
}

/// A link change whose request is still waiting answers its failure, which
/// the request's error boundary logs; once the request is dropped, the task
/// logs a failure itself (here a sign-out that failed), but not a link that
/// was not there to remove. A panic is answered as a failure.
#[tokio::test]
async fn a_link_change_logs_its_own_failure_once_its_request_is_gone() {
    let logs = crate::tests::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish(),
    );
    let user_id = Uuid::from_u128(0x1936);
    let answered = in_own_task("oidc_unlink", user_id, async {
        Err::<(), _>(ServiceError::backend("the store is gone"))
    })
    .await;
    assert!(
        matches!(answered, Err(ServiceError::Backend(_))),
        "{answered:?}"
    );
    let panicked = in_own_task("oidc_unlink", user_id, async move {
        if user_id.is_nil() {
            return Ok(());
        }
        panic!("the task panics");
    })
    .await;
    assert!(
        matches!(panicked, Err(ServiceError::Backend(_))),
        "{panicked:?}"
    );
    assert!(!logs.text().contains("after its request was dropped"));

    for error in [
        ServiceError::not_found("no such OIDC link"),
        ServiceError::backend("the sign-out failed"),
    ] {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (finished, done) = tokio::sync::oneshot::channel::<()>();
        let request = in_own_task("oidc_unlink", user_id, async move {
            drop(released.await);
            finished.send(()).expect("the test waits");
            Err::<(), _>(error)
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), request)
                .await
                .is_err(),
            "the request is dropped"
        );
        release.send(()).expect("the task waits");
        done.await.expect("the task finishes");
        tokio::task::yield_now().await;
    }
    let text = logs.text();
    assert!(text.contains("the sign-out failed"), "{text}");
    assert!(text.contains("oidc_unlink"), "{text}");
    assert!(!text.contains("no such OIDC link"), "{text}");
}
