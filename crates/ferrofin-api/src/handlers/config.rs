//! `ConfigurationController` — server configuration read/write.
//!
//! Ports the `[Route("System")]` configuration actions:
//! - `GET`/`POST /System/Configuration` — the strongly-typed [`ServerConfiguration`].
//! - `GET /System/Configuration/MetadataOptions/Default` — a default [`MetadataOptions`].
//! - `POST /System/Configuration/Branding` — update the branding config.
//! - `GET`/`POST /System/Configuration/{key}` — a *named* configuration.
//!
//! Named configurations are Jellyfin's pluggable per-key config store, and a
//! key is one only while a store is registered under it: `branding`,
//! `encoding`, `network`, `metadata`, `xbmcmetadata`, `livetv` and `database`.
//! Any other key is not found on read (`404`) and fails on save (`500`), as
//! upstream. `branding`
//! has a dedicated typed store; the others use per-key files at
//! `{config}/users/named/{key}.json`, except `database`, which is SQLite and
//! read-only. They bind through typed JsonDefaults converters before
//! persistence and return typed documents on reads.
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

use crate::auth::{RequireAdminWithDefault, RequireAuth, is_administrator_identity};
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
    _auth: RequireAdminWithDefault,
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
async fn get_default_metadata_options(_auth: RequireAdminWithDefault) -> Json<MetadataOptions> {
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
    _auth: RequireAdminWithDefault,
    JsonBody(dto): JsonBody<BrandingOptionsDto>,
) -> Result<StatusCode, ApiError> {
    let mut current = state.config.get_branding().await?;
    current.login_disclaimer = dto.login_disclaimer;
    current.custom_css = dto.custom_css;
    current.splashscreen_enabled = dto.splashscreen_enabled;
    state.config.update_branding(&current).await?;
    run_committed_checks(&state, "branding")?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /System/Configuration/{key}` — a named configuration.
///
/// Port of `ConfigurationController.GetNamedConfiguration`: the configuration
/// manager's `GetConfiguration(key)`
/// ([`ServerConfigurationManager::get_named_configuration`]), which binds the
/// stored document to its type, reads one that does not parse as its
/// defaults, and fails on one it cannot read. A key no store is registered
/// under is `404`, as `GetConfiguration`'s `ResourceNotFoundException` is.
///
/// [`ServerConfigurationManager::get_named_configuration`]: ferrofin_traits::configuration::ServerConfigurationManager::get_named_configuration
#[utoipa::path(
    get,
    path = "/System/Configuration/{key}",
    params(("key" = String, Path, description = "Configuration key")),
    responses((status = 200, description = "Configuration returned")),
    tag = "ferrofin"
)]
async fn get_named_configuration(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // Typed here, not through the document: the numbers JsonDefaults cannot
    // write must answer 400, not come back as `null`.
    if key.eq_ignore_ascii_case("encoding") {
        return encoding_response(&state.config.get_encoding_options().await?).map(Json);
    }
    let Some(mut value) = state.config.get_named_configuration(&key).await? else {
        return Err(ApiError::NotFound(format!(
            "Configuration with key {key} not found."
        )));
    };
    // `livetv` merges the persisted scalars (recording paths, padding) with the
    // canonical tuner/provider rows the Live TV manager keeps in SQLite — the
    // dashboard's Live TV page renders its tuner and guide-provider lists from
    // this config object, so they must reflect the manager's state.
    if key.eq_ignore_ascii_case("livetv")
        && let (Some(live_tv), Value::Object(map)) = (state.live_tv.as_ref(), &mut value)
    {
        let to_value = |r: Result<Value, serde_json::Error>| {
            r.map_err(|e| {
                ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
                    "serialize configuration `{key}`: {e}"
                )))
            })
        };
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
    // Divergence (owner decision 2026-10-23): upstream's plain [Authorize]
    // hands these credentials to every signed-in user, and only the
    // dashboard's admin pages read them. Blanked on the document itself, so
    // the lists a dashboard save left in `livetv.json` are covered too.
    if key.eq_ignore_ascii_case("livetv") && !is_administrator_identity(&state, &auth.0).await? {
        blank_live_tv_credentials(&mut value);
    }
    Ok(Json(value))
}

/// Blanks the credentials in a `livetv` document: each tuner's `Url` (an IPTV
/// playlist's usually carries the account's user name and password) and each
/// listing provider's `Password` (Schedules Direct accepts the stored hash as
/// the credential). A member the document does not have stays absent.
fn blank_live_tv_credentials(document: &mut Value) {
    for (list, member) in [("TunerHosts", "Url"), ("ListingProviders", "Password")] {
        if let Some(Value::Array(rows)) = document.get_mut(list) {
            for row in rows {
                if let Some(credential @ Value::String(_)) = row.get_mut(member) {
                    *credential = Value::String(String::new());
                }
            }
        }
    }
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
        // Jellyfin's `database` store picks the database provider for the next
        // start. Ferrofin runs on SQLite only, so a saved choice could never be
        // honoured; refuse it rather than keep a setting that does nothing.
        "database" => Err(ApiError::BadRequest(
            "Ferrofin runs on SQLite only, so the database configuration cannot be changed."
                .to_owned(),
        )),
        // `GetConfigurationType` looks the store up with `First()`, which throws
        // `InvalidOperationException` for a key none is registered under; the
        // exception middleware answers that 500.
        _ => Err(ferrofin_traits::error::ServiceError::backend(format!(
            "no configuration store is registered under the key {key}"
        ))
        .into()),
    }
}

/// `POST /System/Configuration/{key}` — update a named configuration.
///
/// Port of `ConfigurationController.UpdateNamedConfiguration` (elevation-gated).
/// Registered keys bind through their configuration DTO with exact member
/// names and JsonDefaults value conversion before being saved. `database` is
/// refused (`400`), and a key no store is registered under fails (`500`), as
/// upstream's store lookup does.
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
    _auth: RequireAdminWithDefault,
    Path(key): Path<String>,
    JsonValueBody(raw): JsonValueBody<Box<serde_json::value::RawValue>>,
) -> Result<StatusCode, ApiError> {
    let body = normalize_named_configuration(&key, raw.get())?;
    if key.eq_ignore_ascii_case("branding") {
        let branding: BrandingOptions =
            serde_json::from_value(body).map_err(|error| configuration_error(&error))?;
        state.config.update_branding(&branding).await?;
        run_committed_checks(&state, &key)?;
        return Ok(StatusCode::NO_CONTENT);
    }
    // Once accepted, finish persistence and the live update even if the HTTP
    // client disconnects. The worker owns the save lock until both are done.
    tokio::spawn(save_named_configuration(state, key, body))
        .await
        .map_err(|error| {
            ferrofin_traits::error::ServiceError::backend(format!("configuration worker: {error}"))
        })??;
    Ok(StatusCode::NO_CONTENT)
}

/// `BaseConfigurationManager.SaveConfiguration(key, …)` for a store the API
/// writes: validation, the atomic write under the save lock, the live network
/// policy for `network`, the configuration listeners, then the committed
/// checks. `body` is the document as it is saved.
pub(crate) async fn save_named_configuration(
    state: AppState,
    key: String,
    body: Value,
) -> Result<(), ApiError> {
    use ferrofin_traits::error::ServiceError;
    let path = named_config_file(&state, &key)
        .ok_or_else(|| ApiError::BadRequest("invalid configuration key".to_owned()))?;
    let _save = state.configuration_write_lock.lock().await;
    // Parse everything needed by the live consumer before replacing the file.
    let network = if key.eq_ignore_ascii_case("network") {
        Some(
            serde_json::from_value::<ferrofin_networking::NetworkConfiguration>(body.clone())
                .map_err(|error| configuration_error(&error))?,
        )
    } else {
        None
    };
    let json = serde_json::to_string_pretty(&body).map_err(|error| configuration_error(&error))?;
    if let Some(validator) = state
        .configuration_validators
        .get(&key.to_ascii_lowercase())
    {
        // Validate against the document GET serves, as `SaveConfiguration`
        // passes `GetConfiguration(key)`: a malformed old document reads as its
        // defaults, so it stays repairable.
        let old = state
            .config
            .get_named_configuration(&key)
            .await?
            .unwrap_or_else(|| serde_json::json!({}));
        let old = serde_json::to_string(&old).map_err(|error| configuration_error(&error))?;
        validator.validate(&old, &json).map_err(validation_error)?;
    }
    let committed: std::sync::Arc<str> = json.clone().into();
    tokio::task::spawn_blocking(move || {
        ferrofin_util::file_helper::atomic_write(&path, json.as_bytes())
    })
    .await
    .map_err(|error| ServiceError::backend(format!("configuration writer: {error}")))?
    .map_err(|error| ServiceError::backend(format!("persist configuration `{key}`: {error}")))?;
    if let Some(config) = network {
        state.update_network_settings(&config);
    }
    state.config.named_configuration_updated(&key, committed);
    run_committed_checks(&state, &key)
}

/// Runs the checks registered for after a named save (Jellyfin's
/// `NamedConfigurationUpdated` handlers). An error answers the save, which
/// stays committed.
pub(crate) fn run_committed_checks(state: &AppState, key: &str) -> Result<(), ApiError> {
    for check in &state.committed_configuration_checks {
        check.check(key).map_err(validation_error)?;
    }
    Ok(())
}

/// A rejected or failed save as Jellyfin's exception middleware answers it:
/// a missing directory or file is `404`, anything else `500`.
fn validation_error(
    error: ferrofin_common::configuration::ConfigurationValidationError,
) -> ferrofin_traits::error::ServiceError {
    use ferrofin_common::configuration::ConfigurationValidationError;
    use ferrofin_traits::error::ServiceError;
    match error {
        ConfigurationValidationError::DirectoryNotFound(message)
        | ConfigurationValidationError::FileNotFound(message) => ServiceError::not_found(message),
        ConfigurationValidationError::InvalidOperation(message) => ServiceError::backend(message),
    }
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
