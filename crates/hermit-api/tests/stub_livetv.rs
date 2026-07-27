//! Unit 6 client-contract gate: the **mutation and by-id** slice of the
//! `LiveTvController` is registered but not yet ported (Live TV has no
//! tuner/EPG/DVR backend), so each of those routes must resolve to
//! `501 Not Implemented`, never `404`.
//!
//! The **read/query** slice (channel/program/recording/timer *listings* plus
//! `Info`/`GuideInfo`/defaults) is real: it returns the empty/disabled
//! "nothing configured" state so the web UI works — those ops are exercised by
//! `live_tv_empty_state.rs`, not here. What remains 501 is the surface a
//! browsing client never reaches (parent lists are empty): create/update/delete
//! a timer, add a tuner host / listing provider, reset a tuner, delete a
//! recording, the live-stream endpoints, and single-resource `{id}` lookups.
//!
//! A real client (Wolphin) probing any of them must learn "route exists, not
//! implemented", not "no such route". The auth-context middleware is
//! non-rejecting and the shared `not_implemented` stub takes no auth extractor,
//! so a tokenless probe reaches the stub and yields `501` deterministically.
//!
//! The full superset gate lives in `contract_superset.rs`; this test enumerates
//! the still-stubbed `LiveTv` surface explicitly so a regression fails with a
//! clear signal.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use hermit_api::create_router;
use hermit_api::test_support::fake_state;
use tower::ServiceExt;

/// The still-stubbed `(method, path)` ops of the `LiveTvController`, with
/// concrete segment values where the vendored route has a `{param}`. 22 ops.
const LIVETV_PROBES: &[(Method, &str)] = &[
    // Channel mapping (mutation)
    (Method::POST, "/LiveTv/ChannelMappings"),
    // Listing providers — SchedulesDirect proxy (not the XMLTV backend)
    (
        Method::GET,
        "/LiveTv/ListingProviders/SchedulesDirect/Countries",
    ),
    // Live streams (need the live-stream proxy backend)
    (Method::GET, "/LiveTv/LiveRecordings/recording-1/stream"),
    (Method::GET, "/LiveTv/LiveStreamFiles/stream-1/stream.mp4"),
    // Recordings — deprecated group-by-id lookup
    (Method::GET, "/LiveTv/Recordings/Groups/group-1"),
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

/// Guards the op count: of the `LiveTvController`'s 41 contract routes, 36 are
/// now real (the read/disabled surface, tuner/provider config, channel/program
/// by-id, tuner reset, and the full DVR timer/series-timer/recording CRUD). Five
/// remain stubbed: ChannelMappings, the SchedulesDirect proxy, the two live-stream
/// proxy endpoints, and the deprecated recording-group-by-id lookup. This doubles
/// as a drift alarm — promoting another op to real must decrement this.
#[test]
fn unit6_covers_remaining_stubbed_livetv_ops() {
    assert_eq!(
        LIVETV_PROBES.len(),
        5,
        "5 LiveTv ops remain 501-stubbed (41 total − 36 real); probe table drifted"
    );
}
