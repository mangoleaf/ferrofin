//! The account rules every sign-in passes, alone
//! ([`UserManager::ensure_sign_in_allowed`]): each refuses with the error the
//! password sign-in gives, in the password sign-in's order.

use super::*;
use ferrofin_model::users::{AccessSchedule, DynamicDayOfWeek};

const NAME: &str = "sign-in-rules";
const DISABLED: &str =
    "The sign-in-rules account is currently disabled. Please consult with your administrator.";
const NO_REMOTE_ACCESS: &str = "remote access is not permitted for this user";
const OUTSIDE_SCHEDULE: &str = "User is not allowed access at this time.";
const LOCAL: &str = "127.0.0.1";
const REMOTE: &str = "203.0.113.8";

/// A manager whose local network is the loopback alone, and its one
/// passwordless user, saved with `policy`.
async fn user_with(policy: &UserPolicy) -> (FerrofinUserManager, UserEntity) {
    let network = Arc::new(RwLock::new(
        ferrofin_networking::NetworkManager::with_defaults(
            ferrofin_networking::NetworkConfiguration::default(),
            "127.0.0.1,1,lo",
        ),
    ));
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await).with_network(network);
    let user = mgr.create_user(NAME).await.unwrap();
    mgr.update_policy(Uuid::parse_str(&user.id).unwrap(), policy)
        .await
        .unwrap();
    let user = mgr.get_user_by_name(NAME).await.unwrap().unwrap();
    (mgr, user)
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

/// A window that opens at the end of the day, so no time is inside it.
fn never() -> Vec<AccessSchedule> {
    every_day(24.0, 24.0)
}

/// The message of a refusal, `None` for a sign-in let through.
fn refusal<T>(result: Result<T, ServiceError>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(ServiceError::Forbidden(message)) => Some(message),
        Err(other) => panic!("not a refusal: {other:?}"),
    }
}

#[rstest::rstest]
#[case::disabled(true, true, vec![], LOCAL, Some(DISABLED))]
#[case::remote_without_remote_access(false, false, vec![], REMOTE, Some(NO_REMOTE_ACCESS))]
#[case::local_without_remote_access(false, false, vec![], LOCAL, None)]
#[case::outside_the_parental_schedule(false, true, never(), LOCAL, Some(OUTSIDE_SCHEDULE))]
#[case::inside_the_parental_schedule(false, true, every_day(0.0, 24.0), LOCAL, None)]
#[case::allowed(false, true, vec![], REMOTE, None)]
#[case::disabled_comes_first(true, false, never(), REMOTE, Some(DISABLED))]
#[case::remote_access_before_the_schedule(false, false, never(), REMOTE, Some(NO_REMOTE_ACCESS))]
#[tokio::test]
async fn each_rule_answers_as_the_password_sign_in_does(
    #[case] is_disabled: bool,
    #[case] enable_remote_access: bool,
    #[case] access_schedules: Vec<AccessSchedule>,
    #[case] address: &str,
    #[case] expected: Option<&str>,
) {
    let (mgr, user) = user_with(&UserPolicy {
        is_disabled,
        enable_remote_access,
        access_schedules,
        ..UserPolicy::default()
    })
    .await;

    let rules = refusal(mgr.ensure_sign_in_allowed(&user, address).await);
    assert_eq!(rules.as_deref(), expected);

    let password = mgr.authenticate_user(NAME, "", address, true).await;
    if let Ok(signed_in) = &password {
        assert!(signed_in.is_some(), "an allowed sign-in signs in");
    }
    assert_eq!(
        refusal(password),
        rules,
        "the password sign-in answers alike"
    );
}
