//! `GET /Ferrofin/Settings` and `POST /Ferrofin/Settings` — the Ferrofin
//! settings page's view of the server's bootstrap settings (CLI flags,
//! `FERROFIN_*` environment variables, `config.toml`), and its saves to
//! `config.toml`.
//!
//! The HTTP seam of the **Ferrofin** extension
//! (`ferrofin-extensions::ferrofin_settings`); the settings themselves live in
//! the server binary, which owns the configuration, behind the
//! [`ServerSettingsManager`] trait. Not Jellyfin contract operations, so the
//! routes are registered here and listed in neither `REAL_ROUTES` nor
//! `EXTENSION_ROUTES` (both hold contract operations only), like the runtime
//! plugins' `/Plugins/{pluginId}/web/{*path}`.
//!
//! Administrators only, since the answer holds the providers' keys and the
//! seed admin password in plain text, and a save changes how the server
//! starts, as Jellyfin's plugin configuration routes do
//! (`RequiresElevation`). While the Ferrofin plugin is disabled both answer
//! `404`, like a disabled plugin's controller.
//!
//! [`ServerSettingsManager`]: ferrofin_traits::server_settings::ServerSettingsManager

use axum::routing::get;
use axum::{Json, Router, extract::State};
use ferrofin_model::server_settings::{
    ServerSettingsDto, ServerSettingsUpdateDto, ServerSettingsUpdateResultDto,
};

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::state::AppState;

/// `GET /Ferrofin/Settings` — every Ferrofin bootstrap setting with its
/// running value, default and source, and whether the settings page may
/// change it.
#[utoipa::path(
    get,
    path = "/Ferrofin/Settings",
    responses(
        (status = 200, description = "The Ferrofin settings", body = ServerSettingsDto),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Ferrofin plugin is disabled"),
        (status = 409, description = "config.toml changed since the server started and no longer loads")
    ),
    tag = "ferrofin"
)]
async fn get_settings(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Json<ServerSettingsDto>, ApiError> {
    Ok(Json(state.server_settings.snapshot().await?))
}

/// `POST /Ferrofin/Settings` — saves the settings page's changes to
/// `config.toml` (a value sets a key, `null` removes it) and marks a restart
/// pending. All or nothing: a refused change writes nothing.
#[utoipa::path(
    post,
    path = "/Ferrofin/Settings",
    request_body = ServerSettingsUpdateDto,
    responses(
        (status = 200, description = "What was saved", body = ServerSettingsUpdateResultDto),
        (status = 400, description = "An unknown key, a setting the page may not change, or an invalid value; nothing was written"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "The Ferrofin plugin is disabled"),
        (status = 409, description = "config.toml or its directory cannot be written, or the file no longer loads; nothing was written")
    ),
    tag = "ferrofin"
)]
async fn update_settings(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
    JsonBody(update): JsonBody<ServerSettingsUpdateDto>,
) -> Result<Json<ServerSettingsUpdateResultDto>, ApiError> {
    Ok(Json(state.server_settings.update(update).await?))
}

/// Registers the Ferrofin settings routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/Ferrofin/Settings",
        get(get_settings).post(update_settings),
    )
}
