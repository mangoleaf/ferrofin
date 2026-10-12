//! Prometheus-scrapable `/metrics` for Ferrofin, instrumented through the
//! **OpenTelemetry** API (one meter provider → prometheus exporter → registry).
//!
//! This is a **parity port**: where Jellyfin's `/metrics` (prometheus-net)
//! exposes a metric whose concept exists in Rust, Ferrofin exposes the same name,
//! labels, and buckets — so existing Jellyfin Grafana dashboards work unchanged.
//! The `prometheus` crate is a direct dep of this crate ONLY, used solely for
//! `Registry` + `TextEncoder` (the 0.32 exporter ships no render helper).
//!
//! Shape:
//! - [`init`] installs the global meter provider and registers the `http_*`
//!   (via [`track_http`]) and `process_*` / `ferrofin_tokio_*` families.
//! - [`MetricsHandle::router`] serves `GET /metrics`; [`MetricsHandle::gauge_cell`]
//!   / [`MetricsHandle::gauge_map`] mint sampler-fed gauges (async-sourced values
//!   go through the [`GaugeCell`] / [`GaugeMap`] mirror — callbacks are sync).
//! - [`RouteLabels`] gives the HTTP metrics their `controller`/`action` labels
//!   from the vendored OpenAPI spec.
//!
//! Every instrument added here must satisfy the metric-collection rules in
//! `docs/conventions/METRICS.md` (parity-first names, bounded labels,
//! noop-when-disabled, sync callbacks). Read that file before adding a metric.

mod error;
mod gauges;
mod http;
mod init;
mod labels;
mod process;
mod router;

pub use error::MetricsError;
pub use gauges::{GaugeCell, GaugeMap};
pub use http::track_http;
pub use init::{MetricsHandle, init};
pub use labels::RouteLabels;

/// Every metric name this crate's own instruments expose (the `http_*` and
/// `process_*` parity families plus `ferrofin_tokio_*`), for the dashboard
/// lint test. The integration test asserts each one renders.
pub const METRIC_NAMES: &[&str] = &[
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
    "ferrofin_tokio_workers",
    "ferrofin_tokio_alive_tasks",
];
