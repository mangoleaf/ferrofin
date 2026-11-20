//! The required role (B8) and role sync (B9, B10, D6), through the sign-in
//! harness and the fake identity provider, and the role claim's reading.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::sso::OidcFolderRole;
use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek, UserPolicy};
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

/// The roles the access mappings name besides [`USERS`], and two library
/// ids. The harness has no library at all, so both are ids no library has:
/// a sign-in goes on with them, and they grant nothing.
const ALL: &str = "ferrofin-all";
const KIDS: &str = "ferrofin-kids";
const TV: &str = "ferrofin-tv";
const DVR: &str = "ferrofin-dvr";
const MOVIES: Uuid = Uuid::from_u128(0x6d6f_7669_6573_0000_0000_0000_0000_0001);
const SHOWS: Uuid = Uuid::from_u128(0x7368_6f77_7300_0000_0000_0000_0000_0002);

/// Role sync on, with [`ALL`] giving every library, [`USERS`] the movies,
/// [`KIDS`] the shows and the movies, [`TV`] Live TV and [`DVR`] its
/// recordings.
fn map_access(provider: &mut OidcProvider) {
    provider.role_sync = true;
    provider.all_folders_roles = vec![ALL.to_owned()];
    provider.folder_roles = vec![
        OidcFolderRole {
            role: USERS.to_owned(),
            folders: vec![MOVIES],
        },
        OidcFolderRole {
            role: KIDS.to_owned(),
            folders: vec![SHOWS, MOVIES],
        },
    ];
    provider.live_tv_roles = vec![TV.to_owned()];
    provider.live_tv_management_roles = vec![DVR.to_owned()];
}

/// The four members the mappings set: `EnableAllFolders`, `EnabledFolders`,
/// `EnableLiveTvAccess`, `EnableLiveTvManagement`.
fn access(policy: &UserPolicy) -> (bool, Vec<Uuid>, bool, bool) {
    (
        policy.enable_all_folders,
        policy.enabled_folders.clone(),
        policy.enable_live_tv_access,
        policy.enable_live_tv_management,
    )
}

