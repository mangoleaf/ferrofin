//! `LiveTvController` client-contract gate: **every** vendored `LiveTv` route now
//! resolves to a real handler — none returns `501 Not Implemented`.
//!
//! The read/query surface returns the empty/disabled "nothing configured" state;
//! tuner/provider config, channel/program by-id, tuner reset, and the DVR
//! timer/series-timer/recording CRUD are backed by the manager; and the last five
//! ops (channel mappings, the SchedulesDirect country list, the two live-stream /
//! recording file endpoints, and the deprecated recording-group-by-id lookup) are
//! now real too — faithful for Ferrofin's M3U+XMLTV backend (empty/`404` where no
//! DVR capture / live-stream buffering exists; the SchedulesDirect country list
//! is the cached SD document itself, pinned in `live_tv_schedules_direct.rs`).
//!
//! Each real handler sits behind the `RequireAuth` extractor, so a **tokenless**
//! probe is rejected with `401 Unauthorized` — never `501` (which only the shared
//! `not_implemented` stub returns) and never `404`. This test pins that the five
//! formerly-stubbed ops have graduated off the stub; the full superset gate lives
//! in `contract_superset.rs`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::fake_state;
use tower::ServiceExt;

/// The five `LiveTvController` ops promoted off the `501` stub, with concrete
/// segment values where the vendored route has a `{param}`.
const LIVETV_PROMOTED: &[(Method, &str)] = &[
    (Method::POST, "/LiveTv/ChannelMappings"),
    (
        Method::GET,
        "/LiveTv/ListingProviders/SchedulesDirect/Countries",
    ),
    (Method::GET, "/LiveTv/LiveRecordings/recording-1/stream"),
    (Method::GET, "/LiveTv/LiveStreamFiles/stream-1/stream.mp4"),
    (Method::GET, "/LiveTv/Recordings/Groups/group-1"),
];

#[tokio::test]
async fn promoted_livetv_routes_are_real_not_501() {
    let router = create_router(fake_state());

    for (method, uri) in LIVETV_PROMOTED {
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
        // A real handler behind `RequireAuth` rejects a tokenless probe with 401;
        // it must never be the `501` stub (nor a `404`).
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "LiveTv route {method} {uri} should be a real handler behind auth (401), got {}",
            response.status()
        );
    }
}

/// Guards that the promotion set is exactly the five formerly-stubbed ops; the
/// whole `LiveTvController` (41 contract routes) is now real. This doubles as a
/// drift alarm — a `LiveTv` route regressing to a `501` stub fails the test above.
#[test]
fn all_livetv_ops_are_now_real() {
    assert_eq!(
        LIVETV_PROMOTED.len(),
        5,
        "the last 5 LiveTv stubs were promoted to real handlers; probe table drifted"
    );
}
