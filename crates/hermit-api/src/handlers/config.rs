//! `ConfigurationController` — server configuration read/write.
//!
//! Ports the `[Route("System")]` configuration actions:
//! - `GET`/`POST /System/Configuration` — the strongly-typed [`ServerConfiguration`].
//! - `GET /System/Configuration/MetadataOptions/Default` — a default [`MetadataOptions`].
//! - `POST /System/Configuration/Branding` — update the branding config.
//! - `GET`/`POST /System/Configuration/{key}` — a *named* configuration.
//!
//! Named configurations are Jellyfin's pluggable per-key config store. Hermit
//! serves the core keys the dashboard reads — `branding` + `encoding` from
//! storage, `network`/`metadata`/`xbmcmetadata` as default objects; plugin-owned
//! keys stay on the `501` stub. The write side currently persists only
//! `branding` (other keys `501`).
//!
//! Every route is `[Authorize]` (writes additionally `RequiresElevation`), which
//! collapses to authentication at this layer via [`RequireAuth`].

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::branding::{BrandingOptions, BrandingOptionsDto};
use hermit_model::configuration::{MetadataOptions, ServerConfiguration};
use serde_json::Value;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /System/Configuration` — the current server configuration.
///
/// Port of `ConfigurationController.GetConfiguration`.
#[utoipa::path(
    get,
    path = "/System/Configuration",
    responses((status = 200, description = "Application configuration returned", body = ServerConfiguration)),
    tag = "hermit"
)]
async fn get_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
) -> Result<Json<ServerConfiguration>, ApiError> {
    Ok(Json(state.config.configuration().await?))
}

/// `POST /System/Configuration` — replace the server configuration.
///
/// Port of `ConfigurationController.UpdateConfiguration` (elevation-gated).
#[utoipa::path(
    post,
    path = "/System/Configuration",
    request_body = ServerConfiguration,
    responses((status = 204, description = "Configuration updated")),
    tag = "hermit"
)]
async fn update_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Json(configuration): Json<ServerConfiguration>,
) -> Result<StatusCode, ApiError> {
    state.config.update_configuration(&configuration).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /System/Configuration/MetadataOptions/Default` — a default [`MetadataOptions`].
///
/// Port of `ConfigurationController.GetDefaultMetadataOptions`; returns a fresh
/// `new MetadataOptions()` (all defaults).
#[utoipa::path(
    get,
    path = "/System/Configuration/MetadataOptions/Default",
    responses((status = 200, description = "Metadata options returned", body = MetadataOptions)),
    tag = "hermit"
)]
async fn get_default_metadata_options(_auth: RequireAuth) -> Json<MetadataOptions> {
    Json(MetadataOptions::default())
}

/// `POST /System/Configuration/Branding` — update the branding configuration.
///
/// Port of `ConfigurationController.UpdateBrandingConfiguration`: reads the
/// current branding to preserve `SplashscreenLocation`, overlays the DTO's three
/// editable fields, and persists.
#[utoipa::path(
    post,
    path = "/System/Configuration/Branding",
    request_body = BrandingOptionsDto,
    responses((status = 204, description = "Branding configuration updated")),
    tag = "hermit"
)]
async fn update_branding_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Json(dto): Json<BrandingOptionsDto>,
) -> Result<StatusCode, ApiError> {
    let mut current = state.config.get_branding().await?;
    current.login_disclaimer = dto.login_disclaimer;
    current.custom_css = dto.custom_css;
    current.splashscreen_enabled = dto.splashscreen_enabled;
    state.config.update_branding(&current).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /System/Configuration/{key}` — a named configuration.
///
/// Port of `ConfigurationController.GetNamedConfiguration`. The dashboard reads
/// these core sections on load, so each returns its stored value (`branding`,
/// `encoding`) or Jellyfin's default object (`network`, `metadata`,
/// `xbmcmetadata`). Plugin-owned keys stay on the `501` stub.
///
/// ponytail: `network`/`metadata`/`xbmcmetadata` return defaults — enough to
/// render + populate the config pages; wire persisted round-trips when the
/// matching `POST` is ported.
#[utoipa::path(
    get,
    path = "/System/Configuration/{key}",
    params(("key" = String, Path, description = "Configuration key")),
    responses((status = 200, description = "Configuration returned")),
    tag = "hermit"
)]
async fn get_named_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    use hermit_model::configuration::{MetadataConfiguration, XbmcMetadataOptions};
    let to_value = |r: Result<Value, serde_json::Error>| {
        r.map_err(|e| {
            ApiError::from(hermit_traits::error::ServiceError::backend(format!(
                "serialize configuration `{key}`: {e}"
            )))
        })
    };
    let value = match key.to_ascii_lowercase().as_str() {
        "branding" => to_value(serde_json::to_value(state.config.get_branding().await?))?,
        "encoding" => to_value(serde_json::to_value(
            state.config.get_encoding_options().await?,
        ))?,
        "network" => to_value(serde_json::to_value(
            hermit_networking::NetworkConfiguration::default(),
        ))?,
        "metadata" => to_value(serde_json::to_value(MetadataConfiguration::default()))?,
        "xbmcmetadata" => to_value(serde_json::to_value(XbmcMetadataOptions::default()))?,
        _ => return Err(ApiError::NotImplemented),
    };
    Ok(Json(value))
}

/// `POST /System/Configuration/{key}` — update a named configuration.
///
/// Port of `ConfigurationController.UpdateNamedConfiguration` (elevation-gated).
/// Only `branding` is backed by real storage; other keys return `501`.
#[utoipa::path(
    post,
    path = "/System/Configuration/{key}",
    params(("key" = String, Path, description = "Configuration key")),
    request_body = Object,
    responses((status = 204, description = "Named configuration updated")),
    tag = "hermit"
)]
async fn update_named_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, ApiError> {
    if key.eq_ignore_ascii_case("branding") {
        let branding: BrandingOptions = serde_json::from_value(body).map_err(|_| {
            ApiError::BadRequest("Body doesn't contain a valid configuration".to_owned())
        })?;
        state.config.update_branding(&branding).await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    Err(ApiError::NotImplemented)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/System/Configuration",
            get(get_configuration).post(update_configuration),
        )
        .route(
            "/System/Configuration/MetadataOptions/Default",
            get(get_default_metadata_options),
        )
        .route(
            "/System/Configuration/Branding",
            axum::routing::post(update_branding_configuration),
        )
        .route(
            "/System/Configuration/{key}",
            get(get_named_configuration).post(update_named_configuration),
        )
}
