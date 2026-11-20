//! Linking an account and an identity (B12), through the sign-in harness and
//! the fake identity provider: the link flow from its start to its callback,
//! each way a callback is refused, the admin's links, and a removal signing
//! the account out.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::secret::Secret;
use ferrofin_traits::external_identities::ExternalIdentityStore as _;
use ferrofin_traits::library::UserManager as _;
use ferrofin_traits::session::SessionManager as _;
use ferrofin_traits::sso::{OidcCallback, OidcExchangeRefusal, OidcExchangeReply};

use super::*;
use crate::sso::EXTENSION_ID;
use crate::sso::fake_idp::{SUBJECT, USERNAME};
use crate::sso::sign_in::tests::Harness;

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

/// A link flow of `user` started: where the browser is sent, and its flow
/// cookie's value.
async fn start_link(harness: &Harness, user: &UserEntity) -> (String, String) {
    let reply = harness
        .sso
        .start_oidc_link("fake", id(user), CLIENT)
        .await
        .expect("starts");
    assert_eq!(reply.status, 302, "{reply:?}");
    let cookie = reply.set_cookies[0]
        .strip_prefix("ferrofin_sso_flow=")
        .and_then(|rest| rest.split(';').next())
        .expect("the flow cookie")
        .to_owned();
    (reply.location.expect("a redirect"), cookie)
}

/// Where the fake sends the browser back to from `location`, as the callback
/// route reads it with `cookie`.
async fn callback(harness: &Harness, location: &str, cookie: Option<&str>) -> OidcCallback {
    let url = url::Url::parse(&harness.fake.authorize(location).await).expect("a URL");
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

/// A whole link flow of `user` at `fake`, through the callback: its page.
async fn link(harness: &Harness, user: &UserEntity) -> SsoReply {
    let (location, cookie) = start_link(harness, user).await;
    let callback = callback(harness, &location, Some(&cookie)).await;
    harness
        .sso
        .finish_oidc_sign_in("fake", callback)
        .await
        .expect("an answer")
}

/// The account the fake's `SUBJECT` is linked to.
async fn owner(harness: &Harness, subject: &str) -> Option<Uuid> {
    harness
        .identities
        .find(ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &harness.fake.issuer(),
            subject,
        })
        .await
        .expect("find")
        .map(|link| link.user_id)
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
        ["ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure"]
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

#[tokio::test]
async fn a_link_callback_without_its_cookie_or_replayed_links_nothing() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;

    for cookie in [None, Some("another-browser")] {
        let (location, _) = start_link(&harness, &alice).await;
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
    let (location, cookie) = start_link(&harness, &alice).await;
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
        .add_oidc_link(id(&alice), "fake", SUBJECT)
        .await
        .expect_err("taken");
    assert!(matches!(refused, ServiceError::Conflict(_)), "{refused:?}");
    assert_eq!(
        owner(&harness, SUBJECT).await,
        Some(id(&bob)),
        "nothing moved"
    );

    // Linking the identity its account already has keeps that link.
    assert_eq!(link(&harness, &bob).await.status, 200);
    assert_eq!(owner(&harness, SUBJECT).await, Some(id(&bob)));
}

#[tokio::test]
async fn a_second_identity_at_the_same_issuer_replaces_the_first() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    assert_eq!(link(&harness, &alice).await.status, 200);
    harness
        .fake
        .sign_in(|s| s.subject = "alice-second".to_owned());
    assert_eq!(link(&harness, &alice).await.status, 200);

    assert_eq!(owner(&harness, SUBJECT).await, None);
    assert_eq!(owner(&harness, "alice-second").await, Some(id(&alice)));
    let links = harness.sso.oidc_links(id(&alice)).await.expect("links");
    assert_eq!(links.links.len(), 1);
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
            name: "Fake <IdP>".to_owned()
        }]
    );
    assert_eq!(link(&harness, &alice).await.status, 200);

    let links = harness.sso.oidc_links(id(&alice)).await.expect("links");
    assert_eq!(links.links.len(), 1);
    assert_eq!(links.links[0].provider.as_deref(), Some("fake"));
    assert_eq!(links.links[0].provider_name.as_deref(), Some("Fake <IdP>"));
    assert_eq!(links.links[0].issuer, harness.fake.issuer());
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
        .remove_oidc_link(id(&alice), OidcLinkAt::Provider("fake"))
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
        .remove_oidc_link(id(&alice), OidcLinkAt::Provider("fake"))
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
        .add_oidc_link(id(&alice), "fake", "alice-by-hand")
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
            .add_oidc_link(id(&alice), "fake", subject)
            .await
            .expect_err(why);
        assert!(matches!(refused, ServiceError::InvalidInput(_)), "{why}");
    }
    let long = "s".repeat(256);
    assert!(matches!(
        harness.sso.add_oidc_link(id(&alice), "fake", &long).await,
        Err(ServiceError::InvalidInput(_))
    ));
    assert!(
        harness
            .sso
            .add_oidc_link(id(&alice), "fake", &long[..255])
            .await
            .is_ok()
    );
    assert!(matches!(
        harness.sso.add_oidc_link(Uuid::nil(), "fake", "x").await,
        Err(ServiceError::InvalidInput(_))
    ));
    for (user, key) in [(Uuid::from_u128(0xDEAD), "fake"), (id(&alice), "nope")] {
        assert!(
            matches!(
                harness.sso.add_oidc_link(user, key, "x").await,
                Err(ServiceError::NotFound(_))
            ),
            "{user} {key}"
        );
    }

    // Removed by the link's issuer, which no provider may still have.
    let issuer = harness.fake.issuer();
    harness
        .sso
        .remove_oidc_link(id(&alice), OidcLinkAt::Issuer(&issuer))
        .await
        .expect("removed");
    assert_eq!(harness.sso.all_oidc_links().await.expect("all"), []);
    for at in [
        OidcLinkAt::Issuer("https://other.example"),
        OidcLinkAt::Provider("nope"),
    ] {
        assert!(
            matches!(
                harness.sso.remove_oidc_link(id(&alice), at).await,
                Err(ServiceError::NotFound(_))
            ),
            "{at:?}"
        );
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
}

