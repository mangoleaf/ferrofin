//! Completing a sign-in (B5, B7): the completion page, and the exchange of
//! its code for a session under the account rules of a password sign-in,
//! through the sign-in harness and the fake identity provider.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrofin_model::activity::ActivityLogEntry;
use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek, UserPolicy};
use ferrofin_traits::activity::{ActivityLogQuery, ActivityManager as _};
use ferrofin_traits::library::UserManager as _;
use ferrofin_traits::session::SessionManager as _;

use super::*;
use crate::sso::fake_idp::{SUBJECT, USERNAME};
use crate::sso::sign_in::tests::{Harness, device, exchange_code};

/// What the completion page's script reads for each refusal that names no
/// rule.
const GENERIC: &str = "Sign-in failed. Try again.";

/// The flow cookie, cleared.
const CLEARED: &str = "ferrofin_sso_flow=; Path=/jf/sso; Max-Age=0; HttpOnly; SameSite=Lax; Secure";

/// The account the fake's identity signs in to, once one sign-in made it.
async fn account(harness: &Harness) -> UserEntity {
    harness
        .users
        .get_user_by_name(USERNAME)
        .await
        .expect("lookup")
        .expect("the account")
}

/// Saves `edit`ed policy of the fake's account, made by a first sign-in.
async fn with_policy(harness: &Harness, edit: impl FnOnce(&mut UserPolicy)) -> UserEntity {
    harness.session("first-device").await.expect("signed in");
    let user = account(harness).await;
    let mut policy = harness
        .users
        .get_user_dto(&user, None)
        .await
        .expect("a DTO")
        .policy
        .expect("a policy");
    edit(&mut policy);
    harness
        .users
        .update_policy(Uuid::parse_str(&user.id).expect("an id"), &policy)
        .await
        .expect("saved");
    account(harness).await
}

/// The activity log, newest first.
async fn activity(harness: &Harness) -> Vec<ActivityLogEntry> {
    harness
        .activity
        .get_paged_result(&ActivityLogQuery::default())
        .await
        .expect("the log")
        .items
}

/// How many sessions are open.
async fn session_count(harness: &Harness) -> usize {
    harness
        .sessions
        .get_session_summaries()
        .await
        .expect("sessions")
        .len()
}

#[tokio::test]
async fn the_completion_page_holds_a_code_bound_to_the_cookie_and_may_run_its_script_only() {
    let harness = Harness::new().await;
    let (page, cookie) = harness.completed().await;
    assert_eq!(page.status, 200, "{}", page.html);
    assert_eq!(page.location, None);
    let code = exchange_code(&page.html);
    assert!(code.len() >= 43, "32 random bytes: {code}");
    assert_ne!(code, cookie);
    assert!(
        page.html
            .contains(&format!("<script>{COMPLETE_SCRIPT}</script>")),
        "{}",
        page.html
    );
    assert!(page.html.contains(&format!(
        "data-version=\"{}\"",
        ferrofin_health::RELEASE_VERSION
    )));
    assert!(!page.html.contains(SUBJECT), "no claim: {}", page.html);
    assert!(!page.html.contains(USERNAME), "no claim: {}", page.html);
    // The cookie lives as long as the code; the exchange clears it.
    assert_eq!(
        page.set_cookies,
        [format!(
            "ferrofin_sso_flow={cookie}; Path=/jf/sso; Max-Age=60; HttpOnly; SameSite=Lax; Secure"
        )]
    );
    let hash = STANDARD.encode(Sha256::digest(COMPLETE_SCRIPT));
    assert_eq!(
        page.content_security_policy.as_deref(),
        Some(
            format!(
                "default-src 'none'; script-src 'sha256-{hash}'; connect-src 'self'; \
                 base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
            )
            .as_str()
        )
    );
    // Nobody is signed in before the exchange.
    assert_eq!(harness.sso.exchanges.len(), 1);
    assert_eq!(session_count(&harness).await, 0);
}

