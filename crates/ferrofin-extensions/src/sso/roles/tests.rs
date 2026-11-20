//! The required role (B8) and role sync (B9, D6), through the sign-in harness
//! and the fake identity provider, and the role claim's reading.

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::external_identities::ExternalIdentityStore as _;
use ferrofin_traits::library::UserManager as _;
use ferrofin_traits::session::SessionManager as _;
use ferrofin_traits::sso::SsoManager as _;
use serde_json::{Map, json};

use super::*;
use crate::Extension as _;
use crate::sso::SsoExtension;
use crate::sso::accounts::PROVIDER_ID;
use crate::sso::fake_idp::{SUBJECT, USERINFO_PATH, USERNAME};
use crate::sso::sign_in::tests::{Harness, PUBLIC_URL};

/// The role every test requires or maps to the administrator flag.
const USERS: &str = "ferrofin-users";
const ADMINS: &str = "ferrofin-admins";

/// The fake's identity with `claims`, as a callback verifies it.
fn identity(claims: serde_json::Value) -> VerifiedIdentity {
    let serde_json::Value::Object(claims) = claims else {
        panic!("claims are an object");
    };
    VerifiedIdentity {
        provider_key: "fake".to_owned(),
        issuer: "https://id.example.org".to_owned(),
        subject: SUBJECT.to_owned(),
        username: Some(USERNAME.to_owned()),
        claims,
    }
}

/// The fake's next sign-ins carry `groups` (`null` removes the claim).
fn groups(harness: &Harness, groups: serde_json::Value) {
    harness.fake.sign_in(|sign_in| {
        sign_in.claims.insert("groups".to_owned(), groups);
    });
}

/// The account the fake's identity signs in to, if one was made.
async fn account(harness: &Harness) -> Option<UserEntity> {
    harness
        .users
        .get_user_by_name(USERNAME)
        .await
        .expect("lookup")
}

fn id_of(user: &UserEntity) -> Uuid {
    Uuid::parse_str(&user.id).expect("a user id")
}

/// `user`'s policy, as stored.
async fn policy(harness: &Harness, user: &UserEntity) -> UserPolicy {
    harness
        .users
        .get_user_dto(user, None)
        .await
        .expect("a DTO")
        .policy
        .expect("a policy")
}

/// Whether the account `user` is now an administrator.
async fn is_administrator(harness: &Harness, user: &UserEntity) -> bool {
    let user = harness
        .users
        .get_user_by_id(id_of(user))
        .await
        .expect("lookup")
        .expect("the account");
    policy(harness, &user).await.is_administrator
}

/// Sets `user`'s administrator flag through its policy, as the dashboard
/// does.
async fn set_administrator(harness: &Harness, user: &UserEntity, administrator: bool) {
    let mut policy = policy(harness, user).await;
    policy.is_administrator = administrator;
    harness
        .users
        .update_policy(id_of(user), &policy)
        .await
        .expect("saved");
}

/// A local administrator, so the fake's account is never the last one.
async fn another_administrator(harness: &Harness) -> UserEntity {
    let keeper = harness.users.create_user("keeper").await.expect("created");
    set_administrator(harness, &keeper, true).await;
    keeper
}

async fn session_count(harness: &Harness) -> usize {
    harness
        .sessions
        .get_session_summaries()
        .await
        .expect("sessions")
        .len()
}

#[rstest::rstest]
#[case::array(json!({"groups": [USERS, ADMINS]}), &[USERS, ADMINS])]
#[case::one_string(json!({"groups": USERS}), &[USERS])]
#[case::non_strings_skipped(json!({"groups": [USERS, 7, null, {"name": ADMINS}]}), &[USERS])]
#[case::missing(json!({}), &[])]
#[case::empty(json!({"groups": []}), &[])]
#[case::a_number(json!({"groups": 7}), &[])]
#[case::an_object(json!({"groups": {"ferrofin-users": true}}), &[])]
fn the_role_claim_is_an_array_of_strings_or_one_string(
    #[case] claims: serde_json::Value,
    #[case] expected: &[&str],
) {
    let provider = OidcProvider::default();
    assert_eq!(roles(&provider, &identity(claims)), expected);
}

