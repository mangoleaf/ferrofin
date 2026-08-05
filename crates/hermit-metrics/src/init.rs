//! Pipeline construction: one OTel meter provider → prometheus exporter →
//! registry, plus the [`MetricsHandle`] that renders it and mints gauges.

use opentelemetry::metrics::{Meter, MeterProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::{Registry, TextEncoder};

use crate::error::MetricsError;
use crate::gauges::{GaugeCell, GaugeMap};
use crate::http::HttpInstruments;
use crate::labels::RouteLabels;

/// The live metrics pipeline. Must stay alive for the process lifetime: the
/// meter provider owns the observable callbacks, so dropping this stops the
/// process/gauge scrapes. The composition root holds it in `run()`.
pub struct MetricsHandle {
    registry: Registry,
    // Held to keep the provider (and its observable callbacks) alive.
    _provider: SdkMeterProvider,
    meter: Meter,
}

/// Initialises the process-global metrics pipeline: builds the exporter +
/// registry, installs the global meter provider, and registers the HTTP and
/// process/tokio instruments.
///
/// `route_labels` maps matched routes to their `controller`/`action` labels;
/// `runtime` is the server's tokio [`Handle`](tokio::runtime::Handle), captured
/// so the `hermit_tokio_*` callbacks can read runtime metrics off the scrape
/// path. Sampler-fed gauges are registered separately via
/// [`MetricsHandle::gauge_cell`] / [`MetricsHandle::gauge_map`].
///
/// # Errors
/// Returns [`MetricsError`] if the prometheus exporter fails to build.
pub fn init(
    route_labels: RouteLabels,
    runtime: tokio::runtime::Handle,
) -> Result<MetricsHandle, MetricsError> {
    let (registry, provider) = build_provider()?;
    opentelemetry::global::set_meter_provider(provider.clone());
    let meter = provider.meter("hermit");
    crate::http::install(HttpInstruments::new(&meter, route_labels));
    crate::process::register(&meter, runtime);
    Ok(MetricsHandle {
        registry,
        _provider: provider,
        meter,
    })
}

/// Builds the registry + OTel meter provider wired through the prometheus
/// exporter, WITHOUT touching any process-global (the meter provider / instrument
/// `OnceLock`). Split out so unit tests can build a fresh, isolated pipeline —
/// the process globals can only be set once per process.
///
/// # Errors
/// Returns [`MetricsError`] if the exporter fails to build.
pub(crate) fn build_provider() -> Result<(Registry, SdkMeterProvider), MetricsError> {
    let registry = Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        // The instrument names ARE the final prometheus names (fixture parity),
        // so disable every form of auto-suffixing / decoration.
        .without_units()
        .without_counter_suffixes()
        .without_scope_info()
        .without_target_info()
        .build()
        .map_err(|e| MetricsError::Exporter(e.to_string()))?;
    let provider = SdkMeterProvider::builder().with_reader(exporter).build();
    Ok((registry, provider))
}

impl MetricsHandle {
    /// Assembles a handle from an already-built pipeline, for in-crate tests
    /// that must avoid the process-global meter provider / instrument `OnceLock`.
    #[cfg(test)]
    pub(crate) fn for_test(registry: Registry, provider: SdkMeterProvider) -> Self {
        let meter = provider.meter("hermit");
        Self {
            registry,
            _provider: provider,
            meter,
        }
    }

    /// Renders the current exposition in Prometheus text format. `gather()`
    /// drives the exporter's collect (running every observable callback).
    #[must_use]
    pub fn render(&self) -> String {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .unwrap_or_default()
    }

    /// The `prometheus::Registry` the `/metrics` router serves.
    #[must_use]
    pub(crate) fn registry(&self) -> Registry {
        self.registry.clone()
    }

    /// The crate meter, for in-crate tests that register a probe instrument.
    #[cfg(test)]
    pub(crate) fn meter(&self) -> &Meter {
        &self.meter
    }

    /// Registers a scalar sampler-fed gauge and returns its mirror cell.
    #[must_use]
    pub fn gauge_cell(
        &self,
        name: &'static str,
        desc: &'static str,
        attrs: Vec<opentelemetry::KeyValue>,
    ) -> GaugeCell {
        crate::gauges::register_cell(&self.meter, name, desc, attrs)
    }

    /// Registers a one-label sampler-fed gauge and returns its mirror map.
    #[must_use]
    pub fn gauge_map(
        &self,
        name: &'static str,
        desc: &'static str,
        label: &'static str,
    ) -> GaugeMap {
        crate::gauges::register_map(&self.meter, name, desc, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_provider_renders_a_registered_counter() {
        // A fresh, isolated pipeline (no process globals touched).
        let (registry, provider) = build_provider().expect("exporter builds");
        let handle = MetricsHandle::for_test(registry, provider);
        let counter = handle
            .meter
            .u64_counter("test_widget_total")
            .with_description("test")
            .build();
        counter.add(3, &[]);

        let out = handle.render();
        assert!(
            out.contains("test_widget_total"),
            "counter missing from exposition:\n{out}"
        );
        // No auto-suffixing: the name is verbatim, not `test_widget_total_total`.
        assert!(!out.contains("test_widget_total_total"));
    }

    #[test]
    fn gauge_cell_and_map_render() {
        let (registry, provider) = build_provider().expect("exporter builds");
        let handle = MetricsHandle::for_test(registry, provider);
        let cell = handle.gauge_cell("test_cell", "c", vec![]);
        cell.set(7);
        let map = handle.gauge_map("test_map", "m", "kind");
        map.set(std::collections::HashMap::from([("movie".to_owned(), 4)]));

        let out = handle.render();
        assert!(out.contains("test_cell 7"), "cell missing:\n{out}");
        assert!(
            out.contains("test_map{kind=\"movie\"} 4"),
            "map missing:\n{out}"
        );
    }
}
