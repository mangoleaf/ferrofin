//! The Live TV **read surface** is real (empty/disabled "nothing configured"
//! state), not a `501` stub — this is what unbreaks the web UI, whose home
//! screen calls `GET /LiveTv/Programs/Recommended` on load.
//!
//! Each route below is served by a real `RequireAuth` handler, so a tokenless
//! probe against `fake_state` reaches the auth extractor and yields `401` — the
//! point is that it is **neither `501` (still stubbed) nor `404` (unrouted)**.
//! The bodies are `Json(<Dto>::default())` and cannot fail, so proving the route
//! is mounted-and-real is the whole regression guard. The still-stubbed
//! mutation/by-id ops are covered by `stub_livetv.rs`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use ferrofin_api::create_router;
use ferrofin_api::test_support::fake_state;
use tower::ServiceExt;

/// The 19 real read/query ops promoted off the `501` stub.
const LIVETV_REAL: &[(Method, &str)] = &[
    (Method::GET, "/LiveTv/Info"),
    (Method::GET, "/LiveTv/GuideInfo"),
    (Method::GET, "/LiveTv/Channels"),
    (Method::GET, "/LiveTv/Programs"),
    (Method::POST, "/LiveTv/Programs"),
    (Method::GET, "/LiveTv/Programs/Recommended"),
    (Method::GET, "/LiveTv/Recordings"),
    (Method::GET, "/LiveTv/Recordings/Folders"),
    (Method::GET, "/LiveTv/Recordings/Groups"),
    (Method::GET, "/LiveTv/Recordings/Series"),
    (Method::GET, "/LiveTv/Timers"),
    (Method::GET, "/LiveTv/Timers/Defaults"),
    (Method::GET, "/LiveTv/SeriesTimers"),
    (Method::GET, "/LiveTv/ChannelMappingOptions"),
    (Method::GET, "/LiveTv/ListingProviders/Default"),
    (Method::GET, "/LiveTv/ListingProviders/Lineups"),
    (Method::GET, "/LiveTv/TunerHosts/Types"),
    (Method::GET, "/LiveTv/Tuners/Discover"),
    (Method::GET, "/LiveTv/Tuners/Discvover"),
];

#[tokio::test]
async fn livetv_read_routes_are_real_not_stubbed() {
    let router = create_router(fake_state());

    for (method, uri) in LIVETV_REAL {
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
        let status = response.status();
        assert_ne!(
            status,
            StatusCode::NOT_IMPLEMENTED,
            "{method} {uri} is still on the 501 stub — expected a real handler"
        );
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri} is unrouted (404) — it must be registered"
        );
    }
}

/// Drift alarm: 19 read ops are real. Promoting/demoting one must update this.
#[test]
fn livetv_real_op_count_is_19() {
    assert_eq!(LIVETV_REAL.len(), 19, "LiveTv real read-op table drifted");
}
