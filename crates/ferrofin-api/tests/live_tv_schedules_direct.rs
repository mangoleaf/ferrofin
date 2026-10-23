//! The Schedules Direct routes through the real router: the country list,
//! saving a provider (`POST /LiveTv/ListingProviders`) and its lineups
//! (`GET /LiveTv/ListingProviders/Lineups`).
//!
//! Port of `LiveTvController.GetSchedulesDirectCountries`: the handler streams
//! the raw JSON document the [`LiveTvManager`] hands back (which the manager
//! fetched from Schedules Direct, or served from its memory/disk cache) as
//! `application/json` — never re-serialised, never parsed. The manager here is a
//! recording fake, so the tests pin the HTTP contract end to end: the admin
//! gate (`RequiresElevation` upstream: `401` tokenless, `403` plain user), the
//! byte-exact passthrough, and the `500` an upstream fetch failure becomes
//! (`EnsureSuccessStatusCode` throwing upstream).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

/// A [`LiveTvManager`] whose live methods are the Schedules Direct country
/// list, the provider save and the lineups; it counts and records calls so
/// the test can prove what the handler asked the seam for.
struct CountriesLiveTv {
    document: Option<Vec<u8>>,
    calls: AtomicUsize,
    /// Every `save_listing_provider` call: the info and the two flags.
    saved: Mutex<Vec<(ListingsProviderInfo, bool, bool)>>,
    /// Every `get_lineups` call: id, type, country, location.
    lineups: Mutex<Vec<[Option<String>; 4]>>,
}

