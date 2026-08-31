//! The Live TV permission gates, end to end through the real router.
//!
//! v10.11.8's `LiveTvController` declares `[Authorize(Policy = Policies.LiveTvAccess)]`
//! on 22 read actions and `[Authorize(Policy = Policies.LiveTvManagement)]` on
//! its seven timer/recording mutations
//! (`ApiServiceCollectionExtensions.cs:80-81` registers both as
//! `UserPermissionRequirement`s). Ferrofin served the read actions under plain
//! `RequireAuth` and the mutations under `RequireAuth`/`RequireAdmin`, so
//! "Allow Live TV access" was a checkbox the dashboard rendered and the server
//! ignored.
//!
//! A `UserPermissionRequirement` is **not** a bare permission check, and these
//! tests pin all four of its arms in the order `DefaultAuthorizationHandler`
//! evaluates them — API key, remote-without-`EnableRemoteAccess`,
//! administrator, access schedule, then the named permission. The order is
//! itself under test: `context.Fail()` is unconditional in ASP.NET Core, so the
//! remote arm refuses an administrator (it precedes the admin arm) while the
//! schedule arm does not (it follows). See `require_live_tv_permission`'s doc
//! comment for the citation chain.
//!
//! These tests pin the gate itself: the deny is what regresses silently, so
//! every case asserts the *denied* status as well as the allowed one, and the
//! Live TV manager is a fake that records whether it was reached — a `403` that
//! still ran the handler would be no gate at all. Each `Fail` arm also carries a
//! positive twin (the same account, LAN-side; the same account, open window) so
//! a case cannot pass by refusing everything.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{authed_fake_state_with_policy, elevated_fake_state, fake_state};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::live_tv::{
    GuideInfo, ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use ferrofin_model::querying::QueryResult;
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::stubs::{LiveTvChannelQuery, LiveTvManager};
use tower::ServiceExt;
use uuid::Uuid;

/// A Live TV manager that answers the probed routes and counts how often the
/// handler behind the gate actually ran.
#[derive(Default)]
struct CountingLiveTv {
    reached: AtomicUsize,
}