/// Counts the user manager's user-updated announcements, one per policy
/// save. Each is consumed as the server's push consumes it, by reading the
/// account back through the manager.
fn announcements(harness: &Harness) -> Arc<AtomicUsize> {
    let count = Arc::new(AtomicUsize::new(0));
    let (reader, seen) = (Arc::clone(&harness.users), Arc::clone(&count));
    harness.events.subscribe(
        ferrofin_core::user_updated::USER_UPDATED_EVENT,
        Arc::new(move |payload: &str| {
            let id: Uuid = serde_json::from_str(payload).expect("a user id");
            let (reader, seen) = (Arc::clone(&reader), Arc::clone(&seen));
            Box::pin(async move {
                let user = reader.get_user_by_id(id).await?.expect("the user");
                reader.get_user_dto(&user, None).await?;
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }),
    );
    count
}

/// B10: with role sync on, a user's libraries and both Live TV permissions
/// are what the mappings give their roles at each sign-in, in the first
/// session already, and a role taken away takes its access with it. A user
/// with no mapped role ends with no library and no Live TV. Every mapped id
/// is one no library has, as after a library is deleted: the sign-in goes
/// on, and the ids are kept as mapped, where they match nothing.
#[tokio::test]
async fn with_role_sync_libraries_and_live_tv_follow_the_mapped_roles() {
    let harness = Harness::with(Some(PUBLIC_URL), map_access).await;
    for (held, expected) in [
        (json!([USERS]), (false, vec![MOVIES], false, false)),
        (
            json!([USERS, KIDS, TV]),
            (false, vec![MOVIES, SHOWS], true, false),
        ),
        (json!([ALL, DVR]), (true, vec![], false, true)),
        (json!(KIDS), (false, vec![SHOWS, MOVIES], false, false)),
        (json!(["another-group"]), (false, vec![], false, false)),
        (json!(null), (false, vec![], false, false)),
    ] {
        groups(&harness, held.clone());
        let session = harness.session("member-device").await.expect("signed in");
        let first = session.user.policy.expect("a policy");
        assert_eq!(access(&first), expected, "{held}: the session's user");
        let user = account(&harness).await.expect("the account");
        assert_eq!(access(&policy(&harness, &user).await), expected, "{held}");
    }
}

/// Only the four members change, and a sign-in that changes none of them
/// saves nothing, so announces nothing; the same libraries in another order
/// are no change. The announcement's consumer reads the account back, and
/// the sign-in is not held up by it: nothing on the way holds a lock the
/// save takes.
#[tokio::test]
async fn role_sync_changes_only_the_mapped_members_and_saves_only_a_change() {
    let harness = Harness::with(Some(PUBLIC_URL), map_access).await;
    let announced = announcements(&harness);
    groups(&harness, json!([USERS]));
    harness.session("member-device").await.expect("signed in");
    let user = account(&harness).await.expect("the account");
    assert_eq!(
        announced.load(Ordering::SeqCst),
        1,
        "the new account's defaults narrowed"
    );

    // Every other kind of member, set by hand as the dashboard does.
    let mut edited = policy(&harness, &user).await;
    edited.is_hidden = true;
    edited.enable_content_deletion = true;
    edited.enable_all_channels = false;
    edited.enabled_channels = vec![Uuid::from_u128(0xC1)];
    edited.blocked_tags = vec!["gore".to_owned()];
    edited.max_parental_rating = Some(13);
    edited.remote_client_bitrate_limit = 8_000_000;
    edited.access_schedules = vec![AccessSchedule {
        user_id: id_of(&user),
        day_of_week: DynamicDayOfWeek::Everyday,
        start_hour: 0.0,
        end_hour: 24.0,
        ..AccessSchedule::default()
    }];
    harness
        .users
        .update_policy(id_of(&user), &edited)
        .await
        .expect("saved");
    let before = policy(&harness, &user).await;
    assert_eq!(announced.load(Ordering::SeqCst), 2);

    // The same roles: nothing to change, nothing saved.
    harness.session("member-device").await.expect("signed in");
    assert_eq!(announced.load(Ordering::SeqCst), 2, "no save");
    assert_eq!(policy(&harness, &user).await, before);

    // Other roles: those four members change, nothing else.
    groups(&harness, json!([USERS, KIDS, TV, DVR]));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        harness.session("member-device"),
    )
    .await
    .expect("the sign-in does not wait on its own announcement")
    .expect("signed in");
    assert_eq!(announced.load(Ordering::SeqCst), 3);
    let mut expected = before.clone();
    expected.enabled_folders = vec![MOVIES, SHOWS];
    expected.enable_live_tv_access = true;
    expected.enable_live_tv_management = true;
    let mut after = policy(&harness, &user).await;
    // A policy save replaces the access schedules, which renumbers their
    // rows, as a save from the dashboard does: the schedule is the same.
    assert_ne!(after.access_schedules[0].id, 0);
    after.access_schedules[0].id = expected.access_schedules[0].id;
    assert_eq!(after, expected);

    // The same libraries, stored in another order: no change.
    let mut reordered = policy(&harness, &user).await;
    reordered.enabled_folders = vec![SHOWS, MOVIES];
    harness
        .users
        .update_policy(id_of(&user), &reordered)
        .await
        .expect("saved");
    assert_eq!(announced.load(Ordering::SeqCst), 4);
    harness.session("member-device").await.expect("signed in");
    assert_eq!(announced.load(Ordering::SeqCst), 4, "no save");
    assert_eq!(
        policy(&harness, &user).await.enabled_folders,
        [SHOWS, MOVIES]
    );
}

/// B10, D6: with role sync off the mappings change nothing. A new account
/// keeps the policy its creation gives it, and an existing account's policy,
/// set by hand, is byte for byte what it was, with nothing saved.
#[tokio::test]
async fn with_role_sync_off_the_mappings_leave_the_policy_alone() {
    let harness = Harness::with(Some(PUBLIC_URL), |provider| {
        map_access(provider);
        provider.role_sync = false;
    })
    .await;
    let announced = announcements(&harness);
    groups(&harness, json!([USERS]));
    harness.session("first-device").await.expect("signed in");
    let user = account(&harness).await.expect("the account");
    let created = policy(&harness, &user).await;
    assert_eq!(access(&created), (true, vec![], true, false), "as created");
    assert_eq!(announced.load(Ordering::SeqCst), 0);

    let mut by_hand = created;
    by_hand.enable_all_folders = false;
    by_hand.enabled_folders = vec![SHOWS];
    by_hand.enable_live_tv_access = false;
    harness
        .users
        .update_policy(id_of(&user), &by_hand)
        .await
        .expect("saved");
    let before = serde_json::to_vec(&policy(&harness, &user).await).expect("serializes");
    groups(&harness, json!([ALL, KIDS, TV, DVR]));
    harness.session("first-device").await.expect("signed in");
    let after = serde_json::to_vec(&policy(&harness, &user).await).expect("serializes");
    assert_eq!(String::from_utf8(after), String::from_utf8(before));
    assert_eq!(announced.load(Ordering::SeqCst), 1, "the hand edit only");
}

/// The access mappings are trimmed on save, an empty role is dropped from a
/// role list and refused in a library mapping, and the nested lists come
/// back from the plugin configuration as they were saved, through the
/// provider routes and the generic configuration route alike.
#[tokio::test]
async fn the_access_mappings_are_trimmed_checked_and_kept_on_save() {
    let harness = Harness::new().await;
    let saved = harness
        .sso
        .save_oidc_provider(
            "fake",
            OidcProvider {
                all_folders_roles: vec![" all ".to_owned(), "  ".to_owned()],
                folder_roles: vec![OidcFolderRole {
                    role: " kids ".to_owned(),
                    folders: vec![SHOWS, MOVIES],
                }],
                live_tv_roles: vec!["tv".to_owned(), String::new()],
                live_tv_management_roles: vec!["\tdvr".to_owned()],
                ..harness.provider("fake").await
            },
        )
        .await
        .expect("saved")
        .provider;
    assert_eq!(saved.all_folders_roles, ["all"]);
    assert_eq!(saved.folder_roles[0].role, "kids");
    assert_eq!(saved.live_tv_roles, ["tv"]);
    assert_eq!(saved.live_tv_management_roles, ["dvr"]);
    let stored = harness.provider("fake").await;
    assert_eq!(stored.all_folders_roles, saved.all_folders_roles);
    assert_eq!(stored.folder_roles, saved.folder_roles);
    assert_eq!(stored.live_tv_roles, saved.live_tv_roles);
    assert_eq!(stored.live_tv_management_roles, ["dvr"]);

    for (folder_role, live_tv_role) in [(" ", "tv"), ("kids\u{7}", "tv"), ("kids", "tv\u{7}")] {
        let answer = harness
            .sso
            .save_oidc_provider(
                "fake",
                OidcProvider {
                    folder_roles: vec![OidcFolderRole {
                        role: folder_role.to_owned(),
                        folders: vec![SHOWS],
                    }],
                    live_tv_roles: vec![live_tv_role.to_owned()],
                    ..harness.provider("fake").await
                },
            )
            .await;
        match answer {
            Err(ServiceError::InvalidInput(why)) => {
                assert!(why.contains("library and Live TV mappings"), "{why}");
            }
            other => panic!("{folder_role:?} {live_tv_role:?}: {other:?}"),
        }
    }

    // The configuration route projects a post onto the defaults, whose
    // `Providers` list is empty: each provider is kept as posted, its nested
    // lists included.
    let mut config = harness.sso.config().await.expect("config");
    config.providers[0]["FolderRoles"][0]["Folders"]
        .as_array_mut()
        .expect("a list")
        .push(json!("0D0A6E1C-2F3B-4A5C-9D8E-7F6A5B4C3D2E"));
    harness
        .sso
        .plugins
        .set_plugin_configuration(
            crate::sso::EXTENSION_ID,
            serde_json::to_vec(&config).expect("serializes"),
        )
        .await
        .expect("saved");
    assert_eq!(
        harness.provider("fake").await.folder_roles[0].folders,
        [
            SHOWS,
            MOVIES,
            Uuid::from_u128(0x0d0a_6e1c_2f3b_4a5c_9d8e_7f6a_5b4c_3d2e)
        ]
    );
}

/// The settings page reads and sends each access mapping, lists the
/// libraries from the route the dashboard's user access page reads, and
/// shows a mapped library that is gone as missing.
#[test]
fn the_settings_page_edits_the_access_mappings() {
    let page = SsoExtension.config_pages().remove(0).bytes;
    let html = String::from_utf8(page).expect("UTF-8");
    for member in [
        "AllFoldersRoles",
        "FolderRoles",
        "LiveTvRoles",
        "LiveTvManagementRoles",
    ] {
        assert_eq!(
            html.matches(&format!("info.{member}")).count(),
            1,
            "{member}"
        );
        assert_eq!(html.matches(&format!(" {member}: ")).count(), 1, "{member}");
    }
    assert!(html.contains("getUrl('Library/MediaFolders', { IsHidden: false })"));
    assert!(html.contains("Missing library"), "{html}");
}
