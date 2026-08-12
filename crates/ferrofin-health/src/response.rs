//! Serializable response bodies for the liveness and readiness probes.

use serde::Serialize;
use utoipa::ToSchema;

/// Body returned by `GET /health/live`.
///
/// Liveness is process-only by contract: it never touches dependencies, so its
/// only field is a constant status string.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LiveResponse {
    /// Always `"ok"` — the process is up and serving.
    #[schema(example = "ok")]
    pub status: &'static str,
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
