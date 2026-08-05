//! HTTP request instrumentation — the `http_*` family, prometheus-net parity.
//!
//! [`track_http`] is an [`axum::middleware::from_fn`] layer the composition root
//! mounts (only when metrics are enabled) AFTER routing, so it sees the
//! [`MatchedPath`] template and the final status code. When the crate's
//! instrument [`OnceLock`] is unset it is a pure pass-through.

use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

use crate::labels::RouteLabels;

/// The duration histogram boundaries (seconds) — prometheus-net.AspNetCore's
/// default exponential series `0.001 × 2ⁿ` for n = 0..15. Copied verbatim from
/// the live `contrib/metrics/jellyfin-metrics-fixture.txt`; existing Jellyfin
/// Grafana dashboards assume exactly these buckets.
const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.002, 0.004, 0.008, 0.016, 0.032, 0.064, 0.128, 0.256, 0.512, 1.024, 2.048, 4.096,
    8.192, 16.384, 32.768,
];

/// The process-global HTTP instrument set. Set once by
/// [`crate::init`]; read by [`track_http`] on every request.
static HTTP_INSTRUMENTS: OnceLock<HttpInstruments> = OnceLock::new();

/// The three `http_*` instruments plus the route-label lookup that gives them
/// their `controller`/`action` values.
pub(crate) struct HttpInstruments {
    received: Counter<u64>,
    in_progress: UpDownCounter<i64>,
    duration: Histogram<f64>,
    labels: RouteLabels,
}

impl HttpInstruments {
    /// Creates the `http_*` instruments on `meter` (names are their final
    /// Prometheus names — the exporter does no suffixing).
    pub(crate) fn new(meter: &Meter, labels: RouteLabels) -> Self {
        Self {
            received: meter
                .u64_counter("http_requests_received_total")
                .with_description("Provides the count of HTTP requests that have been processed.")
                .build(),
            in_progress: meter
                .i64_up_down_counter("http_requests_in_progress")
                .with_description("The number of requests currently in progress.")
                .build(),
            duration: meter
                .f64_histogram("http_request_duration_seconds")
                .with_description("The duration of HTTP requests processed by the application.")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            labels,
        }
    }
}

/// Installs the process-global HTTP instruments. A second call is a no-op (the
/// `OnceLock` is set-once) — the instruments live for the process lifetime.
pub(crate) fn install(instruments: HttpInstruments) {
    HTTP_INSTRUMENTS.set(instruments).ok();
}

/// axum middleware recording `http_requests_received_total`,
/// `http_requests_in_progress`, and `http_request_duration_seconds` with the
/// `code`/`method`/`controller`/`action`/`endpoint` labels.
///
/// Pass-through when the instruments are not installed (metrics disabled).
pub async fn track_http(req: Request, next: Next) -> Response {
    let Some(inst) = HTTP_INSTRUMENTS.get() else {
        return next.run(req).await;
    };

    let method = req.method().clone();
    // The matched route TEMPLATE (bounded cardinality), never the raw request
    // path. Unmatched requests (404) carry no `MatchedPath` → "".
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_default();
    // Contract routes: controller/action + the Jellyfin-form endpoint (spec path,
    // no leading slash). Non-contract matched routes (`/health/*`, `/metrics`):
    // empty controller/action, endpoint = the raw matched path (prometheus-net's
    // convention for its own middleware routes like `/health`).
    let (controller, action, endpoint) =
        inst.labels
            .lookup(&method, &matched)
            .unwrap_or(("", "", matched.as_str()));
    let shared = shared_labels(&method, controller, action, endpoint);

    inst.in_progress.add(1, &shared);
    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    inst.in_progress.add(-1, &shared);

    let mut labelled = shared;
    labelled.push(KeyValue::new(
        "code",
        response.status().as_u16().to_string(),
    ));
    inst.received.add(1, &labelled);
    inst.duration.record(elapsed, &labelled);

    response
}

/// The labels shared by every `http_*` instrument (in-progress omits `code`,
/// which the caller appends for the other two). `page` is always empty — it is a
/// Razor-Pages artifact prometheus-net emits on every series (fixture parity);
/// Hermit has no pages, so it is a constant `""`.
fn shared_labels(method: &Method, controller: &str, action: &str, endpoint: &str) -> Vec<KeyValue> {
    vec![
        KeyValue::new("method", method.as_str().to_owned()),
        KeyValue::new("controller", controller.to_owned()),
        KeyValue::new("action", action.to_owned()),
        KeyValue::new("page", ""),
        KeyValue::new("endpoint", endpoint.to_owned()),
    ]
}