impl CountriesLiveTv {
    fn serving(document: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            document: Some(document.to_vec()),
            calls: AtomicUsize::new(0),
            saved: Mutex::new(Vec::new()),
            lineups: Mutex::new(Vec::new()),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            document: None,
            calls: AtomicUsize::new(0),
            saved: Mutex::new(Vec::new()),
            lineups: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LiveTvManager for CountriesLiveTv {
    async fn get_guide_info(&self) -> Result<ferrofin_model::live_tv::GuideInfo, ServiceError> {
        unimplemented!("this fake is never asked for the guide window")
    }
    async fn get_recommended_programs(
        &self,
        _query: &ferrofin_traits::options::InternalItemsQuery,
        _options: &ferrofin_traits::options::DtoOptions,
    ) -> Result<ferrofin_model::querying::QueryResult<ferrofin_model::dto::BaseItemDto>, ServiceError>
    {
        unimplemented!("this fake is never asked for recommended programs")
    }
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
    fn tuner_host_types(&self) -> Vec<ferrofin_model::dto::NameIdPair> {
        unreachable!()
    }
    async fn discover_tuners(
        &self,
        _discovery_duration_ms: u64,
        _new_devices_only: bool,
    ) -> Result<Vec<TunerHostInfo>, ServiceError> {
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
        info: ListingsProviderInfo,
        validate_login: bool,
        validate_listings: bool,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        self.saved
            .lock()
            .unwrap()
            .push((info.clone(), validate_login, validate_listings));
        Ok(ListingsProviderInfo {
            id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            ..info
        })
    }
    async fn get_lineups(
        &self,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
        country: Option<&str>,
        location: Option<&str>,
    ) -> Result<Vec<ferrofin_model::dto::NameIdPair>, ServiceError> {
        self.lineups
            .lock()
            .unwrap()
            .push([provider_id, provider_type, country, location].map(|v| v.map(str::to_owned)));
        Ok(vec![ferrofin_model::dto::NameIdPair {
            name: Some("Antenna".to_owned()),
            id: Some("USA-OTA-90210".to_owned()),
        }])
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
    /// The refresh a provider save queues; nothing to rebuild here.
    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        Ok(())
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
    async fn get_series_timers(
        &self,
        _query: &ferrofin_model::live_tv::SeriesTimerQuery,
    ) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
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

/// Sends `method uri` with an optional JSON body to the real router.
async fn send(
    state: AppState,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = create_router(state)
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, body.to_vec())
}

/// jellyfin-web's Schedules Direct login step
/// (`src/components/tvproviders/schedulesdirect.js`): the plain password in the
/// body, `ValidateLogin=true` in PascalCase. The flag reaches the manager and
/// the password is stored as given.
#[tokio::test]
async fn the_dashboard_login_save_forwards_validate_login_and_keeps_the_password() {
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let state = elevated_fake_state().with_live_tv(manager.clone());
    let (status, body) = send(
        state,
        "POST",
        "/LiveTv/ListingProviders?ValidateLogin=true",
        Some(r#"{"Type":"SchedulesDirect","Username":"user","EnableAllTuners":true,"Password":"plain"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let saved: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(saved["Id"], "0123456789abcdef0123456789abcdef");

    let calls = manager.saved.lock().unwrap();
    let (info, validate_login, validate_listings) = &calls[0];
    assert_eq!((*validate_login, *validate_listings), (true, false));
    assert_eq!(info.type_.as_deref(), Some("SchedulesDirect"));
    assert_eq!(
        info.password
            .as_ref()
            .map(ferrofin_model::secret::Secret::expose),
        Some("plain")
    );
}

/// `AddListingProvider`'s `pw`: a non-empty one replaces the body's password
/// with its lowercase SHA1 hex (jellyfin/jellyfin#17174); `validateListings`
/// is forwarded; an empty `pw` changes nothing; absent flags are `false`.
#[tokio::test]
async fn pw_is_stored_as_its_lowercase_sha1_and_validate_listings_is_forwarded() {
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let state = elevated_fake_state().with_live_tv(manager.clone());
    let body = r#"{"Type":"SchedulesDirect","Username":"user","Password":"body"}"#;
    let (status, _) = send(
        state.clone(),
        "POST",
        "/LiveTv/ListingProviders?pw=secret&validateListings=true",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(state, "POST", "/LiveTv/ListingProviders?pw=", Some(body)).await;
    assert_eq!(status, StatusCode::OK);

    let calls = manager.saved.lock().unwrap();
    assert_eq!(
        calls[0]
            .0
            .password
            .as_ref()
            .map(ferrofin_model::secret::Secret::expose),
        Some("e5e9fa1ba31ecd1ae84f75caaa474f3a663f05f4")
    );
    assert_eq!((calls[0].1, calls[0].2), (false, true));
    assert_eq!(
        calls[1]
            .0
            .password
            .as_ref()
            .map(ferrofin_model::secret::Secret::expose),
        Some("body")
    );
    assert_eq!((calls[1].1, calls[1].2), (false, false));
}

/// `RequiresElevation`: a plain user cannot save a provider.
#[tokio::test]
async fn saving_a_provider_is_elevation_gated() {
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let (status, _) = send(
        authed_fake_state().with_live_tv(manager.clone()),
        "POST",
        "/LiveTv/ListingProviders?ValidateLogin=true",
        Some(r#"{"Type":"SchedulesDirect"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(manager.saved.lock().unwrap().is_empty());
}

/// jellyfin-web's lineup picker: `Id`, `Location` (the postal code) and
/// `Country` reach the manager, whatever their casing.
#[tokio::test]
async fn lineups_forward_the_id_location_and_country() {
    let manager = CountriesLiveTv::serving(SD_COUNTRIES);
    let state = elevated_fake_state().with_live_tv(manager.clone());
    let (status, body) = send(
        state,
        "GET",
        "/LiveTv/ListingProviders/Lineups?Id=p1&Location=90210&Country=USA",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let lineups: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(lineups[0]["Id"], "USA-OTA-90210");
    assert_eq!(
        manager.lineups.lock().unwrap()[0],
        [
            Some("p1".to_owned()),
            None,
            Some("USA".to_owned()),
            Some("90210".to_owned())
        ]
    );
}
