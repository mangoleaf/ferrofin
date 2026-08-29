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
//! - Every action carries C#'s `[Authorize(Policy = Policies.FirstTimeSetupOrElevated)]`
//!   (StartupController.cs:18), as the [`FirstTimeSetupOrElevated`] extractor:
//!   anonymous while the wizard is incomplete (the first-run wizard has no
//!   account to authenticate with), administrator-only once setup is complete.
//!   Ungated, `POST /Startup/User` let an anonymous caller rename the first
//!   administrator and set its password on a fully configured server.
//! - `RemoteAccess` writes the separate `NetworkConfiguration` store in C#; here
//!   that config lives in the named-config store (`named/network.json`), so the
//!   toggle persists `EnableRemoteAccess` there.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::FirstTimeSetupOrElevated;
use crate::error::ApiError;
use crate::handlers::items::user_uuid;
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
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StartupRemoteAccessDto {
    /// Whether remote access is enabled.
    #[serde(default)]
    enable_remote_access: bool,
    /// Whether UPnP automatic port mapping is enabled.
    #[serde(default)]
    enable_automatic_port_mapping: bool,
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
    tag = "ferrofin"
)]
async fn complete_wizard(
    State(state): State<AppState>,
    FirstTimeSetupOrElevated(_): FirstTimeSetupOrElevated,
) -> Result<StatusCode, ApiError> {
    let mut config = (*state.config.configuration().await?).clone();
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
    tag = "ferrofin"
)]
async fn get_startup_configuration(
    State(state): State<AppState>,
    FirstTimeSetupOrElevated(_): FirstTimeSetupOrElevated,
) -> Result<Json<StartupConfigurationDto>, ApiError> {
    let config = state.config.configuration().await?;
    Ok(Json(StartupConfigurationDto {
        server_name: Some(config.server_name.clone()),
        ui_culture: Some(config.ui_culture.clone()),
        metadata_country_code: Some(config.metadata_country_code.clone()),
        preferred_metadata_language: Some(config.preferred_metadata_language.clone()),
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
    tag = "ferrofin"
)]
async fn update_initial_configuration(
    State(state): State<AppState>,
    FirstTimeSetupOrElevated(_): FirstTimeSetupOrElevated,
    Json(body): Json<StartupConfigurationDto>,
) -> Result<StatusCode, ApiError> {
    let mut config = (*state.config.configuration().await?).clone();
    config.server_name = body.server_name.unwrap_or_default();
    config.ui_culture = body.ui_culture.unwrap_or_default();
    config.metadata_country_code = body.metadata_country_code.unwrap_or_default();
    config.preferred_metadata_language = body.preferred_metadata_language.unwrap_or_default();
    state.config.update_configuration(&config).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Startup/RemoteAccess` — toggle remote access.
///
/// Port of `StartupController.SetRemoteAccess`: persists `EnableRemoteAccess`
/// onto the `NetworkConfiguration` — and only that, as upstream does. Ferrofin
/// keeps the config in the named-config store (`{config}/named/network.json`,
/// the same file `GET/POST /System/Configuration/network` reads), so the wizard
/// toggle actually takes effect rather than being dropped.
///
/// Note the shape: this reads the whole document, changes one field, and writes
/// the whole document back. Every field therefore has to survive
/// deserialization, or this handler quietly resets it — which is why
/// `NetworkConfiguration` carries `#[serde(default)]` and aliases for the names
/// older versions wrote.
#[utoipa::path(
    post,
    path = "/Startup/RemoteAccess",
    responses((status = 204, description = "Configuration saved")),
    tag = "ferrofin"
)]
async fn set_remote_access(
    State(state): State<AppState>,
    FirstTimeSetupOrElevated(_): FirstTimeSetupOrElevated,
    Json(body): Json<StartupRemoteAccessDto>,
) -> Result<StatusCode, ApiError> {
    let path = crate::handlers::config::named_config_file(&state, "network").ok_or_else(|| {
        ApiError::from(ferrofin_traits::error::ServiceError::backend(
            "network config path unavailable",
        ))
    })?;
    // Load the persisted network config (or its defaults), set the flag, save.
    // Every settle-for-defaults branch here overwrites the operator's whole
    // document a few lines below, so none of them may be silent. Upstream
    // substitutes defaults too, but logs first
    // (`BaseConfigurationManager.LoadConfiguration`).
    let mut config = match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "the saved network configuration could not be read; it is being replaced \
                     with defaults"
                );
                ferrofin_networking::NetworkConfiguration::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ferrofin_networking::NetworkConfiguration::default()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "the saved network configuration could not be read; it is being replaced \
                 with defaults"
            );
            ferrofin_networking::NetworkConfiguration::default()
        }
    };
    config.enable_remote_access = body.enable_remote_access;
    // Accepted and dropped, exactly as upstream does it: `SetRemoteAccess`
    // (`Jellyfin.Api/Controllers/StartupController.cs:94`) assigns
    // `EnableRemoteAccess` and nothing else, so the wizard's port-mapping
    // checkbox never reaches `EnableUPnP` there either.
    let _ = body.enable_automatic_port_mapping;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            ApiError::from(ferrofin_traits::error::ServiceError::backend(e.to_string()))
        })?;
    }
    let json = serde_json::to_vec_pretty(&config).map_err(|e| {
        ApiError::from(ferrofin_traits::error::ServiceError::backend(e.to_string()))
    })?;
    tokio::fs::write(&path, json).await.map_err(|e| {
        ApiError::from(ferrofin_traits::error::ServiceError::backend(e.to_string()))
    })?;
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
    tag = "ferrofin"
)]
async fn get_first_user(
    State(state): State<AppState>,
    FirstTimeSetupOrElevated(_): FirstTimeSetupOrElevated,
) -> Result<Json<StartupUserDto>, ApiError> {
    state.users.initialize().await?;
    let user = state.users.get_first_user().await?.ok_or_else(|| {
        ApiError::from(ferrofin_traits::error::ServiceError::backend(
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
    tag = "ferrofin"
)]
async fn update_startup_user(
    State(state): State<AppState>,
    FirstTimeSetupOrElevated(_): FirstTimeSetupOrElevated,
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

    let user_id = user_uuid(&user)?;
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