#[async_trait]
impl LiveTvManager for CountingLiveTv {
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        Ok(LiveTvInfo::default())
    }
    async fn get_guide_info(&self) -> Result<GuideInfo, ServiceError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        Ok(GuideInfo::default())
    }
    async fn cancel_timer(&self, _id: &str) -> Result<(), ServiceError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[allow(unused_variables)]
    async fn get_tuner_hosts(&self) -> Result<Vec<TunerHostInfo>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn save_tuner_host(&self, info: TunerHostInfo) -> Result<TunerHostInfo, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    fn tuner_host_types(&self) -> Vec<ferrofin_model::dto::NameIdPair> {
        unimplemented!("not probed by the gate tests")
    }
    async fn get_lineups(
        &self,
        _provider_id: Option<&str>,
        _provider_type: Option<&str>,
        _country: Option<&str>,
        _location: Option<&str>,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    async fn get_channel_mapping_options(
        &self,
        _provider_id: &str,
    ) -> Result<ferrofin_model::live_tv::ChannelMappingOptionsDto, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    async fn set_channel_mapping(
        &self,
        _provider_id: &str,
        _tuner_channel_id: &str,
        _provider_channel_id: &str,
    ) -> Result<ferrofin_model::live_tv::TunerChannelMapping, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn discover_tuners(
        &self,
        discovery_duration_ms: u64,
        new_devices_only: bool,
    ) -> Result<Vec<TunerHostInfo>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn delete_tuner_host(&self, id: &str) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_listing_providers(&self) -> Result<Vec<ListingsProviderInfo>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn save_listing_provider(
        &self,
        info: ListingsProviderInfo,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn delete_listing_provider(&self, id: &str) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_channels(
        &self,
        query: &LiveTvChannelQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_channel(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_programs(
        &self,
        query: &InternalItemsQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_recommended_programs(
        &self,
        query: &InternalItemsQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_program(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn reset_tuner(&self, id: &str) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_channel_stream_url(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_timer(&self, id: &str) -> Result<Option<TimerInfoDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn create_timer(&self, timer: TimerInfoDto) -> Result<String, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn update_timer(&self, id: &str, timer: TimerInfoDto) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_series_timers(
        &self,
        _query: &ferrofin_model::live_tv::SeriesTimerQuery,
    ) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_series_timer(&self, id: &str) -> Result<Option<SeriesTimerInfoDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn create_series_timer(&self, timer: SeriesTimerInfoDto) -> Result<String, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn update_series_timer(
        &self,
        id: &str,
        timer: SeriesTimerInfoDto,
    ) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn cancel_series_timer(&self, id: &str) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_recordings(&self) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_recording(&self, id: Uuid) -> Result<Option<BaseItemDto>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_recording_path(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
    #[allow(unused_variables)]
    async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
        unimplemented!("not probed by the gate tests")
    }
}

/// The policy a stock Jellyfin account is seeded with for the two Live TV
/// permissions: access granted, management withheld
/// (`UserEntityExtensions.cs:187-188`). `EnableRemoteAccess` also defaults to
/// `true` there, and `UserPolicy::default()` agrees, so the remote arm is out
/// of the way unless a case turns it off.
fn policy(access: bool, management: bool, admin: bool) -> UserPolicy {
    UserPolicy {
        enable_live_tv_access: access,
        enable_live_tv_management: management,
        is_administrator: admin,
        enable_remote_access: true,
        ..UserPolicy::default()
    }
}

async fn probe(state: AppState, method: &str, uri: &str) -> StatusCode {
    probe_from(state, method, uri, None).await
}

/// [`probe`] with an explicit transport peer, so a case can put the caller
/// outside the local network. `None` leaves `ConnectInfo` absent, which
/// `client_address` resolves to loopback exactly as C#
/// `GetNormalizedRemoteIP` does.
async fn probe_from(state: AppState, method: &str, uri: &str, peer: Option<&str>) -> StatusCode {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
    }
    create_router(state)
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

/// A read route is served when the caller holds `EnableLiveTvAccess` and
/// refused — without reaching the handler — when they do not.
#[tokio::test]
async fn live_tv_reads_require_the_access_permission() {
    for (granted, expected) in [(true, StatusCode::OK), (false, StatusCode::FORBIDDEN)] {
        for uri in ["/LiveTv/Info", "/LiveTv/GuideInfo"] {
            let manager = Arc::new(CountingLiveTv::default());
            let state = authed_fake_state_with_policy(policy(granted, false, false))
                .with_live_tv(manager.clone());
            assert_eq!(
                probe(state, "GET", uri).await,
                expected,
                "{uri} granted={granted}"
            );
            assert_eq!(
                manager.reached.load(Ordering::SeqCst),
                usize::from(granted),
                "a refused {uri} must not reach the manager"
            );
        }
    }
}

/// "Admins can do everything" — `DefaultAuthorizationHandler.cs:73-77`. It runs
/// for `UserPermissionRequirement` because that requirement subclasses
/// `DefaultAuthorizationRequirement` (`UserPermissionRequirement.cs:9`) and the
/// handler is an `AuthorizationHandler<DefaultAuthorizationRequirement>`,
/// registered first (`ApiServiceCollectionExtensions.cs:59`).
///
/// This test previously asserted the opposite — `403`, on the reading that "the
/// policy names the permission, and only the permission". That reading came
/// from `UserPermissionHandler` alone and is contradicted by the same pinned
/// tag: a real 10.11.8 served an administrator whose `EnableLiveTvManagement`
/// was `0`, and the arm responsible is per-*requirement*, not per-policy, so it
/// governs `EnableLiveTvAccess` identically.
///
/// Both policies are asserted here so the sibling can never drift from it again.
#[tokio::test]
async fn an_administrator_is_admitted_without_either_live_tv_permission() {
    for (method, uri) in [("GET", "/LiveTv/Info"), ("DELETE", "/LiveTv/Timers/t1")] {
        let manager = Arc::new(CountingLiveTv::default());
        let state =
            authed_fake_state_with_policy(policy(false, false, true)).with_live_tv(manager.clone());
        assert_ne!(
            probe(state, method, uri).await,
            StatusCode::FORBIDDEN,
            "{uri}: an administrator must not be refused by a UserPermissionRequirement"
        );
        assert_eq!(manager.reached.load(Ordering::SeqCst), 1, "{uri}");
    }
}

/// `DefaultAuthorizationHandler.cs:66-70` fails a caller who is outside the
/// local network and lacks `EnableRemoteAccess`. It sits *before* the admin arm,
/// and `context.Fail()` is unconditional in ASP.NET Core, so it refuses an
/// administrator too — the one ordering detail that distinguishes a real port of
/// this policy from a permission check with an admin bypass bolted on.
#[tokio::test]
async fn a_remote_caller_without_remote_access_is_refused_even_as_administrator() {
    for admin in [false, true] {
        let denied = UserPolicy {
            enable_remote_access: false,
            ..policy(true, true, admin)
        };
        let manager = Arc::new(CountingLiveTv::default());
        let state = authed_fake_state_with_policy(denied).with_live_tv(manager.clone());
        assert_eq!(
            probe_from(state, "GET", "/LiveTv/Info", Some("203.0.113.7:9000")).await,
            StatusCode::FORBIDDEN,
            "admin={admin}"
        );
        assert_eq!(manager.reached.load(Ordering::SeqCst), 0, "admin={admin}");

        // Same account, same permission, on the LAN: the arm is about *where*
        // the caller is, so this must still be served.
        let manager = Arc::new(CountingLiveTv::default());
        let local = UserPolicy {
            enable_remote_access: false,
            ..policy(true, true, admin)
        };
        let state = authed_fake_state_with_policy(local).with_live_tv(manager.clone());
        assert_eq!(
            probe_from(state, "GET", "/LiveTv/Info", Some("127.0.0.1:9000")).await,
            StatusCode::OK,
            "admin={admin}"
        );
        assert_eq!(manager.reached.load(Ordering::SeqCst), 1, "admin={admin}");
    }
}

/// `DefaultAuthorizationHandler.cs:81-84` fails a caller outside their access
/// schedule, because `UserPermissionRequirement` leaves
/// `validateParentalSchedule` at `true` (`UserPermissionRequirement.cs:17`).
/// It sits *after* the admin arm, so an administrator is not schedule-bound —
/// the mirror image of the remote arm above, and the reason both orderings are
/// pinned rather than assumed.
#[tokio::test]
async fn an_access_schedule_gates_the_non_administrator_only() {
    // A window that cannot contain "now": `StartHour`/`EndHour` are fractional
    // hours of the local day, and this one is empty on every day of the week.
    let closed = ferrofin_model::users::AccessSchedule {
        id: 1,
        user_id: Uuid::nil(),
        day_of_week: ferrofin_model::users::DynamicDayOfWeek::Everyday,
        start_hour: 25.0,
        end_hour: 26.0,
    };
    for (admin, expected) in [(false, StatusCode::FORBIDDEN), (true, StatusCode::OK)] {
        let scheduled = UserPolicy {
            access_schedules: vec![closed],
            ..policy(true, true, admin)
        };
        let manager = Arc::new(CountingLiveTv::default());
        let state = authed_fake_state_with_policy(scheduled).with_live_tv(manager.clone());
        assert_eq!(
            probe(state, "GET", "/LiveTv/Info").await,
            expected,
            "admin={admin}"
        );
        assert_eq!(
            manager.reached.load(Ordering::SeqCst),
            usize::from(expected == StatusCode::OK),
            "admin={admin}"
        );
    }

    // The same account with a window that spans the whole day is served, so the
    // case above is testing the window and not merely "a schedule exists".
    let open = ferrofin_model::users::AccessSchedule {
        start_hour: 0.0,
        end_hour: 24.0,
        ..closed
    };
    let manager = Arc::new(CountingLiveTv::default());
    let state = authed_fake_state_with_policy(UserPolicy {
        access_schedules: vec![open],
        ..policy(true, true, false)
    })
    .with_live_tv(manager.clone());
    assert_eq!(probe(state, "GET", "/LiveTv/Info").await, StatusCode::OK);
    assert_eq!(manager.reached.load(Ordering::SeqCst), 1);
}

/// A management route admits the administrator (the arm at
/// `DefaultAuthorizationHandler.cs:73-77`) *or* the holder of
/// `EnableLiveTvManagement` (the arm in `UserPermissionHandler`), and refuses a
/// plain account that is neither — where Ferrofin previously let any
/// authenticated caller cancel a timer.
#[tokio::test]
async fn timer_mutations_require_management_or_administrator() {
    for (management, admin, expected) in [
        (true, false, StatusCode::NO_CONTENT),
        (false, true, StatusCode::NO_CONTENT),
        (true, true, StatusCode::NO_CONTENT),
        (false, false, StatusCode::FORBIDDEN),
    ] {
        let manager = Arc::new(CountingLiveTv::default());
        let state = authed_fake_state_with_policy(policy(true, management, admin))
            .with_live_tv(manager.clone());
        let status = probe(state, "DELETE", "/LiveTv/Timers/t1").await;
        assert_eq!(status, expected, "management={management} admin={admin}");
        assert_eq!(
            manager.reached.load(Ordering::SeqCst),
            usize::from(expected != StatusCode::FORBIDDEN),
            "a refused mutation must not reach the manager"
        );
    }
}

/// An API key carries global permissions and satisfies both policies outright —
/// `UserPermissionHandler`: "Api keys have global permissions, so just succeed
/// the requirement."
#[tokio::test]
async fn an_api_key_satisfies_both_live_tv_policies() {
    let manager = Arc::new(CountingLiveTv::default());
    let state = elevated_fake_state().with_live_tv(manager.clone());
    assert_eq!(
        probe(state.clone(), "GET", "/LiveTv/Info").await,
        StatusCode::OK
    );
    assert_eq!(
        probe(state, "DELETE", "/LiveTv/Timers/t1").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(manager.reached.load(Ordering::SeqCst), 2);
}

/// The gate is not a substitute for authentication: a tokenless caller is still
/// `401`, not `403`.
#[tokio::test]
async fn a_tokenless_caller_is_unauthorized_not_forbidden() {
    let state = fake_state().with_live_tv(Arc::new(CountingLiveTv::default()));
    assert_eq!(
        probe(state, "GET", "/LiveTv/Info").await,
        StatusCode::UNAUTHORIZED
    );
}
