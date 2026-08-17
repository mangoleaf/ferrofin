//! Serializable response bodies for the liveness and readiness probes.

use serde::Serialize;
use utoipa::ToSchema;

/// Body returned by `GET /health/live`.
///
/// Liveness is process-only by contract: it never touches dependencies, so its
/// fields are compile-time constants: a status string and the build identity.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LiveResponse {
    /// Always `"ok"` — the process is up and serving.
    #[schema(example = "ok")]
    pub status: &'static str,
    /// The build identity baked in at compile time ([`crate::build_version`]).
    /// Lets an operator — or the benchmark harness — verify *which* binary is
    /// serving, independent of any runtime configuration.
    #[schema(example = "v0.5.0-3-gabc123def456")]
    pub build: &'static str,
}

/// Body returned by `GET /health/ready`.
///
/// `status` is `"ready"` when every checker passed and `"not_ready"` when one or
/// more failed. `failing` lists the [`name`](crate::HealthChecker::name)s of the
/// checkers that returned `Err`, empty when all passed.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReadyResponse {
    /// `"ready"` when all checkers passed, otherwise `"not_ready"`.
    #[schema(example = "ready")]
    pub status: &'static str,
    /// Names of the checkers that failed; empty on success.
    #[schema(example = json!([]))]
    pub failing: Vec<String>,
}
