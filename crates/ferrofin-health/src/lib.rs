//! Lean health-check router for Ferrofin — liveness/readiness endpoints with
//! no dependency on any external health crate.
//!
//! This crate is NEW code (no C# source); it mirrors the whisper-api pattern
//! (`create_router` + health checkers). It provides:
//!
//! - [`HealthChecker`] — an `async` trait (via [`async_trait`]) for
//!   probing a single dependency: [`check`](HealthChecker::check) +
//!   [`name`](HealthChecker::name).
//! - [`FnChecker`] — a helper wrapping an `async` closure as a checker.
//! - [`health_router`] — an [`axum::Router`] mounting Jellyfin's `/health`
//!   (runs all checkers; plain-text `Healthy` / `Unhealthy`, `200` / `503`),
//!   `GET /health/live` (process-only `200`) and `GET /health/ready` (runs all
//!   checkers; `200` when all pass, `503` with the failing names otherwise).
//! - [`health_routers`] — the same routes in two parts, so a server can put
//!   Jellyfin's `/health` behind middleware its own probes stay outside.
//! - [`HealthApi`] — a [`utoipa::OpenApi`] document for the probe endpoints and
//!   their response schemas.
//!
//! # Examples
//!
//! ```
//! use std::sync::Arc;
//!
//! use ferrofin_health::{FnChecker, HealthChecker, health_router};
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
pub use router::{STARTUP_CHECK, health_router, health_routers, startup_router};

/// The build identity baked in at compile time by this crate's `build.rs`:
/// `FERROFIN_GIT_DESCRIBE` when supplied (Docker builds without `.git`), else
/// `git describe --tags --always --dirty`, else the crate version.
///
/// This is a *compile-time* value — reading it from a running process (via
/// `GET /health/live`) identifies the binary itself, which is what lets the
/// benchmark harness refuse to measure a stale build.
#[must_use]
pub fn build_version() -> &'static str {
    env!("FERROFIN_BUILD_VERSION")
}

/// [`build_version`] without the tag's `v`: `1.5.1`, or `1.5.0-12-gabc123def456`
/// between tags. The version Ferrofin names itself by in its outbound
/// `User-Agent`s and backup manifests, as Jellyfin writes `Jellyfin-Server/10.11.8`.
pub const RELEASE_VERSION: &str = env!("FERROFIN_RELEASE_VERSION");

/// The `User-Agent` Ferrofin's own HTTP requests send, `Ferrofin/1.5.1`: the
/// product header of Jellyfin's `NamedClient.Default` (`Jellyfin-Server/<version>`),
/// with [`RELEASE_VERSION`].
pub const USER_AGENT: &str = concat!("Ferrofin/", env!("FERROFIN_RELEASE_VERSION"));

use utoipa::OpenApi;

/// OpenAPI document for the health probe endpoints.
///
/// Merge this into a service's own [`utoipa::OpenApi`] so `/health`,
/// `/health/live` and `/health/ready` appear in the published spec alongside
/// the service's own paths.
#[derive(OpenApi)]
#[openapi(
    paths(health_doc, live_doc, ready_doc),
    components(schemas(LiveResponse, ReadyResponse)),
    tags((name = "health", description = "Liveness and readiness probes")),
)]
pub struct HealthApi;

/// OpenAPI stub documenting `GET /health` (the handler itself lives in
/// [`health_router`]).
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses(
        (status = 200, description = "Every check passed", body = String, content_type = "text/plain", example = "Healthy"),
        (status = 503, description = "A check failed", body = String, content_type = "text/plain", example = "Unhealthy"),
    ),
)]
#[allow(dead_code)]
fn health_doc() {}

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

    use super::{HealthApi, RELEASE_VERSION, USER_AGENT, build_version};

    /// The release version is the build's `git describe` less the tag's `v`.
    #[test]
    fn the_user_agent_names_the_build_without_its_tag_v() {
        assert_eq!(
            build_version()
                .strip_prefix('v')
                .unwrap_or_else(build_version),
            RELEASE_VERSION
        );
        assert_eq!(USER_AGENT, format!("Ferrofin/{RELEASE_VERSION}"));
    }

    #[test]
    fn openapi_spec_has_probe_paths() {
        let json = HealthApi::openapi().to_pretty_json().unwrap();
        for path in ["/health/live", "/health/ready"] {
            assert!(json.contains(path), "spec missing {path}");
        }
    }
}
