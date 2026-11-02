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
use std::time::Duration;
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
        .route("/hang", get(std::future::pending::<&'static str>))
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

    drop_mid_handler(&app).await;
    send_extension_method(&app).await;

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
    assert_dropped_request_left_in_progress(&out);
    assert_extension_method_is_other(&out);
    // Unmatched request rendered with empty-string labels (prometheus-net convention).
    assert!(
        out.contains(r#"endpoint="""#) && out.contains(r#"code="404""#),
        "missing empty-label 404 series:\n{out}"
    );

    // Every name the crate declares renders (the dashboard lint test trusts
    // this list).
    for name in ferrofin_metrics::METRIC_NAMES {
        assert!(
            out.lines().any(|l| l.starts_with(name)),
            "declared metric `{name}` is absent from the exposition:\n{out}"
        );
    }

    // Parity name-set gate: every portable Jellyfin metric name renders here.
    // (dotnet_* / prometheus_net_* families are honest divergences — excluded.)
    for name in PORTABLE_JELLYFIN_METRICS {
        assert!(
            out.contains(name),
            "portable Jellyfin metric `{name}` is absent from Ferrofin's exposition:\n{out}"
        );
    }
}

/// Drops a request mid-handler, the way hyper drops the service future on a
/// client disconnect.
async fn drop_mid_handler(app: &Router) {
    // Any short bound works: the increment happens on the first poll.
    let dropped = tokio::time::timeout(
        Duration::from_millis(50),
        app.clone()
            .oneshot(Request::builder().uri("/hang").body(Body::empty()).unwrap()),
    )
    .await;
    assert!(dropped.is_err(), "the hanging handler completed");
}

/// The dropped request no longer counts as in progress.
fn assert_dropped_request_left_in_progress(out: &str) {
    let hang = out
        .lines()
        .find(|l| l.starts_with("http_requests_in_progress{") && l.contains(r#"endpoint="/hang""#))
        .unwrap_or_else(|| panic!("missing in-progress series for /hang:\n{out}"));
    assert!(
        hang.trim_end().ends_with(" 0"),
        "a dropped request left the in-progress gauge raised: {hang}"
    );
}

/// Sends a request with a non-standard method to a matched route.
async fn send_extension_method(app: &Router) {
    let extension = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::from_bytes(b"ZZZ1").unwrap())
                .uri("/Items/abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(extension.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// A non-standard method is reported as `_OTHER`, never verbatim.
fn assert_extension_method_is_other(out: &str) {
    assert!(
        !out.contains("ZZZ1") && out.contains(r#"method="_OTHER""#),
        "a non-standard method leaked into the method label:\n{out}"
    );
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
