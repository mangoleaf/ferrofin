//! `LdapController` — the LDAP-Auth plugin's settings-page routes.
//!
//! The HTTP seam of the **LDAP-Auth extension**
//! (`ferrofin-extensions::ldap_auth`, upstream `jellyfin-plugin-ldapauth` v24,
//! `d11c86a`). The configuration, the directory client and the tests
//! themselves live with the extension; this handler is the thin seam over the
//! [`LdapManager`] trait:
//! - `POST /Ldap/TestServerBind` — save the connection settings and test them.
//!
//! Not a Jellyfin contract operation, so it is listed in
//! [`PLUGIN_ROUTES`](super::PLUGIN_ROUTES) rather than in `REAL_ROUTES` or
//! `EXTENSION_ROUTES` (both hold contract operations only), and carries no
//! `VERIFIED` row: its comparison with the plugin is recorded in
//! `docs/PLUGINS_UPSTREAM.md`. The path layer matches it ignoring case, since
//! the settings page calls `ldap/TestServerBind`.
//!
//! Administrators only, as upstream's `[Authorize(Policy =
//! Policies.RequiresElevation)]` controller: the body carries the bind
//! password and is saved into the plugin configuration. While the plugin is
//! disabled (or the extension is not wired) the route answers `404` whatever
//! the body holds, a body that does not bind included: the observable
//! behaviour of a Jellyfin server whose disabled plugin's controller is not
//! registered.

use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router, extract::State};
use ferrofin_model::ldap::{ServerConnectionInfo, ServerTestResponse};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::ldap::LdapManager;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::{JsonBody, ProblemDetails};
use crate::state::AppState;

/// Resolves the wired LDAP-Auth service, or the `404` an absent plugin
/// controller produces.
fn service(state: &AppState) -> Result<Arc<dyn LdapManager>, ApiError> {
    state
        .ldap
        .clone()
        .ok_or_else(|| ServiceError::not_found("the LDAP-Auth plugin is not available").into())
}

/// `POST /Ldap/TestServerBind` — saves the connection settings into the
/// plugin configuration, then connects, binds and searches the base DN with
/// them, reporting each step.
///
/// Port of `LdapController.TestServerBind`. A body the binder cannot read, or
/// one without `LdapServer` or `LdapBaseDn` (upstream's `[Required]`), is a
/// `400` validation problem, as ASP.NET's `[ApiController]` answers. A step
/// that fails is part of the `200` answer, never an error status.
#[utoipa::path(
    post,
    path = "/Ldap/TestServerBind",
    request_body = ServerConnectionInfo,
    responses(
        (status = 200, description = "Server connection was tested", body = ServerTestResponse),
        (status = 400, description = "Body is missing required data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The LDAP-Auth plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn test_server_bind(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    body: Result<JsonBody<ServerConnectionInfo>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let ldap = service(&state)?;
    ldap.ensure_enabled().await?;
    let connection = match body {
        Ok(JsonBody(connection)) => connection,
        Err(problem) => return Ok(problem.into_response()),
    };
    if let Some(problem) = ProblemDetails::members_validation(connection.missing_required()) {
        return Ok(problem.into_response());
    }
    Ok(Json(ldap.test_server_bind(connection).await?).into_response())
}

/// Registers this controller's routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Ldap/TestServerBind", post(test_server_bind))
}