#[tokio::test]
async fn the_code_swaps_once_for_a_session_of_the_account_and_a_success_event() {
    let harness = Harness::new().await;
    let succeeded = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&succeeded);
    harness.events.subscribe(
        "AuthenticationSucceeded",
        Arc::new(move |payload: &str| {
            seen.lock().expect("events").push(payload.to_owned());
            ferrofin_core::event_manager::consumer_done()
        }),
    );
    let (page, cookie) = harness.completed().await;
    let code = exchange_code(&page.html);
    let reply = harness
        .exchange(&code, Some(&cookie), device("browser-1"))
        .await;
    assert_eq!(reply.set_cookies, [CLEARED]);
    let session = reply.outcome.expect("a session");
    let user = account(&harness).await;
    assert_eq!(session.user.name.as_deref(), Some(USERNAME));
    assert_eq!(session.session.device_id.as_deref(), Some("browser-1"));
    assert_eq!(session.session.client.as_deref(), Some("Jellyfin Web"));
    assert_eq!(
        session.session.server_id.as_deref(),
        Some("sso-test-server")
    );
    // The token is the session's.
    let found = harness
        .sessions
        .get_session_by_authentication_token(
            session.access_token.expose(),
            "browser-1",
            "192.0.2.7",
        )
        .await
        .expect("the token opens the session");
    assert_eq!(found.user_name.as_deref(), Some(USERNAME));
    assert_eq!(
        succeeded.lock().expect("events").len(),
        1,
        "logged as a success"
    );
    // As a Quick Connect sign-in: the activity date only, and no counter.
    assert!(user.last_activity_date.is_some());
    assert_eq!(user.last_login_date, None);
    assert_eq!(user.invalid_login_attempt_count, 0);
    assert!(harness.sso.exchanges.is_empty());

    // Once.
    let again = harness
        .exchange(&code, Some(&cookie), device("browser-2"))
        .await;
    assert_eq!(
        again.outcome,
        Err(OidcExchangeRefusal {
            status: 400,
            message: GENERIC.to_owned(),
        })
    );
    assert_eq!(again.set_cookies, [CLEARED]);
    assert_eq!(session_count(&harness).await, 1);
}

#[tokio::test]
async fn a_sign_in_from_the_same_device_replaces_its_session() {
    let harness = Harness::new().await;
    let first = harness.session("browser-1").await.expect("signed in");
    let second = harness.session("browser-1").await.expect("signed in");
    assert_ne!(first.access_token, second.access_token);
    assert!(
        harness
            .sessions
            .get_session_by_authentication_token(first.access_token.expose(), "browser-1", "")
            .await
            .is_err(),
        "the earlier token is revoked"
    );
    assert_eq!(session_count(&harness).await, 1);
}

#[tokio::test]
async fn the_code_needs_its_cookie_and_is_used_up_by_any_attempt() {
    let harness = Harness::new().await;
    for (cookie, reason) in [(None, "missing"), (Some("another-browser"), "wrong")] {
        let (page, right) = harness.completed().await;
        let code = exchange_code(&page.html);
        let refused = harness.exchange(&code, cookie, device("browser-1")).await;
        assert_eq!(refused.outcome.expect_err(reason).status, 400, "{reason}");
        assert_eq!(refused.set_cookies, [CLEARED], "{reason}");
        let late = harness
            .exchange(&code, Some(&right), device("browser-1"))
            .await;
        assert!(late.outcome.is_err(), "{reason}: used up");
    }
    assert!(
        harness
            .exchange("guessed", Some("guessed"), device("browser-1"))
            .await
            .outcome
            .is_err()
    );
    assert_eq!(session_count(&harness).await, 0);
    assert!(activity(&harness).await.is_empty(), "no account is named");
}

#[tokio::test]
async fn the_code_lives_a_minute() {
    let harness = Harness::new().await;
    let (page, cookie) = harness.completed().await;
    let code = exchange_code(&page.html);
    let mut pending = harness.sso.exchanges.take(&code).expect("pending");
    pending.created = Instant::now()
        .checked_sub(EXCHANGE_CODE_LIFETIME + Duration::from_secs(1))
        .expect("the clock reaches back");
    harness.sso.exchanges.insert(code.clone(), pending);
    let late = harness
        .exchange(&code, Some(&cookie), device("browser-1"))
        .await;
    assert_eq!(late.outcome.expect_err("expired").status, 400);
    assert_eq!(session_count(&harness).await, 0);
}

#[tokio::test]
async fn a_provider_disabled_since_the_callback_opens_no_session() {
    let harness = Harness::new().await;
    let (page, cookie) = harness.completed().await;
    let mut provider = harness
        .sso
        .oidc_providers()
        .await
        .expect("listed")
        .providers
        .remove(0)
        .provider;
    provider.enabled = false;
    harness
        .sso
        .save_oidc_provider("fake", provider)
        .await
        .expect("saved");
    let refused = harness
        .exchange(
            &exchange_code(&page.html),
            Some(&cookie),
            device("browser-1"),
        )
        .await;
    assert_eq!(refused.outcome.expect_err("refused").status, 400);
    assert_eq!(session_count(&harness).await, 0);
}

/// An every-day window from `start_hour` to `end_hour`.
fn every_day(start_hour: f64, end_hour: f64) -> Vec<AccessSchedule> {
    vec![AccessSchedule {
        day_of_week: DynamicDayOfWeek::Everyday,
        start_hour,
        end_hour,
        ..AccessSchedule::default()
    }]
}

