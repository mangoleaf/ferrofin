//! `StartupController` — the first-run setup wizard.
//!
//! Ports Jellyfin's `StartupController`, whose actions read and mutate the server
//! configuration and the first user:
//!
//! - `POST /Startup/Complete` — mark the wizard complete.
//! - `GET  /Startup/Configuration` — the wizard's server-config snapshot.
//! - `POST /Startup/Configuration` — apply the wizard's server-config changes.
//! - `POST /Startup/RemoteAccess` — toggle remote access.
//! - `GET  /Startup/User` (and `/Startup/FirstUser`) — the first user's name.
//! - `POST /Startup/User` — set the first user's name and password.
//!
//! Port notes:
//! - `[Authorize(Policy = FirstTimeSetupOrElevated)]` is not enforced by a policy
//!   middleware here; these routes are reachable during first-run exactly as the
//!   wizard needs.
//! - `RemoteAccess` writes the separate `NetworkConfiguration` store in C#. Only
//!   the main [`ServerConfiguration`] is ported at this layer, so the toggle is
//!   accepted and acknowledged (`204`) but the network-config persistence is a
//!   flagged follow-up rather than silently pretending to store elsewhere.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// The startup-wizard server-configuration DTO (`StartupConfigurationDto`).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StartupConfigurationDto {
    /// The server's display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_name: Option<String>,
    /// The UI language culture (serde name `UICulture`).
    #[serde(default, rename = "UICulture", skip_serializing_if = "Option::is_none")]
    ui_culture: Option<String>,
    /// The metadata country code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata_country_code: Option<String>,
    /// The preferred metadata language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preferred_metadata_language: Option<String>,
}

/// The remote-access toggle DTO (`StartupRemoteAccessDto`).
///
/// The field is accepted for wire compatibility but unused: the separate
/// `NetworkConfiguration` store the C# writes is not ported at this layer, so the
/// toggle is acknowledged without persisting (see the module note).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StartupRemoteAccessDto {
    /// Whether remote access is enabled.
    #[serde(default)]
    #[allow(dead_code)]
    enable_remote_access: bool,
}

/// The first-user DTO (`StartupUserDto`): name plus optional password.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StartupUserDto {
    /// The user's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// The user's password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// `POST /Startup/Complete` — mark the startup wizard complete.
///
/// Port of `StartupController.CompleteWizard`.
#[utoipa::path(
    post,
    path = "/Startup/Complete",
    responses((status = 204, description = "Startup wizard completed")),
    tag = "hermit"
)]
async fn complete_wizard(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let mut config = state.config.configuration().await?;
    config.is_startup_wizard_completed = true;
    state.config.update_configuration(&config).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Startup/Configuration` — the wizard's server-config snapshot.
///
/// Port of `StartupController.GetStartupConfiguration`.
#[utoipa::path(
    get,
    path = "/Startup/Configuration",
    responses((status = 200, description = "Startup configuration returned")),
    tag = "hermit"
)]
async fn get_startup_configuration(
    State(state): State<AppState>,
) -> Result<Json<StartupConfigurationDto>, ApiError> {
    let config = state.config.configuration().await?;
    Ok(Json(StartupConfigurationDto {
        server_name: Some(config.server_name),
        ui_culture: Some(config.ui_culture),
        metadata_country_code: Some(config.metadata_country_code),
        preferred_metadata_language: Some(config.preferred_metadata_language),
    }))
}

/// `POST /Startup/Configuration` — apply the wizard's server-config changes.
///
/// Port of `StartupController.UpdateInitialConfiguration`. Each field defaults to
/// an empty string when omitted, matching the C# `?? string.Empty`.
#[utoipa::path(
    post,
    path = "/Startup/Configuration",
    responses((status = 204, description = "Configuration saved")),
    tag = "hermit"
)]
async fn update_initial_configuration(
    State(state): State<AppState>,
    Json(body): Json<StartupConfigurationDto>,
) -> Result<StatusCode, ApiError> {
    let mut config = state.config.configuration().await?;
    config.server_name = body.server_name.unwrap_or_default();
    config.ui_culture = body.ui_culture.unwrap_or_default();
    config.metadata_country_code = body.metadata_country_code.unwrap_or_default();
    config.preferred_metadata_language = body.preferred_metadata_language.unwrap_or_default();
    state.config.update_configuration(&config).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Startup/RemoteAccess` — toggle remote access.
///
/// Port of `StartupController.SetRemoteAccess`. See the module note: the
/// `NetworkConfiguration` store is not ported, so the toggle is acknowledged
/// without persisting to that separate store.
#[utoipa::path(
    post,
    path = "/Startup/RemoteAccess",
    responses((status = 204, description = "Configuration saved")),
    tag = "hermit"
)]
async fn set_remote_access(
    State(_state): State<AppState>,
    Json(_body): Json<StartupRemoteAccessDto>,
) -> Result<StatusCode, ApiError> {
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Startup/User` (and `/Startup/FirstUser`) — the first user's name.
///
/// Port of `StartupController.GetFirstUser`: ensures at least one user exists,
/// then returns its name.
#[utoipa::path(
    get,
    path = "/Startup/User",
    responses((status = 200, description = "First user returned")),
    tag = "hermit"
)]
async fn get_first_user(State(state): State<AppState>) -> Result<Json<StartupUserDto>, ApiError> {
    state.users.initialize().await?;
    let user = state.users.get_first_user().await?.ok_or_else(|| {
        ApiError::from(hermit_traits::error::ServiceError::backend(
            "no user exists after initialization",
        ))
    })?;
    Ok(Json(StartupUserDto {
        name: Some(user.username),
        password: None,
    }))
}

/// `POST /Startup/User` — set the first user's name and password.
///
/// Port of `StartupController.UpdateStartupUser`: rejects a user that already has
/// a password (`403`), requires a non-empty password (`400`), then renames and
/// sets the password.
#[utoipa::path(
    post,
    path = "/Startup/User",
    responses(
        (status = 204, description = "First user updated"),
        (status = 400, description = "Password must not be empty"),
        (status = 403, description = "First user already has a password"),
        (status = 404, description = "No first user")
    ),
    tag = "hermit"
)]
async fn update_startup_user(
    State(state): State<AppState>,
    Json(body): Json<StartupUserDto>,
) -> Result<StatusCode, ApiError> {
    let user = state
        .users
        .get_first_user()
        .await?
        .ok_or_else(|| ApiError::NotFound("no first user".to_owned()))?;

    if user.password.is_some() {
        return Err(ApiError::Forbidden(
            "first user already has a password".to_owned(),
        ));
    }
    let password = body.password.unwrap_or_default();
    if password.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "password must not be empty".to_owned(),
        ));
    }

    let user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
    state.users.update_user(&user).await?;

    if let Some(name) = &body.name
        && !name.eq_ignore_ascii_case(&user.username)
    {
        state
            .users
            .rename_user(user_id, &user.username, name)
            .await?;
    }
    state.users.change_password(user_id, &password).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Startup/Complete", post(complete_wizard))
        .route(
            "/Startup/Configuration",
            get(get_startup_configuration).post(update_initial_configuration),
        )
        .route("/Startup/RemoteAccess", post(set_remote_access))
        .route(
            "/Startup/User",
            get(get_first_user).post(update_startup_user),
        )
        .route("/Startup/FirstUser", get(get_first_user))
}
