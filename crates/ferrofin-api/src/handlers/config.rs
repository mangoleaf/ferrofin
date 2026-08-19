//! `ConfigurationController` — server configuration read/write.
//!
//! Ports the `[Route("System")]` configuration actions:
//! - `GET`/`POST /System/Configuration` — the strongly-typed [`ServerConfiguration`].
//! - `GET /System/Configuration/MetadataOptions/Default` — a default [`MetadataOptions`].
//! - `POST /System/Configuration/Branding` — update the branding config.
//! - `GET`/`POST /System/Configuration/{key}` — a *named* configuration.
//!
//! Named configurations are Jellyfin's pluggable per-key config store. `branding`
//! has a dedicated typed store; every other key (`encoding`/`network`/`metadata`/
//! `xbmcmetadata` and any plugin key) round-trips through a generic per-key store
//! at `{config}/named/{key}.json` — `POST` persists the JSON verbatim, `GET`
//! returns it (or a typed default object for the known core keys until saved).
//!
//! Every route is `[Authorize]` (writes additionally `RequiresElevation`), which
//! collapses to authentication at this layer via [`RequireAuth`].

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::branding::{BrandingOptions, BrandingOptionsDto};
use ferrofin_model::configuration::{MetadataOptions, ServerConfiguration};
use serde_json::Value;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The on-disk file backing a persisted named configuration, or `None` when
/// `key` is not a safe single filename segment.
///
/// `key` comes straight from the URL, so this is the path-traversal guard: only
/// `[A-Za-z0-9_-]` is allowed (rejecting `..`, `/`, `.`), and the file lives in a
/// dedicated `named/` subdir of the configuration directory.
pub(crate) fn named_config_file(state: &AppState, key: &str) -> Option<std::path::PathBuf> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let dir = state
        .config
        .application_paths()
        .user_configuration_directory_path();
    Some(
        std::path::Path::new(&dir)
            .join("named")
            .join(format!("{}.json", key.to_ascii_lowercase())),
    )
}

/// `GET /System/Configuration` — the current server configuration.
///
/// Port of `ConfigurationController.GetConfiguration`.
#[utoipa::path(
    get,
    path = "/System/Configuration",
    responses((status = 200, description = "Application configuration returned", body = ServerConfiguration)),
    tag = "ferrofin"
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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

/// Reads the persisted named configuration at `path`, or `None` when nothing
/// usable is stored there.
///
/// Port of `BaseConfigurationManager.LoadConfiguration`, which wraps its
/// deserialize in `catch { Logger.LogError(ex, "Error loading configuration
/// file: {Path}", path) }` and then falls back to `Activator.CreateInstance` —
/// the response shape stays the typed default, but the failure is **logged**.
/// A missing file is the ordinary "never saved" case (upstream's `File.Exists`
/// guard) and stays silent; a read error or invalid JSON is a broken store that
/// would otherwise be indistinguishable from it, making an admin's saved
/// settings look reset to defaults with nothing to look at.
async fn read_named_config(path: &std::path::Path) -> Option<Value> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "reading named configuration failed; falling back to defaults"
            );
            return None;
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "named configuration is not valid JSON; falling back to defaults"
            );
            None
        }
    }
}

