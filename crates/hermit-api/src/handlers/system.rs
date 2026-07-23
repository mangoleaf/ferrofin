//! `SystemController` — server information endpoints.
//!
//! Port of the `Info` and `Info/Public` actions: the full [`SystemInfo`] (behind
//! auth) and the anonymous [`PublicSystemInfo`]. Both delegate to the
//! [`SystemManager`](hermit_traits::system::SystemManager), building its
//! [`RequestContext`] from the request parts.

use axum::extract::State;
use axum::http::request::Parts;
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::system::{PublicSystemInfo, SystemInfo};
use hermit_traits::net::RequestContext;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Builds a [`RequestContext`] from an axum request's [`Parts`] (headers +
/// query), mirroring the auth middleware's construction so the system manager
/// sees the same request view.
fn context_from_parts(parts: &Parts) -> RequestContext {
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    RequestContext {
        headers,
        query_string: parts.uri.query().map(ToOwned::to_owned),
        remote_endpoint: None,
    }
}

/// `GET /System/Info` — the full system information for an authenticated client.
///
/// Port of `SystemController.GetSystemInfo`. Requires a valid token (Jellyfin's
/// `FirstTimeSetupOrIgnoreParentalControl` policy collapses to "authenticated"
/// at this layer).
#[utoipa::path(
    get,
    path = "/System/Info",
    responses((status = 200, description = "System info returned", body = SystemInfo)),
    tag = "hermit"
)]
async fn get_system_info(
    State(state): State<AppState>,
    _auth: RequireAuth,
    parts: Parts,
) -> Result<Json<SystemInfo>, ApiError> {
    let ctx = context_from_parts(&parts);
    let info = state.system.get_system_info(&ctx).await?;
    Ok(Json(info))
}

/// `GET /System/Info/Public` — the anonymous, public system information.
///
/// Port of `SystemController.GetPublicSystemInfo`. Never requires auth.
#[utoipa::path(
    get,
    path = "/System/Info/Public",
    responses((status = 200, description = "Public system info returned", body = PublicSystemInfo)),
    tag = "hermit"
)]
async fn get_public_system_info(
    State(state): State<AppState>,
    parts: Parts,
) -> Result<Json<PublicSystemInfo>, ApiError> {
    let ctx = context_from_parts(&parts);
    let info = state.system.get_public_system_info(&ctx).await?;
    Ok(Json(info))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/System/Info", get(get_system_info))
        .route("/System/Info/Public", get(get_public_system_info))
}
