//! Integration coverage for the HTTP middleware + the parity name-set gate.
//!
//! These live in their own binary because [`ferrofin_metrics::init`] sets the
//! process-global meter provider and instrument `OnceLock` (set-once). One
//! `init` per process; the assertions share the single rendered exposition.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::get;
use ferrofin_metrics::RouteLabels;
use tower::ServiceExt as _;

/// A one-route spec so the stub `RouteLabels` maps `GET /Items/{itemId}` →
/// controller `Items`, action `GetItem` (identity normalizer — the endpoint
/// template already matches axum's `{param}` form).
fn stub_labels() -> RouteLabels {
    let spec =
        r#"{"paths":{"/Items/{itemId}":{"get":{"tags":["Items"],"operationId":"GetItem"}}}}"#;
    RouteLabels::from_openapi_spec(spec, str::to_owned)
}

fn app() -> Router {
    Router::new()
        .route("/Items/{itemId}", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(ferrofin_metrics::track_http))
}

#[tokio::test]
async fn tracks_requests_and_satisfies_parity_name_set() {
    let handle = ferrofin_metrics::init(stub_labels(), tokio::runtime::Handle::current())
        .expect("metrics init");
    let app = app();

    // A matched request: MatchedPath → endpoint template, labels from the spec.
    let matched = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Items/abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(matched.status(), StatusCode::OK);

    // An unmatched request (404, no MatchedPath) → empty endpoint/controller/action.
    let unmatched = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unmatched.status(), StatusCode::NOT_FOUND);

    let out = handle.render();

    // Matched request rendered with the Jellyfin-form endpoint (spec path, no
    // leading slash) + status + spec labels.
    assert!(
        out.contains(r#"endpoint="Items/{itemId}""#),
        "missing matched endpoint template:\n{out}"
    );
    assert!(
        out.contains(r#"controller="Items""#),
        "missing controller:\n{out}"
    );
    assert!(
        out.contains(r#"action="GetItem""#),
        "missing action:\n{out}"
    );
    assert!(
        out.contains(r#"code="200""#),
        "missing status code label:\n{out}"
    );
    // The `page` label (fixture parity) is present, always empty.
    assert!(
        out.contains(r#"page="""#),
        "missing empty page label:\n{out}"
    );
    // Histogram is exported with the fixture's exponential buckets.
    assert!(
        out.contains("http_request_duration_seconds_bucket"),
        "missing duration buckets:\n{out}"
    );
    assert!(
        out.contains(r#"le="0.032""#) && out.contains(r#"le="32.768""#),
        "duration buckets are not the fixture's exponential series:\n{out}"
    );
    // The raw path is NEVER a label value (cardinality).
    assert!(
        !out.contains("abc-123"),
        "raw path leaked into a label:\n{out}"
    );
    // In-progress returns to zero after both requests drain.
    assert!(
        out.lines()
            .any(|l| l.starts_with("http_requests_in_progress") && l.trim_end().ends_with(" 0")),
        "in-progress did not return to 0:\n{out}"
    );
    // Unmatched request rendered with empty-string labels (prometheus-net convention).
    assert!(
        out.contains(r#"endpoint="""#) && out.contains(r#"code="404""#),
        "missing empty-label 404 series:\n{out}"
    );

    // Parity name-set gate: every portable Jellyfin metric name renders here.
    // (dotnet_* / prometheus_net_* families are honest divergences — excluded.)
    for name in PORTABLE_JELLYFIN_METRICS {
        assert!(
            out.contains(name),
            "portable Jellyfin metric `{name}` is absent from Ferrofin's exposition:\n{out}"
        );
    }
}

/// The Jellyfin (prometheus-net) metric names whose concept exists in Rust and
/// that Ferrofin therefore ports 1:1. Kept in sync with
/// `contrib/metrics/jellyfin-metrics-fixture.txt`; the `dotnet_*` runtime
/// families are excluded by design (see `contrib/metrics/README.md`).
const PORTABLE_JELLYFIN_METRICS: &[&str] = &[
    "http_requests_received_total",
    "http_requests_in_progress",
    "http_request_duration_seconds",
    "process_cpu_seconds_total",
    "process_start_time_seconds",
    "process_open_handles",
    "process_working_set_bytes",
    "process_virtual_memory_bytes",
    "process_private_memory_bytes",
    "process_num_threads",
    "process_cpu_count",
];