/// The claim is named as a top-level member: a nested one is never walked
/// into, and names are compared exactly.
#[test]
fn a_nested_claim_is_not_read_and_names_are_exact() {
    let nested = OidcProvider {
        role_claim: "realm_access.roles".to_owned(),
        ..OidcProvider::default()
    };
    let token = identity(json!({"realm_access": {"roles": [USERS]}}));
    assert_eq!(roles(&nested, &token), Vec::<&str>::new());

    let required = OidcProvider {
        required_role: USERS.to_owned(),
        ..OidcProvider::default()
    };
    for other in ["Ferrofin-Users", "ferrofin-users ", "/ferrofin-users"] {
        assert_eq!(
            check_required_role(&required, &identity(json!({"groups": [other]}))),
            Err(MISSING_ROLE),
            "{other}"
        );
    }
    assert_eq!(
        check_required_role(&required, &identity(json!({"groups": [USERS]}))),
        Ok(())
    );
}

/// B8, with role sync off or on: a user without the role is refused with a
/// page saying so, and no account is made or linked; with it they sign in.
#[rstest::rstest]
#[tokio::test]
async fn without_the_required_role_no_account_is_made(#[values(false, true)] role_sync: bool) {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        USERS.clone_into(&mut provider.required_role);
        provider.role_sync = role_sync;
    })
    .await;
    for refused in [
        json!(["another-group"]),
        json!([]),
        json!(null),
        json!("ferrofin"),
    ] {
        groups(&harness, refused.clone());
        let reply = harness.finish().await;
        assert_eq!(reply.status, 403, "{refused}: {reply:?}");
        assert!(
            reply
                .html
                .contains("does not have the role this server requires"),
            "{}",
            reply.html
        );
        assert!(!reply.html.contains(USERS), "no role on the page");
        assert!(!reply.html.contains("data-code"), "no exchange code");
        assert_eq!(account(&harness).await, None, "{refused}");
        assert_eq!(
            harness.identities.list(PROVIDER_ID).await.expect("list"),
            Vec::new()
        );
    }

    groups(&harness, json!([USERS]));
    let session = harness.session("member-device").await.expect("signed in");
    assert_eq!(session.user.name.as_deref(), Some(USERNAME));
}

/// B8: an account already linked to the identity gets no session once the
/// role is gone, and is left as it was.
#[tokio::test]
async fn a_linked_account_without_the_required_role_gets_no_session() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        USERS.clone_into(&mut provider.required_role);
    })
    .await;
    groups(&harness, json!([USERS]));
    harness.session("first-device").await.expect("signed in");
    let before = account(&harness).await.expect("the account");
    let sessions = session_count(&harness).await;

    groups(&harness, json!(["another-group"]));
    let reply = harness.finish().await;
    assert_eq!(reply.status, 403, "{reply:?}");
    assert!(!reply.html.contains("data-code"), "no exchange code");
    assert_eq!(session_count(&harness).await, sessions);
    assert_eq!(account(&harness).await, Some(before));
}