/// B1: while the plugin is disabled, no link route does anything.
#[tokio::test]
async fn a_disabled_plugin_has_no_links() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
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
    not_found(sso.start_oidc_link("fake", user, CLIENT).await.map(drop));
    not_found(sso.all_oidc_links().await.map(drop));
    not_found(sso.add_oidc_link(user, "fake", "x").await.map(drop));
    not_found(
        sso.remove_oidc_link(user, OidcLinkAt::Provider("fake"))
            .await,
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
}

/// A link flow whose account was deleted before its callback, or whose
/// identity has an empty subject, links nothing.
#[tokio::test]
async fn a_link_to_a_deleted_account_or_an_empty_subject_links_nothing() {
    let harness = Harness::new().await;
    let alice = local(&harness, "alice").await;
    let bob = local(&harness, "bob").await;
    let (location, cookie) = start_link(&harness, &alice).await;
    harness
        .users
        .delete_user(id(&alice))
        .await
        .expect("deleted");
    let back = callback(&harness, &location, Some(&cookie)).await;
    let page = harness
        .sso
        .finish_oidc_sign_in("fake", back)
        .await
        .expect("an answer");
    assert_eq!(page.status, 400, "{}", page.html);
    assert!(
        page.html.contains("Linking failed. Try again."),
        "{}",
        page.html
    );
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
    let page = harness
        .sso
        .link_verified(&provider, id(&bob), empty)
        .await
        .expect("an answer");
    assert_eq!(page.status, 400);
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
        .exchange(
            &code,
            Some(cookie),
            crate::sso::sign_in::tests::device(device_id),
        )
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
        .remove_oidc_link(id(&alice), OidcLinkAt::Provider("fake"))
        .await
        .expect("removed");
    // Unlinked: no link names the identity.
    refused(swap(&harness, &first, "browser-late-1").await);
    // Linked to bob since: the code was alice's.
    harness
        .sso
        .add_oidc_link(id(&bob), "fake", SUBJECT)
        .await
        .expect("bob linked");
    refused(swap(&harness, &second, "browser-late-2").await);
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
            .remove_oidc_link(id(&alice), OidcLinkAt::Provider("fake"))
            .await,
        Err(ServiceError::NotFound(_))
    ));
    let issuer = harness.fake.issuer();
    harness
        .sso
        .remove_oidc_link(id(&alice), OidcLinkAt::Issuer(&issuer))
        .await
        .expect("removed");
    assert_eq!(harness.sso.all_oidc_links().await.expect("all"), []);
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
        let removing = harness
            .sso
            .remove_oidc_link(id(&alice), OidcLinkAt::Provider("fake"));
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
