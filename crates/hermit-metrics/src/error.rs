//! The crate error type.

/// Errors raised while initialising the metrics pipeline.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// The OpenTelemetry → Prometheus exporter failed to build. Held as a string
    /// so the type is robust to the exporter's concrete error type across
    /// otel/prometheus point releases.
    #[error("failed to build the prometheus exporter: {0}")]
    Exporter(String),
}
