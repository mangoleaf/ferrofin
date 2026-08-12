//! Lean health-check router for Hermit — liveness/readiness endpoints with
//! no dependency on any external health crate.
//!
//! This crate is NEW code (no C# source); it mirrors the whisper-api pattern
//! (`create_router` + health checkers). It provides:
//!
//! - [`HealthChecker`] — an `async` trait (via [`async_trait`](async_trait)) for
//!   probing a single dependency: [`check`](HealthChecker::check) +
//!   [`name`](HealthChecker::name).
//! - [`FnChecker`] — a helper wrapping an `async` closure as a checker.
//! - [`health_router`] — an [`axum::Router`] mounting `GET /health/live`
//!   (process-only `200`) and `GET /health/ready` (runs all checkers; `200` when
//!   all pass, `503` with the failing names otherwise).
//! - [`HealthApi`] — a [`utoipa::OpenApi`] document for the probe endpoints and
//!   their response schemas.
//!
//! # Examples
//!
//! ```
//! use std::sync::Arc;
//!
//! use hermit_health::{FnChecker, HealthChecker, health_router};
//!
//! let checkers: Vec<Arc<dyn HealthChecker>> = vec![
//!     Arc::new(FnChecker::new("database", || async { Ok(()) })),
//!     Arc::new(FnChecker::new("storage", || async { Ok(()) })),
//! ];
//! let router = health_router(checkers);
//! # let _ = router;
//! ```

mod checker;
mod response;
mod router;

pub use checker::{FnChecker, HealthChecker};
pub use response::{LiveResponse, ReadyResponse};
pub use router::health_router;

use utoipa::OpenApi;

/// OpenAPI document for the health probe endpoints.
///
/// Merge this into a service's own [`utoipa::OpenApi`] so `/health/live` and
/// `/health/ready` appear in the published spec alongside the service's own
/// paths.
#[derive(OpenApi)]
#[openapi(
    paths(live_doc, ready_doc),
    components(schemas(LiveResponse, ReadyResponse)),
    tags((name = "health", description = "Liveness and readiness probes")),
)]
pub struct HealthApi;

/// OpenAPI stub documenting `GET /health/live` (the handler itself lives in
/// [`health_router`]).
#[utoipa::path(
    get,
    path = "/health/live",
    tag = "health",
    responses((status = 200, description = "Process is up", body = LiveResponse)),
)]
#[allow(dead_code)]
fn live_doc() {}

/// OpenAPI stub documenting `GET /health/ready` (the handler itself lives in
/// [`health_router`]).
#[utoipa::path(
    get,
    path = "/health/ready",
    tag = "health",
    responses(
        (status = 200, description = "All checkers passed", body = ReadyResponse),
        (status = 503, description = "One or more checkers failed", body = ReadyResponse),
    ),
)]
#[allow(dead_code)]
fn ready_doc() {}

#[cfg(test)]
mod tests {
    use utoipa::OpenApi;

    use super::HealthApi;

    #[test]
    fn openapi_spec_has_probe_paths() {
        let json = HealthApi::openapi().to_pretty_json().unwrap();
        for path in ["/health/live", "/health/ready"] {
            assert!(json.contains(path), "spec missing {path}");
        }
    }
}
