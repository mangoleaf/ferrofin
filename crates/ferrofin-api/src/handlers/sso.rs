//! The Single Sign-On plugin's provider settings routes.
//!
//! The HTTP seam of the **Single Sign-On extension**
//! (`ferrofin-extensions::sso`), Ferrofin's own OIDC sign-in plugin. The
//! configuration, the secret store and the checks live with the extension;
//! this handler is the thin seam over the [`SsoManager`] trait:
//! - `GET /sso/admin/oidc/providers` — the providers, each with whether a
//!   client secret is stored and its callback URL, never a secret;
//! - `PUT /sso/admin/oidc/providers/{provider}` — create or update one; a body
//!   without `ClientSecret` keeps the stored secret;
//! - `DELETE /sso/admin/oidc/providers/{provider}` — remove one and its secret.
//!
//! Not Jellyfin contract operations, so they are listed in
//! [`PLUGIN_ROUTES`](super::PLUGIN_ROUTES) rather than in `REAL_ROUTES` or
//! `EXTENSION_ROUTES` (both hold contract operations only), and carry no
//! `VERIFIED` row: there is no upstream to compare them with (D2, D15).
//!
//! Administrators only. While the plugin is disabled, or the extension is not
//! wired, every route answers `404` to every caller, before it looks at the
//! credentials or the body: a server without the plugin has no such route
//! (B1).

use std::sync::Arc;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use ferrofin_model::sso::{OidcProvider, OidcProviderInfo, OidcProviderList};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::sso::SsoManager;

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::{JsonBody, ProblemDetails};
use crate::state::AppState;

/// The wired Single Sign-On service while its plugin is enabled, or the `404`
/// a server without the plugin answers.
async fn enabled_service(state: &AppState) -> Result<Arc<dyn SsoManager>, ApiError> {
    let sso = state.sso.clone().ok_or_else(|| {
        ApiError::from(ServiceError::not_found(
            "the Single Sign-On plugin is not available",
        ))
    })?;
    sso.ensure_enabled().await?;
    Ok(sso)
}

/// `GET /sso/admin/oidc/providers` — the plugin's public URL setting and its
/// OIDC providers, each with whether a client secret is stored, its callback
/// URL when a public URL is known, and why it is refused when it is. No answer
/// holds a client secret.
#[utoipa::path(
    get,
    path = "/sso/admin/oidc/providers",
    responses(
        (status = 200, description = "The OIDC providers", body = OidcProviderList),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn list_oidc_providers(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
) -> Result<Json<OidcProviderList>, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    Ok(Json(sso.oidc_providers().await?))
}

/// `PUT /sso/admin/oidc/providers/{provider}` — creates the provider
/// `{provider}`, or replaces its settings, and answers it as the list shows
/// it.
///
/// A body without `ClientSecret`, or with it empty or blank, keeps the
/// stored secret; a provider with none stored needs one. A refused setting is
/// a `400` naming it; a body that does not bind is a `400` validation
/// problem.
#[utoipa::path(
    put,
    path = "/sso/admin/oidc/providers/{provider}",
    params(("provider" = String, Path, description = "The provider's key: 1 to 32 lowercase letters, digits or hyphens")),
    request_body = OidcProvider,
    responses(
        (status = 200, description = "The provider was saved", body = OidcProviderInfo),
        (status = 400, description = "A setting is refused, or the body does not bind"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn save_oidc_provider(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
    provider: Result<Path<String>, PathRejection>,
    body: Result<JsonBody<OidcProvider>, ProblemDetails>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    let provider = match provider {
        Ok(Path(provider)) => provider,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    let settings = match body {
        Ok(JsonBody(settings)) => settings,
        Err(problem) => return Ok(problem.into_response()),
    };
    Ok(Json(sso.save_oidc_provider(&provider, settings).await?).into_response())
}

/// `DELETE /sso/admin/oidc/providers/{provider}` — removes the provider and
/// its client secret.
#[utoipa::path(
    delete,
    path = "/sso/admin/oidc/providers/{provider}",
    params(("provider" = String, Path, description = "The provider's key")),
    responses(
        (status = 204, description = "The provider was removed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "No such provider, or the Single Sign-On plugin is disabled or not available")
    ),
    tag = "ferrofin"
)]
async fn delete_oidc_provider(
    State(state): State<AppState>,
    admin: Result<RequireAdmin, ApiError>,
    provider: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let sso = enabled_service(&state).await?;
    admin?;
    let Path(provider) = match provider {
        Ok(path) => path,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    sso.delete_oidc_provider(&provider).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Registers this controller's routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/sso/admin/oidc/providers", get(list_oidc_providers))
        .route(
            "/sso/admin/oidc/providers/{provider}",
            put(save_oidc_provider).delete(delete_oidc_provider),
        )
}