/// B7: each rule a password sign-in applies refuses the exchange with its
/// message, opens no session, writes the failed sign-in to the activity log,
/// and leaves the failed-login counter alone.
#[rstest::rstest]
#[case::disabled(
    |policy: &mut UserPolicy| policy.is_disabled = true,
    "The fake-user account is currently disabled. Please consult with your administrator."
)]
#[case::remote_without_remote_access(
    |policy: &mut UserPolicy| policy.enable_remote_access = false,
    "remote access is not permitted for this user"
)]
#[case::outside_the_parental_schedule(
    |policy: &mut UserPolicy| policy.access_schedules = every_day(24.0, 24.0),
    "User is not allowed access at this time."
)]
#[case::a_device_the_account_may_not_use(
    |policy: &mut UserPolicy| {
        policy.enable_all_devices = false;
        policy.enabled_devices = vec!["first-device".to_owned()];
    },
    "user is not allowed access from this device"
)]
#[case::the_session_limit(
    |policy: &mut UserPolicy| policy.max_active_sessions = 1,
    "user is at their maximum number of sessions"
)]
#[tokio::test]
async fn each_account_rule_refuses_with_its_message_and_an_activity_entry(
    #[case] edit: fn(&mut UserPolicy),
    #[case] message: &str,
) {
    let harness = Harness::new().await;
    let user = with_policy(&harness, edit).await;
    assert_eq!(user.username, USERNAME);
    let sessions = session_count(&harness).await;
    let entries = activity(&harness).await.len();

    let refused = harness.session("second-device").await;
    assert_eq!(
        refused,
        Err(OidcExchangeRefusal {
            status: 403,
            message: message.to_owned(),
        })
    );
    assert_eq!(session_count(&harness).await, sessions, "no session");
    let log = activity(&harness).await;
    assert_eq!(log.len(), entries + 1);
    assert_eq!(log[0].name, format!("Failed login attempt from {USERNAME}"));
    assert_eq!(log[0].type_, "AuthenticationFailed");
    assert_eq!(
        log[0].short_overview.as_deref(),
        Some("IP address: 192.0.2.7")
    );
    assert_eq!(log[0].severity, ferrofin_model::activity::LogLevel::Error);
    assert_eq!(account(&harness).await.invalid_login_attempt_count, 0);
}

#[tokio::test]
async fn an_account_without_remote_access_signs_in_from_the_local_network() {
    let harness = Harness::new().await;
    with_policy(&harness, |policy| policy.enable_remote_access = false).await;
    let (page, cookie) = harness.completed().await;
    let local = AuthenticationRequest {
        remote_endpoint: Some("127.0.0.1".to_owned()),
        ..device("local-device")
    };
    let session = harness
        .exchange(&exchange_code(&page.html), Some(&cookie), local)
        .await
        .outcome
        .expect("signed in");
    assert_eq!(session.user.name.as_deref(), Some(USERNAME));
}

#[tokio::test]
async fn an_exchange_without_its_device_or_of_a_deleted_account_opens_no_session() {
    let harness = Harness::new().await;
    let (page, cookie) = harness.completed().await;
    let no_device = AuthenticationRequest {
        device_id: None,
        ..device("browser-1")
    };
    let refused = harness
        .exchange(&exchange_code(&page.html), Some(&cookie), no_device)
        .await;
    assert_eq!(refused.outcome.expect_err("no device").status, 400);

    let (page, cookie) = harness.completed().await;
    // The last user cannot be deleted.
    harness.users.create_user("keeper").await.expect("created");
    harness
        .users
        .delete_user(Uuid::parse_str(&account(&harness).await.id).expect("an id"))
        .await
        .expect("deleted");
    let refused = harness
        .exchange(
            &exchange_code(&page.html),
            Some(&cookie),
            device("browser-1"),
        )
        .await;
    assert_eq!(refused.outcome.expect_err("no account").status, 400);
    assert_eq!(session_count(&harness).await, 0);
}

#[tokio::test]
async fn the_exchange_answers_not_found_while_the_plugin_is_disabled() {
    let harness = Harness::new().await;
    let (page, cookie) = harness.completed().await;
    harness
        .sso
        .plugins
        .disable_plugin(crate::sso::EXTENSION_ID)
        .await
        .expect("disabled");
    let answer = harness
        .sso
        .exchange_oidc_sign_in(
            Secret::new(exchange_code(&page.html)),
            Some(Secret::new(cookie)),
            device("browser-1"),
        )
        .await;
    assert!(
        matches!(answer, Err(ServiceError::NotFound(_))),
        "{answer:?}"
    );
}
