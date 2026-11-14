//! HTTP/REST API for Ferrofin — port of `Jellyfin.Api`.
//!
//! axum router + handlers mirroring Jellyfin's controllers. Handlers depend only
//! on the `ferrofin-traits` manager traits via [`AppState`] (`Arc<dyn Trait>`) —
//! never on `ferrofin-core` directly; the concrete impls are injected at the
//! composition root (`ferrofin-server`, Wave 8).
//!
//! **Contract:** every path in the vendored Jellyfin 10.11.8 OpenAPI spec is
//! registered as a route so a client never 404s on a known path (the vendored
//! table is [`VENDORED_ROUTES`]). Every contract operation has a real handler
//! (listed in [`handlers::REAL_ROUTES`]); a contract route without one would
//! return `501 Not Implemented` via the shared [`routes::not_implemented`]
//! handler, never `404`. The test `tests/contract_superset.rs` asserts the
//! registered route table is a superset of the vendored spec.
//!
//! Beside the [`handlers`], the crate holds [`AppState`], [`error::ApiError`],
//! [`auth`] (token middleware + `RequireAuth`), [`router::create_router`], the
//! shared `not_implemented` stub, and the utoipa [`openapi::ApiDoc`].

pub mod auth;
pub mod compression;
mod contract_routes;
pub mod cors;
pub mod error;
pub mod extract;
pub mod forwarded_headers;
pub mod handlers;
pub mod ip_access;
pub mod openapi;
pub mod panic_boundary;
mod query;
mod request_culture;
pub mod response_time;
pub mod router;
pub mod routes;
pub mod state;

#[cfg(any(test, feature = "test-util"))]
pub mod test_support;

pub use error::{ApiError, ProblemStatus};
pub use router::{create_router, startup_layers};
pub use state::{AppState, Inner};

/// Re-export the generated vendored contract table under [`routes`].
pub use contract_routes::VENDORED_ROUTES;