/// The ID token may leave the role claim out (Keycloak does for a user in no
/// group, and some providers put roles in userinfo only): userinfo is then
/// asked, and its answer read as an array or one string. A provider that
/// reads no roles never asks for them.
#[tokio::test]
async fn a_role_claim_the_id_token_lacks_is_read_from_userinfo() {
    let userinfo_calls = |harness: &Harness| {
        harness
            .fake
            .requests()
            .iter()
            .filter(|request| request.path == harness.fake.path(USERINFO_PATH))
            .count()
    };
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        USERS.clone_into(&mut provider.required_role);
    })
    .await;
    for userinfo in [json!(USERS), json!(["other", USERS])] {
        harness.fake.sign_in(|sign_in| {
            sign_in.claims.insert("groups".to_owned(), json!(null));
            sign_in.userinfo = Map::from_iter([("groups".to_owned(), userinfo.clone())]);
        });
        assert_eq!(harness.finish().await.status, 200, "{userinfo}");
    }
    assert_eq!(userinfo_calls(&harness), 2);
    // Neither holds it: refused, after asking.
    harness
        .fake
        .sign_in(|sign_in| sign_in.userinfo = Map::new());
    assert_eq!(harness.finish().await.status, 403);
    assert_eq!(userinfo_calls(&harness), 3);
    // In the ID token: no call.
    groups(&harness, json!([USERS]));
    assert_eq!(harness.finish().await.status, 200);
    assert_eq!(userinfo_calls(&harness), 3);

    let plain = Harness::new().await;
    groups(&plain, json!(null));
    assert_eq!(plain.finish().await.status, 200);
    assert_eq!(userinfo_calls(&plain), 0, "no role setting, no userinfo");
}

/// B9: with role sync on, the administrator role makes the account an
/// administrator before its session is minted, so that first session
/// already has the rights, and losing the role takes the flag away.
#[tokio::test]
async fn with_role_sync_the_administrator_flag_follows_the_administrator_role() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.role_sync = true;
        ADMINS.clone_into(&mut provider.admin_role);
    })
    .await;
    another_administrator(&harness).await;

    groups(&harness, json!([USERS, ADMINS]));
    let session = harness.session("admin-device").await.expect("signed in");
    let promoted = session.user.policy.expect("a policy");
    assert!(promoted.is_administrator, "the first session's user");
    let user = account(&harness).await.expect("the account");
    assert!(is_administrator(&harness, &user).await);

    groups(&harness, json!([USERS]));
    let session = harness.session("member-device").await.expect("signed in");
    assert!(!session.user.policy.expect("a policy").is_administrator);
    assert!(!is_administrator(&harness, &user).await);

    // The flag follows one string too.
    groups(&harness, json!(ADMINS));
    harness.session("admin-device").await.expect("signed in");
    assert!(is_administrator(&harness, &user).await);
}

/// B9: the last administrator keeps the flag when the role is gone, still
/// signs in, and the server warns.
#[tokio::test]
async fn the_last_administrator_keeps_the_flag_and_signs_in() {
    let logs = crate::tests::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_env_filter("ferrofin_core=warn")
            .finish(),
    );
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.role_sync = true;
        ADMINS.clone_into(&mut provider.admin_role);
    })
    .await;
    groups(&harness, json!([ADMINS]));
    harness.session("admin-device").await.expect("signed in");
    let user = account(&harness).await.expect("the account");
    assert!(is_administrator(&harness, &user).await, "the only one");

    groups(&harness, json!(null));
    let session = harness.session("admin-device").await.expect("signed in");
    assert!(session.user.policy.expect("a policy").is_administrator);
    assert!(is_administrator(&harness, &user).await);
    assert!(
        logs.text()
            .contains("sign-in would demote the last administrator"),
        "{}",
        logs.text()
    );
}

/// Role sync comes before the account rules, as a password sign-in applies
/// its provider's flag before them: a demotion lands even when a rule then
/// refuses the session.
#[tokio::test]
async fn role_sync_applies_before_the_account_rules() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.role_sync = true;
        ADMINS.clone_into(&mut provider.admin_role);
    })
    .await;
    another_administrator(&harness).await;
    groups(&harness, json!([ADMINS]));
    harness.session("admin-device").await.expect("signed in");
    let user = account(&harness).await.expect("the account");
    let mut disabled = policy(&harness, &user).await;
    disabled.is_disabled = true;
    harness
        .users
        .update_policy(id_of(&user), &disabled)
        .await
        .expect("saved");

    groups(&harness, json!([USERS]));
    let refusal = harness
        .session("admin-device")
        .await
        .expect_err("a disabled account");
    assert_eq!(refusal.status, 403, "{}", refusal.message);
    assert!(
        !is_administrator(&harness, &user).await,
        "demoted all the same"
    );
}

