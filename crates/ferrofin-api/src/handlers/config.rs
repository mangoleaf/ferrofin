//! `ConfigurationController` — server configuration read/write.
//!
//! Ports the `[Route("System")]` configuration actions:
//! - `GET`/`POST /System/Configuration` — the strongly-typed [`ServerConfiguration`].
//! - `GET /System/Configuration/MetadataOptions/Default` — a default [`MetadataOptions`].
//! - `POST /System/Configuration/Branding` — update the branding config.
//! - `GET`/`POST /System/Configuration/{key}` — a *named* configuration.
//!
//! Named configurations are Jellyfin's pluggable per-key config store. `branding`
//! has a dedicated typed store; other keys use per-key files at
//! `{config}/users/named/{key}.json`. Core sections bind through typed
//! JsonDefaults converters before persistence and return typed documents on
//! reads. Plugin-owned documents keep their raw shape.
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

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::{JsonBody, JsonValueBody};
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
    // The seam hands out a shared handle; this endpoint owns its response body,
    // so it is one of the few callers that deep-clones the document.
    Ok(Json((*state.config.configuration().await?).clone()))
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
    _auth: RequireAdmin,
    JsonBody(configuration): JsonBody<ServerConfiguration>,
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
async fn get_default_metadata_options(_auth: RequireAdmin) -> Json<MetadataOptions> {
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
    _auth: RequireAdmin,
    JsonBody(dto): JsonBody<BrandingOptionsDto>,
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

/// Loads a core section through its configuration DTO. Older
/// documents may omit members; deserialization supplies their defaults. Like
/// `BaseConfigurationManager.LoadConfiguration`, an invalid stored document
/// yields defaults and a diagnostic, without overwriting the damaged file.
fn load_named_configuration<T: serde::de::DeserializeOwned + Default>(
    key: &str,
    saved: Option<Value>,
) -> T {
    let Some(saved) = saved else {
        return T::default();
    };
    match serde_json::from_value(saved) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                configuration_key = key,
                error = %error,
                "saved named configuration could not be read; falling back to defaults"
            );
            T::default()
        }
    }
}

/// `GET /System/Configuration/{key}` — a named configuration.
///
/// Port of `ConfigurationController.GetNamedConfiguration`. `branding` keeps its
/// dedicated typed store; other core keys load their DTO from the per-key
/// store (`{config}/users/named/{key}.json`). Omitted members receive defaults,
/// and invalid documents fall back with a warning. Plugin-owned keys retain
/// their raw shape; an unknown, never-saved key is still `501`.
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
    use ferrofin_model::configuration::{
        EncodingOptions, MetadataConfiguration, XbmcMetadataOptions,
    };
    use ferrofin_model::live_tv::LiveTvOptions;
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
    // Load once: retrying through a manager after a failed file read would
    // turn the logged fallback into another parse error on the same file.
    let saved = match named_config_file(&state, &key) {
        Some(path) => read_named_config(&path).await,
        None => None,
    };
    if key.eq_ignore_ascii_case("encoding") {
        let options = load_named_configuration::<EncodingOptions>(&key, saved);
        return encoding_response(&options).map(Json);
    }
    // `livetv` merges the persisted scalars (recording paths, padding) with the
    // canonical tuner/provider rows the Live TV manager keeps in SQLite — the
    // dashboard's Live TV page renders its tuner and guide-provider lists from
    // this config object, so they must reflect the manager's state.
    if key.eq_ignore_ascii_case("livetv") {
        let options = load_named_configuration::<LiveTvOptions>(&key, saved);
        let mut value = to_value(serde_json::to_value(options))?;
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
    let value = match key.to_ascii_lowercase().as_str() {
        // The DTO's aliases also read the old EnableIpv4/RemoteIpFilter names;
        // serialization always returns the names understood by jellyfin-web.
        "network" => to_value(serde_json::to_value(load_named_configuration::<
            ferrofin_networking::NetworkConfiguration,
        >(&key, saved)))?,
        "metadata" => to_value(serde_json::to_value(load_named_configuration::<
            MetadataConfiguration,
        >(&key, saved)))?,
        "xbmcmetadata" => to_value(serde_json::to_value(load_named_configuration::<
            XbmcMetadataOptions,
        >(&key, saved)))?,
        _ => saved.ok_or(ApiError::NotImplemented)?,
    };
    Ok(Json(value))
}

