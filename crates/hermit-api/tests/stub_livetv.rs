//! Unit 6 client-contract gate: the `LiveTvController` — the single largest
//! Jellyfin controller — is registered but **not yet ported**, so every one of
//! its routes must resolve to `501 Not Implemented`, never `404`.
//!
//! These 41 operations span channels, the program guide, recordings, timers &
//! series timers, listing providers, tuner hosts, and live streams. A real
//! client (Wolphin) probing any of them must learn "route exists, not
//! implemented", not "no such route". The auth-context middleware is
//! non-rejecting and the shared `not_implemented` stub takes no auth extractor,
//! so a tokenless probe reaches the stub and yields `501` deterministically.
//!
//! The full superset gate lives in `contract_superset.rs`; this test enumerates
//! the whole `LiveTv` surface explicitly so a regression in any single op fails
//! with a clear signal.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use hermit_api::create_router;
use hermit_api::test_support::fake_state;
use tower::ServiceExt;

/// Every `(method, path)` of the `LiveTvController`, with concrete segment
/// values where the vendored route has a `{param}`. 41 ops total.
const LIVETV_PROBES: &[(Method, &str)] = &[
    // Channels
    (Method::GET, "/LiveTv/Channels"),
    (Method::GET, "/LiveTv/Channels/channel-1"),
    // Channel mapping
    (Method::GET, "/LiveTv/ChannelMappingOptions"),
    (Method::POST, "/LiveTv/ChannelMappings"),
    // Guide & info
    (Method::GET, "/LiveTv/GuideInfo"),
    (Method::GET, "/LiveTv/Info"),
    // Listing providers
    (Method::POST, "/LiveTv/ListingProviders"),
    (Method::DELETE, "/LiveTv/ListingProviders"),
    (Method::GET, "/LiveTv/ListingProviders/Default"),
    (Method::GET, "/LiveTv/ListingProviders/Lineups"),
    (
        Method::GET,
        "/LiveTv/ListingProviders/SchedulesDirect/Countries",
    ),
    // Live streams
    (Method::GET, "/LiveTv/LiveRecordings/recording-1/stream"),
    (Method::GET, "/LiveTv/LiveStreamFiles/stream-1/stream.mp4"),
    // Programs
    (Method::GET, "/LiveTv/Programs"),
    (Method::POST, "/LiveTv/Programs"),
    (Method::GET, "/LiveTv/Programs/Recommended"),
    (Method::GET, "/LiveTv/Programs/program-1"),
    // Recordings
    (Method::GET, "/LiveTv/Recordings"),
    (Method::GET, "/LiveTv/Recordings/Folders"),
    (Method::GET, "/LiveTv/Recordings/Groups"),
    (Method::GET, "/LiveTv/Recordings/Groups/group-1"),
    (Method::GET, "/LiveTv/Recordings/Series"),
    (Method::GET, "/LiveTv/Recordings/recording-1"),
    (Method::DELETE, "/LiveTv/Recordings/recording-1"),
    // Series timers
    (Method::GET, "/LiveTv/SeriesTimers"),
    (Method::POST, "/LiveTv/SeriesTimers"),
    (Method::GET, "/LiveTv/SeriesTimers/timer-1"),
    (Method::POST, "/LiveTv/SeriesTimers/timer-1"),
    (Method::DELETE, "/LiveTv/SeriesTimers/timer-1"),
    // Timers
    (Method::GET, "/LiveTv/Timers"),
    (Method::POST, "/LiveTv/Timers"),
    (Method::GET, "/LiveTv/Timers/Defaults"),
    (Method::GET, "/LiveTv/Timers/timer-1"),
    (Method::POST, "/LiveTv/Timers/timer-1"),
    (Method::DELETE, "/LiveTv/Timers/timer-1"),
    // Tuner hosts
    (Method::POST, "/LiveTv/TunerHosts"),
    (Method::DELETE, "/LiveTv/TunerHosts"),
    (Method::GET, "/LiveTv/TunerHosts/Types"),
    // Tuners
    (Method::GET, "/LiveTv/Tuners/Discover"),
    (Method::GET, "/LiveTv/Tuners/Discvover"),
    (Method::POST, "/LiveTv/Tuners/tuner-1/Reset"),
];

#[tokio::test]
async fn unit6_livetv_stub_routes_return_501_not_404() {
    let router = create_router(fake_state());

    for (method, uri) in LIVETV_PROBES {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(*uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "Unit-6 LiveTv contract route {method} {uri} must be a registered 501 stub, got {}",
            response.status()
        );
    }
}

/// Guards the op count: the `LiveTvController` contributes exactly 41 routes to
/// the contract, so this test doubles as a drift alarm for the probe table.
#[test]
fn unit6_covers_all_41_livetv_ops() {
    assert_eq!(
        LIVETV_PROBES.len(),
        41,
        "LiveTvController has 41 ops in the vendored contract; probe table drifted"
    );
}
