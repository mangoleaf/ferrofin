//! `GET /LiveTv/ListingProviders/SchedulesDirect/Countries` through the real
//! router.
//!
//! Port of `LiveTvController.GetSchedulesDirectCountries`: the handler streams
//! the raw JSON document the [`LiveTvManager`] hands back (which the manager
//! fetched from Schedules Direct, or served from its memory/disk cache) as
//! `application/json` — never re-serialised, never parsed. The manager here is a
//! recording fake, so the tests pin the HTTP contract end to end: the admin
//! gate (`RequiresElevation` upstream: `401` tokenless, `403` plain user), the
//! byte-exact passthrough, and the `500` an upstream fetch failure becomes
//! (`EnsureSuccessStatusCode` throwing upstream).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{authed_fake_state, elevated_fake_state, fake_state};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::live_tv::{
    ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::stubs::{LiveTvChannelQuery, LiveTvManager};
use tower::ServiceExt;
use uuid::Uuid;

const ROUTE: &str = "/LiveTv/ListingProviders/SchedulesDirect/Countries";

/// A real (abridged) Schedules Direct `available/countries` document, with
/// whitespace and key order the server must not normalise.
const SD_COUNTRIES: &[u8] = br#"{ "North America": [ {"fullName":"United States","shortName":"USA","postalCodeExample":"22206","postalCode":"/\\d{5}/"} ],
 "Europe": [ {"fullName":"United Kingdom","shortName":"GBR","postalCodeExample":"SW1A","postalCode":"/^[A-Z]{1,2}[0-9][A-Z0-9]?$/"} ] }"#;

/// A [`LiveTvManager`] whose only live method is the Schedules Direct country
/// list; it counts calls so the test can prove the handler asked the seam once.
struct CountriesLiveTv {
    document: Option<Vec<u8>>,
    calls: AtomicUsize,
}

impl CountriesLiveTv {
    fn serving(document: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            document: Some(document.to_vec()),
            calls: AtomicUsize::new(0),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            document: None,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl LiveTvManager for CountriesLiveTv {
    async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.document
            .clone()
            .ok_or_else(|| ServiceError::backend("fetch https://json.schedulesdirect.org: 503"))
    }

    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        unreachable!()
    }
    async fn get_tuner_hosts(&self) -> Result<Vec<TunerHostInfo>, ServiceError> {
        unreachable!()
    }
    async fn save_tuner_host(&self, _info: TunerHostInfo) -> Result<TunerHostInfo, ServiceError> {
        unreachable!()
    }
    async fn delete_tuner_host(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_listing_providers(&self) -> Result<Vec<ListingsProviderInfo>, ServiceError> {
        unreachable!()
    }
    async fn save_listing_provider(
        &self,
        _info: ListingsProviderInfo,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        unreachable!()
    }
    async fn get_lineups(
        &self,
        _provider_id: Option<&str>,
        _provider_type: Option<&str>,
        _country: Option<&str>,
        _location: Option<&str>,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        unreachable!()
    }
    async fn get_channel_mapping_options(
        &self,
        _provider_id: &str,
    ) -> Result<ferrofin_model::live_tv::ChannelMappingOptionsDto, ServiceError> {
        unreachable!()
    }
    async fn set_channel_mapping(
        &self,
        _provider_id: &str,
        _tuner_channel_id: &str,
        _provider_channel_id: &str,
    ) -> Result<ferrofin_model::live_tv::TunerChannelMapping, ServiceError> {
        unreachable!()
    }
    async fn delete_listing_provider(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_channels(
        &self,
        _query: &LiveTvChannelQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_channel(
        &self,
        _id: Uuid,
        _user: Option<&UserEntity>,
        _options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_programs(
        &self,
        _query: &InternalItemsQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_program(
        &self,
        _id: Uuid,
        _user: Option<&UserEntity>,
        _options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_channel_stream_url(&self, _id: Uuid) -> Result<Option<String>, ServiceError> {
        unreachable!()
    }
    async fn get_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn get_timer(&self, _id: &str) -> Result<Option<TimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn create_timer(&self, _timer: TimerInfoDto) -> Result<String, ServiceError> {
        unreachable!()
    }
    async fn update_timer(&self, _id: &str, _timer: TimerInfoDto) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn cancel_timer(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_series_timers(&self) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn get_series_timer(
        &self,
        _id: &str,
    ) -> Result<Option<SeriesTimerInfoDto>, ServiceError> {
        unreachable!()
    }
    async fn create_series_timer(
        &self,
        _timer: SeriesTimerInfoDto,
    ) -> Result<String, ServiceError> {
        unreachable!()
    }
    async fn update_series_timer(
        &self,
        _id: &str,
        _timer: SeriesTimerInfoDto,
    ) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn cancel_series_timer(&self, _id: &str) -> Result<(), ServiceError> {
        unreachable!()
    }
    async fn get_recordings(&self) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_recording(&self, _id: Uuid) -> Result<Option<BaseItemDto>, ServiceError> {
        unreachable!()
    }
    async fn get_recording_path(&self, _id: Uuid) -> Result<Option<String>, ServiceError> {
        unreachable!()
    }
    async fn delete_recording(&self, _id: Uuid) -> Result<(), ServiceError> {
        unreachable!()
    }
}

async fn get(state: AppState) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = create_router(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(ROUTE)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, content_type, body.to_vec())
}

#[tokio::test]
async fn passes_the_schedules_direct_document_through_byte_for_byte() {
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let state = elevated_fake_state().with_live_tv(manager.clone());

    let (status, content_type, body) = get(state).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("application/json"));
    // Not `[]`, not re-serialised: the bytes SD served, including the
    // whitespace/key order a round trip through serde would lose.
    assert_eq!(body, SD_COUNTRIES);
    assert_eq!(manager.calls.load(Ordering::SeqCst), 1);
    // …and the document the client gets is the parseable SD shape.
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("SD JSON");
    assert_eq!(parsed["North America"][0]["shortName"], "USA");
}

#[tokio::test]
async fn an_upstream_fetch_failure_is_a_500() {
    let manager = CountriesLiveTv::failing();
    let state = elevated_fake_state().with_live_tv(manager.clone());

    let (status, _, _) = get(state).await;

    // `EnsureSuccessStatusCode` throws upstream → an unhandled 500; Ferrofin's
    // backend error maps the same way rather than faking an empty list.
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(manager.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn is_elevation_gated() {
    // Tokenless → 401, before the manager is consulted.
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let (status, _, _) = get(fake_state().with_live_tv(manager.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(manager.calls.load(Ordering::SeqCst), 0);

    // An authenticated non-administrator → 403 (`RequiresElevation`).
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let (status, _, _) = get(authed_fake_state().with_live_tv(manager.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(manager.calls.load(Ordering::SeqCst), 0);
}