/// A JsonException thrown inside UpdateNamedConfiguration reaches Jellyfin's
/// exception middleware as 500; MVC syntax errors are still rejected as 400
/// before the controller runs.
fn configuration_error(error: &serde_json::Error) -> ApiError {
    ferrofin_traits::error::ServiceError::backend(format!("invalid named configuration: {error}"))
        .into()
}

// JsonDefaults reads named floating-point literals but cannot write them.
// Jellyfin persists these in XML; its response serialization throws an
// ArgumentException, which the exception middleware maps to HTTP 400.
fn encoding_response(
    options: &ferrofin_model::configuration::EncodingOptions,
) -> Result<Value, ApiError> {
    if [
        options.down_mix_audio_boost,
        options.tonemapping_desat,
        options.tonemapping_peak,
        options.tonemapping_param,
        options.vpp_tonemapping_brightness,
        options.vpp_tonemapping_contrast,
    ]
    .iter()
    .any(|value| !value.is_finite())
    {
        return Err(ApiError::BadRequest(
            "Non-finite numbers cannot be written as JSON.".into(),
        ));
    }
    serde_json::to_value(options).map_err(|error| configuration_error(&error))
}

fn normalize_named_configuration(key: &str, raw: &str) -> Result<Value, ApiError> {
    fn typed<T: serde::de::DeserializeOwned + serde::Serialize>(
        raw: &str,
    ) -> Result<Value, ApiError> {
        let config: T = crate::extract::deserialize_defaults(raw)
            .map_err(|error| configuration_error(&error))?;
        serde_json::to_value(config).map_err(|error| configuration_error(&error))
    }
    use ferrofin_model::configuration::{
        EncodingOptions, MetadataConfiguration, XbmcMetadataOptions,
    };
    match key.to_ascii_lowercase().as_str() {
        "branding" => typed::<BrandingOptions>(raw),
        "encoding" => typed::<EncodingOptions>(raw),
        "network" => typed::<ferrofin_networking::NetworkConfiguration>(raw),
        "metadata" => typed::<MetadataConfiguration>(raw),
        "xbmcmetadata" => typed::<XbmcMetadataOptions>(raw),
        "livetv" => typed::<ferrofin_model::live_tv::LiveTvOptions>(raw),
        // Plugin-owned documents have no core DTO: retain their existing store.
        _ => serde_json::from_str(raw).map_err(|error| configuration_error(&error)),
    }
}

/// `POST /System/Configuration/{key}` — update a named configuration.
///
/// Port of `ConfigurationController.UpdateNamedConfiguration` (elevation-gated).
/// Known keys bind through their configuration DTO with exact member names
/// and JsonDefaults value conversion before being saved. Plugin-owned keys
/// retain their generic JSON store.
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
    _auth: RequireAdmin,
    Path(key): Path<String>,
    JsonValueBody(raw): JsonValueBody<Box<serde_json::value::RawValue>>,
) -> Result<StatusCode, ApiError> {
    let body = normalize_named_configuration(&key, raw.get())?;
    if key.eq_ignore_ascii_case("branding") {
        let branding: BrandingOptions =
            serde_json::from_value(body).map_err(|error| configuration_error(&error))?;
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
    // The network policy caches its parsed subnets, so a saved
    // `LocalNetworkSubnets` / `RemoteIPFilter` has to be pushed into it or it
    // would keep enforcing the configuration the server booted with until a
    // restart. C# `NetworkManager` subscribes to the same configuration event.
    if key.eq_ignore_ascii_case("network") {
        match serde_json::from_value::<ferrofin_networking::NetworkConfiguration>(body) {
            Ok(config) => state.update_network_settings(&config),
            Err(e) => tracing::warn!(
                error = %e,
                "the saved network configuration could not be read back; the running policy is unchanged"
            ),
        }
    }
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
