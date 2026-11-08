//! `LdapController` — the LDAP-Auth plugin's settings-page routes.
//!
//! The HTTP seam of the **LDAP-Auth extension**
//! (`ferrofin-extensions::ldap_auth`, upstream `jellyfin-plugin-ldapauth` v24,
//! `d11c86a`). The configuration, the directory client and the tests
//! themselves live with the extension; this handler is the thin seam over the
//! [`LdapManager`] trait:
//! - `POST /Ldap/TestServerBind` — save the connection settings and test them;
//! - `POST /Ldap/TestLdapFilters` — save the user and admin filters and count
//!   what each finds;
//! - `POST /Ldap/LdapUserSearch` — save the search attributes and find the
//!   entry a name signs in as.
//!
//! Not Jellyfin contract operations, so they are listed in
//! [`PLUGIN_ROUTES`](super::PLUGIN_ROUTES) rather than in `REAL_ROUTES` or
//! `EXTENSION_ROUTES` (both hold contract operations only), and carry no
//! `VERIFIED` row: their comparison with the plugin is recorded in
//! `docs/PLUGINS_UPSTREAM.md`. The path layer matches them ignoring case, since
//! the settings page calls `ldap/TestServerBind` and the others so.
//!
//! Administrators only, as upstream's `[Authorize(Policy =
//! Policies.RequiresElevation)]` controller: each body is saved into the plugin
//! configuration, and the connection test's carries the bind password. While
//! the plugin is disabled (or the extension is not wired) every route answers
//! `404` whatever the body holds, a body that does not bind included: the
//! observable behaviour of a Jellyfin server whose disabled plugin's controller
//! is not registered.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router, extract::State};
use ferrofin_model::ldap::{
    LdapFilterResponse, LdapTestErrorResponse, ServerConnectionInfo, ServerTestResponse,
    UserFilterInfo, UserSearchAttributes, UserSearchResponse,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::ldap::{LdapManager, LdapTestAnswer};
use serde::Serialize;

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

/// `POST /Ldap/TestLdapFilters` — saves the user and admin filters into the
/// plugin configuration, then counts the entries each finds under the base
/// DN, the admin filter's only while it is on.
///
/// Port of `LdapController.TestLdapFilters`. A body the binder cannot read is
/// a `400` validation problem. A directory that cannot be reached, or refuses
/// the bind DN, is a `401`; a search it fails, a filter that does not parse
/// included, is a `400` whose `Message` starts `User Filter: ` or
/// `Admin Filter: `. Both carry upstream's `LdapTestErrorResponse`, which the
/// settings page shows.
#[utoipa::path(
    post,
    path = "/Ldap/TestLdapFilters",
    request_body = UserFilterInfo,
    responses(
        (status = 200, description = "Filters were queried", body = LdapFilterResponse),
        (status = 400, description = "Body is missing required data or filter is invalid", body = LdapTestErrorResponse),
        (status = 401, description = "Failed to connect to LDAP server", body = LdapTestErrorResponse),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The LDAP-Auth plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn test_ldap_filters(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    body: Result<JsonBody<UserFilterInfo>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let ldap = service(&state)?;
    ldap.ensure_enabled().await?;
    let filters = match body {
        Ok(JsonBody(filters)) => filters,
        Err(problem) => return Ok(problem.into_response()),
    };
    Ok(answer(ldap.test_ldap_filters(filters).await?))
}

/// `POST /Ldap/LdapUserSearch` — saves the search attributes into the plugin
/// configuration, then, when `TestSearchUsername` is given, finds the entry
/// that name signs in as.
///
/// Port of `LdapController.LdapUserSearch`. Without a name the answer is an
/// empty `200`. A body the binder cannot read is a `400` validation problem.
/// A directory that cannot be reached or refuses the bind DN, a search it
/// fails, and a name that finds no entry are each a `401` carrying upstream's
/// `LdapTestErrorResponse`.
#[utoipa::path(
    post,
    path = "/Ldap/LdapUserSearch",
    request_body = UserSearchAttributes,
    responses(
        (status = 200, description = "No test requested or test completed", body = UserSearchResponse),
        (status = 400, description = "Body is missing required data"),
        (status = 401, description = "Failed to connect to LDAP server or user filter is invalid", body = LdapTestErrorResponse),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The LDAP-Auth plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn ldap_user_search(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    body: Result<JsonBody<UserSearchAttributes>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let ldap = service(&state)?;
    ldap.ensure_enabled().await?;
    let search = match body {
        Ok(JsonBody(search)) => search,
        Err(problem) => return Ok(problem.into_response()),
    };
    Ok(answer(ldap.ldap_user_search(search).await?))
}

/// A test's answer as `LdapController` returns it: `Ok(result)`, or
/// `BadRequest`/`Unauthorized` with an `LdapTestErrorResponse`.
fn answer<T: Serialize>(outcome: LdapTestAnswer<T>) -> Response {
    match outcome {
        LdapTestAnswer::Ok(result) => Json(result).into_response(),
        LdapTestAnswer::BadRequest(error) => (StatusCode::BAD_REQUEST, Json(error)).into_response(),
        LdapTestAnswer::Unauthorized(error) => {
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

/// Registers this controller's routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Ldap/TestServerBind", post(test_server_bind))
        .route("/Ldap/TestLdapFilters", post(test_ldap_filters))
        .route("/Ldap/LdapUserSearch", post(ldap_user_search))
}