/// `GET /System/Configuration/{key}` — a named configuration.
///
/// Port of `ConfigurationController.GetNamedConfiguration`. `branding` keeps its
/// dedicated typed store; every other key round-trips through the generic
/// per-key store (`{config}/named/{key}.json`), falling back to a typed default
/// object for the known core sections (`encoding`/`network`/`metadata`/
/// `xbmcmetadata`) when nothing has been saved. An unknown, never-saved key is
/// still `501`.
#[utoipa::path(
    get,
    path = "/System/Configuration/{key}",
    params(("key" = String, Path, description = "Configuration key")),
    responses((status = 200, description = "Configuration returned")),
    tag = "ferrofin"
)]
async fn get_named_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    use ferrofin_model::configuration::{MetadataConfiguration, XbmcMetadataOptions};
    let to_value = |r: Result<Value, serde_json::Error>| {
        r.map_err(|e| {
            ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
                "serialize configuration `{key}`: {e}"
            )))
        })
    };
    if key.eq_ignore_ascii_case("branding") {
        return Ok(Json(to_value(serde_json::to_value(
            state.config.get_branding().await?,
        ))?));
    }
    // A previously-saved value wins over the default object.
    let saved = match named_config_file(&state, &key) {
        Some(path) => read_named_config(&path).await,
        None => None,
    };
    // `livetv` merges the persisted scalars (recording paths, padding) with the
    // canonical tuner/provider rows the Live TV manager keeps in SQLite — the
    // dashboard's Live TV page renders its tuner and guide-provider lists from
    // this config object, so they must reflect the manager's state.
    if key.eq_ignore_ascii_case("livetv") {
        let mut value = match saved {
            Some(v) => v,
            None => to_value(serde_json::to_value(
                ferrofin_model::live_tv::LiveTvOptions::default(),
            ))?,
        };
        if let (Some(live_tv), Value::Object(map)) = (state.live_tv.as_ref(), &mut value) {
            // A backend failure must not read as "no tuners configured": the
            // rows still render empty (Jellyfin's corrupt-store fallback also
            // yields an empty `LiveTvOptions`), but the failure is logged
            // instead of vanishing.
            let tuners = match live_tv.get_tuner_hosts().await {
                Ok(tuners) => tuners,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "listing Live TV tuner hosts failed; reporting none configured"
                    );
                    Vec::new()
                }
            };
            let providers = match live_tv.get_listing_providers().await {
                Ok(providers) => providers,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "listing Live TV listing providers failed; reporting none configured"
                    );
                    Vec::new()
                }
            };
            map.insert(
                "TunerHosts".to_owned(),
                to_value(serde_json::to_value(tuners))?,
            );
            map.insert(
                "ListingProviders".to_owned(),
                to_value(serde_json::to_value(providers))?,
            );
        }
        return Ok(Json(value));
    }
    if let Some(value) = saved {
        return Ok(Json(value));
    }
    let value = match key.to_ascii_lowercase().as_str() {
        "encoding" => to_value(serde_json::to_value(
            state.config.get_encoding_options().await?,
        ))?,
        "network" => to_value(serde_json::to_value(
            ferrofin_networking::NetworkConfiguration::default(),
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
/// `branding` updates its dedicated typed store; every other key is persisted
/// verbatim to the generic per-key store (`{config}/named/{key}.json`), so the
/// dashboard's config pages round-trip.
#[utoipa::path(
    post,
    path = "/System/Configuration/{key}",
    params(("key" = String, Path, description = "Configuration key")),
    request_body = Object,
    responses((status = 204, description = "Named configuration updated")),
    tag = "ferrofin"
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
    let path = named_config_file(&state, &key)
        .ok_or_else(|| ApiError::BadRequest("invalid configuration key".to_owned()))?;
    let io_err = |e: &std::io::Error| {
        ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
            "persist configuration `{key}`: {e}"
        )))
    };
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| io_err(&e))?;
    }
    let bytes = serde_json::to_vec_pretty(&body).map_err(|e| {
        ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
            "serialize configuration `{key}`: {e}"
        )))
    })?;
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| io_err(&e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /System/Configuration/Branding` — the branding configuration.
///
/// Jellyfin serves branding through `GET /System/Configuration/{key}` (key
/// `branding`), but our static `POST /System/Configuration/Branding` route shadows
/// that exact path for *every* method — axum resolves the path first, then the
/// method, so a GET here 405s without a handler while ASP.NET's method-aware routing
/// falls through to the `{key}` action. Serving the same branding object here
/// restores the Jellyfin behavior (a client's `GET .../branding` returns 200).
async fn get_branding_configuration(
    State(state): State<AppState>,
    _auth: RequireAuth,
) -> Result<Json<Value>, ApiError> {
    serde_json::to_value(state.config.get_branding().await?)
        .map(Json)
        .map_err(|e| {
            ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
                "serialize branding configuration: {e}"
            )))
        })
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
            get(get_branding_configuration).post(update_branding_configuration),
        )
        .route(
            "/System/Configuration/{key}",
            get(get_named_configuration).post(update_named_configuration),
        )
}
