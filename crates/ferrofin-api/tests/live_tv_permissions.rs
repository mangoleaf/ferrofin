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
//! These tests pin the gate itself: the deny is what regresses silently, so
//! every case asserts the *denied* status as well as the allowed one, and the
//! Live TV manager is a fake that records whether it was reached — a `403` that
//! still ran the handler would be no gate at all.

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
    async fn get_series_timers(&self) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
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
/// (`UserEntityExtensions.cs:187-188`).
fn policy(access: bool, management: bool, admin: bool) -> UserPolicy {
    UserPolicy {
        enable_live_tv_access: access,
        enable_live_tv_management: management,
        is_administrator: admin,
        ..UserPolicy::default()
    }
}

async fn probe(state: AppState, method: &str, uri: &str) -> StatusCode {
    create_router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
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

/// Being an administrator does not stand in for `EnableLiveTvAccess` — the
/// policy names the permission, and only the permission.
#[tokio::test]
async fn administrator_without_the_access_permission_is_still_refused() {
    let manager = Arc::new(CountingLiveTv::default());
    let state =
        authed_fake_state_with_policy(policy(false, false, true)).with_live_tv(manager.clone());
    assert_eq!(
        probe(state, "GET", "/LiveTv/Info").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(manager.reached.load(Ordering::SeqCst), 0);
}

/// A management route admits the administrator *or* the holder of
/// `EnableLiveTvManagement`, and refuses a plain account that is neither — where
/// Ferrofin previously let any authenticated caller cancel a timer.
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