/// D6: with role sync off, the administrator role changes nothing, for a
/// new account and an existing one alike.
#[tokio::test]
async fn with_role_sync_off_the_administrator_flag_never_changes() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        ADMINS.clone_into(&mut provider.admin_role);
    })
    .await;
    another_administrator(&harness).await;
    groups(&harness, json!([ADMINS]));
    let session = harness.session("first-device").await.expect("signed in");
    assert!(!session.user.policy.expect("a policy").is_administrator);
    let user = account(&harness).await.expect("the account");
    assert!(!is_administrator(&harness, &user).await);

    set_administrator(&harness, &user, true).await;
    groups(&harness, json!([USERS]));
    harness.session("first-device").await.expect("signed in");
    assert!(is_administrator(&harness, &user).await, "left alone");
}

/// Role sync on with no administrator role leaves the flag alone.
#[tokio::test]
async fn role_sync_without_an_administrator_role_leaves_the_flag_alone() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        provider.role_sync = true;
    })
    .await;
    another_administrator(&harness).await;
    groups(&harness, json!([ADMINS]));
    harness.session("first-device").await.expect("signed in");
    let user = account(&harness).await.expect("the account");
    assert!(!is_administrator(&harness, &user).await);
    set_administrator(&harness, &user, true).await;
    groups(&harness, json!(null));
    harness.session("first-device").await.expect("signed in");
    assert!(is_administrator(&harness, &user).await);
}

/// The role settings are trimmed on save like every text setting, and a
/// role setting in use needs a role claim.
#[tokio::test]
async fn role_settings_are_trimmed_and_need_a_role_claim_while_in_use() {
    let harness = Harness::new().await;
    let saved = harness
        .sso
        .save_oidc_provider(
            "fake",
            OidcProvider {
                role_claim: " roles ".to_owned(),
                required_role: " users ".to_owned(),
                admin_role: " admins ".to_owned(),
                ..harness.provider("fake").await
            },
        )
        .await
        .expect("saved");
    assert_eq!(saved.provider.role_claim, "roles");
    assert_eq!(saved.provider.required_role, "users");
    assert_eq!(saved.provider.admin_role, "admins");

    // The role claim may be empty only while no role setting is in use.
    for (required_role, role_sync, refused) in
        [(USERS, false, true), ("", true, true), ("", false, false)]
    {
        let answer = harness
            .sso
            .save_oidc_provider(
                "fake",
                OidcProvider {
                    role_claim: String::new(),
                    required_role: required_role.to_owned(),
                    role_sync,
                    ..harness.provider("fake").await
                },
            )
            .await;
        match answer {
            Err(ServiceError::InvalidInput(why)) if refused => {
                assert!(why.contains("the role claim is required"), "{why}");
            }
            Ok(_) if !refused => {}
            other => panic!("{required_role:?} {role_sync}: {other:?}"),
        }
    }
}

/// The settings page reads and sends each role setting.
#[test]
fn the_settings_page_edits_the_role_settings() {
    let page = SsoExtension.config_pages().remove(0).bytes;
    let html = String::from_utf8(page).expect("UTF-8");
    for member in ["RoleClaim", "RequiredRole", "RoleSync", "AdminRole"] {
        assert_eq!(
            html.matches(&format!("info.{member}")).count(),
            1,
            "{member}"
        );
        assert_eq!(
            html.matches(&format!("{member}: field(")).count(),
            1,
            "{member}"
        );
    }
    assert!(
        html.contains("var DEFAULT_ROLE_CLAIM = 'groups';"),
        "{html}"
    );
}
