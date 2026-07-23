//! HTTP/REST API for Hermit — port of `Jellyfin.Api`.
//!
//! axum router + handlers mirroring Jellyfin's controllers. Handlers depend only
//! on the `hermit-traits` manager traits via [`AppState`] (`Arc<dyn Trait>`) —
//! never on `hermit-core` directly; the concrete impls are injected at the
//! composition root (`hermit-server`, Wave 8).
//!
//! **Contract:** every path in the vendored Jellyfin 10.11.8 OpenAPI spec is
//! registered as a route so a client never 404s on a known path (the vendored
//! table is [`VENDORED_ROUTES`]). First-Light and core routes have real
//! handlers; un-ported routes return `501 Not Implemented` via the shared
//! [`routes::not_implemented`] handler. The test `tests/contract_superset.rs`
//! asserts the registered route table is a superset of the vendored spec. See
//! `brain/PLAN_HERMIT_PORT.md`.
//!
//! This crate is the INFRA layer landed by Wave 7 unit 1: [`AppState`],
//! [`error::ApiError`], [`auth`] (token middleware + `RequireAuth`),
//! [`router::create_router`], the shared `not_implemented` stub, and the
//! utoipa [`openapi::ApiDoc`].

pub mod auth;
mod contract_routes;
pub mod error;
pub mod handlers;
pub mod openapi;
pub mod router;
pub mod routes;
pub mod state;

#[cfg(any(test, feature = "test-util"))]
pub mod test_support;

pub use error::ApiError;
pub use router::create_router;
pub use state::{AppState, Inner};

/// Re-export the generated vendored contract table under [`routes`].
pub use contract_routes::VENDORED_ROUTES;
