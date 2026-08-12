//! `PluginsController` + `PackageController` — the Tier-1 plugin-manager surface.
//!
//! Ports the plugin/package/repository API over the
//! [`PluginManager`](hermit_traits::plugins::PluginManager) seam
//! (`AppState::plugins`), backed by the registry of **compile-time** plugins the
//! composition root registers (see `docs/PLUGINS_UPSTREAM.md`). Reads
//! (`GetPlugins`, config, repositories, image, manifest) and the enable/disable +
//! repository-set mutators are real; the operations that need a *runtime* plugin
//! host — installing a package and uninstalling a compiled-in plugin — return an
//! honest rejection rather than faking success (never a `501`).
//!
//! - `GET /Plugins` — installed plugins
//! - `GET|POST /Plugins/{id}/Configuration` — read/write a plugin's config JSON
//! - `POST /Plugins/{id}/{version}/{Enable,Disable}` — toggle a plugin
//! - `DELETE /Plugins/{id}` / `/Plugins/{id}/{version}` — uninstall (rejected: compiled-in)
//! - `GET /Plugins/{id}/{version}/Image` — a plugin's bundled image
//! - `POST /Plugins/{id}/Manifest` — a plugin's manifest (read; `GetPluginManifest`)
//! - `GET /Repositories`, `POST /Repositories` — package-repository list
//! - `GET /Packages`, `GET /Packages/{name}` — available packages (empty catalog)
//! - `POST /Packages/Installed/{name}` — install (rejected: no runtime host)
//! - `DELETE /Packages/Installing/{packageId}` — cancel an install (none active)

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use hermit_model::plugins::{PluginInfo, PluginStatus};
use hermit_model::updates::{PackageInfo, RepositoryInfo};
use hermit_traits::plugins::PluginDescriptor;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Projects a manager [`PluginDescriptor`] into the `PluginInfo` wire DTO.
///
/// The `enabled` flag becomes `Active`/`Disabled` (the only two states a
/// compile-time plugin reaches — `Malfunctioned`/`NotSupported`/… require the
/// runtime loader we don't have).
fn to_plugin_info(d: PluginDescriptor) -> PluginInfo {
    PluginInfo {
        name: d.name,
        version: d.version,
        configuration_file_name: None,
        description: d.description,
        id: d.id,
        can_uninstall: d.can_uninstall,
        has_image: d.has_image,
        status: if d.enabled {
            PluginStatus::Active
        } else {
            PluginStatus::Disabled
        },
    }
}

/// `GET /Plugins` — list the installed plugins.
#[utoipa::path(
    get,
    path = "/Plugins",
    responses((status = 200, description = "Installed plugins returned", body = [PluginInfo])),
    tag = "hermit"
)]
async fn get_plugins(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<PluginInfo>>, ApiError> {
    let plugins = state.plugins.list_plugins().await?;
    Ok(Json(plugins.into_iter().map(to_plugin_info).collect()))
}

/// `GET /Plugins/{pluginId}/Configuration` — a plugin's stored configuration.
///
/// Returns the opaque config JSON (`{}` until set); `404` when the plugin is not
/// installed.
#[utoipa::path(
    get,
    path = "/Plugins/{pluginId}/Configuration",
    params(("pluginId" = String, Path, description = "Plugin id")),
    responses(
        (status = 200, description = "Configuration returned"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn get_plugin_configuration(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let bytes = state.plugins.get_plugin_configuration(plugin_id).await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

/// `POST /Plugins/{pluginId}/Configuration` — replace a plugin's configuration.
///
/// The body is the plugin's opaque config object (validated as JSON). `404` when
/// the plugin is not installed, `400` when the body is not valid JSON.
#[utoipa::path(
    post,
    path = "/Plugins/{pluginId}/Configuration",
    params(("pluginId" = String, Path, description = "Plugin id")),
    request_body = String,
    responses(
        (status = 204, description = "Configuration updated"),
        (status = 400, description = "Body is not valid JSON"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn update_plugin_configuration(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    state
        .plugins
        .set_plugin_configuration(plugin_id, body.to_vec())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Plugins/{pluginId}/{version}/Enable` — enable a plugin.
#[utoipa::path(
    post,
    path = "/Plugins/{pluginId}/{version}/Enable",
    params(
        ("pluginId" = String, Path, description = "Plugin id"),
        ("version" = String, Path, description = "Plugin version")
    ),
    responses(
        (status = 204, description = "Plugin enabled"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn enable_plugin(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((plugin_id, _version)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    state.plugins.enable_plugin(plugin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Plugins/{pluginId}/{version}/Disable` — disable a plugin.
#[utoipa::path(
    post,
    path = "/Plugins/{pluginId}/{version}/Disable",
    params(
        ("pluginId" = String, Path, description = "Plugin id"),
        ("version" = String, Path, description = "Plugin version")
    ),
    responses(
        (status = 204, description = "Plugin disabled"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn disable_plugin(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((plugin_id, _version)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    state.plugins.disable_plugin(plugin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Plugins/{pluginId}` — uninstall a plugin.
///
/// Compiled-in plugins cannot be removed at runtime, so a known plugin yields
/// `400` (and an unknown one `404`) — never a faked success.
#[utoipa::path(
    delete,
    path = "/Plugins/{pluginId}",
    params(("pluginId" = String, Path, description = "Plugin id")),
    responses(
        (status = 400, description = "Compiled-in plugin cannot be uninstalled"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn uninstall_plugin(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state.plugins.remove_plugin(plugin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Plugins/{pluginId}/{version}` — uninstall a plugin by version.
#[utoipa::path(
    delete,
    path = "/Plugins/{pluginId}/{version}",
    params(
        ("pluginId" = String, Path, description = "Plugin id"),
        ("version" = String, Path, description = "Plugin version")
    ),
    responses(
        (status = 400, description = "Compiled-in plugin cannot be uninstalled"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn uninstall_plugin_by_version(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((plugin_id, _version)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    state.plugins.remove_plugin(plugin_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Plugins/{pluginId}/{version}/Image` — a plugin's bundled image.
#[utoipa::path(
    get,
    path = "/Plugins/{pluginId}/{version}/Image",
    params(
        ("pluginId" = String, Path, description = "Plugin id"),
        ("version" = String, Path, description = "Plugin version")
    ),
    responses(
        (status = 200, description = "Image returned"),
        (status = 404, description = "Plugin or image not found")
    ),
    tag = "hermit"
)]
async fn get_plugin_image(
    State(state): State<AppState>,
    Path((plugin_id, _version)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    match state.plugins.plugin_image(plugin_id).await? {
        Some(image) => {
            Ok(([(header::CONTENT_TYPE, image.content_type)], image.data).into_response())
        }
        None => Err(ApiError::NotFound(format!("image for plugin {plugin_id}"))),
    }
}

/// `POST /Plugins/{pluginId}/Manifest` — read a plugin's manifest.
///
/// Jellyfin models `GetPluginManifest` as a `POST`; it is a read. Returns a
/// manifest projected from the descriptor, or `404` when the plugin is unknown.
#[utoipa::path(
    post,
    path = "/Plugins/{pluginId}/Manifest",
    params(("pluginId" = String, Path, description = "Plugin id")),
    responses(
        (status = 200, description = "Manifest returned"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "hermit"
)]
async fn get_plugin_manifest(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(d) = state.plugins.get_plugin(plugin_id).await? else {
        return Err(ApiError::NotFound(format!("plugin {plugin_id}")));
    };
    // The vendored contract does not pin a manifest schema; project the fields the
    // descriptor carries (a faithful read for a compiled-in plugin).
    Ok(Json(serde_json::json!({
        "Id": d.id,
        "Name": d.name,
        "Version": d.version,
        "Description": d.description,
        "Status": if d.enabled { "Active" } else { "Disabled" },
    })))
}

/// `GET /Repositories` — the configured package repositories.
#[utoipa::path(
    get,
    path = "/Repositories",
    responses((status = 200, description = "Repositories returned", body = [RepositoryInfo])),
    tag = "hermit"
)]
async fn get_repositories(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<RepositoryInfo>>, ApiError> {
    Ok(Json(state.plugins.get_repositories().await?))
}

/// `POST /Repositories` — replace the configured package repositories.
#[utoipa::path(
    post,
    path = "/Repositories",
    request_body = [RepositoryInfo],
    responses((status = 204, description = "Repositories updated")),
    tag = "hermit"
)]
async fn set_repositories(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Json(repositories): Json<Vec<RepositoryInfo>>,
) -> Result<StatusCode, ApiError> {
    state.plugins.set_repositories(repositories).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Packages` — packages available from the enabled repositories.
///
/// Tier-1 does not fetch repository manifests, so the catalog is empty (faithful,
/// never a faked package).
#[utoipa::path(
    get,
    path = "/Packages",
    responses((status = 200, description = "Available packages returned", body = [PackageInfo])),
    tag = "hermit"
)]
async fn get_packages(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<PackageInfo>>, ApiError> {
    Ok(Json(state.plugins.list_packages().await?))
}

/// `GET /Packages/{name}` — a package by name or assembly GUID.
///
/// Port of `PackageController.GetPackageInfo`: looks the package up in the
/// aggregated repository catalog by (case-insensitive) name, or by `?assemblyGuid=`
/// when supplied. `404` when the catalog has no match.
#[utoipa::path(
    get,
    path = "/Packages/{name}",
    params(("name" = String, Path, description = "Package name")),
    responses(
        (status = 200, description = "Package returned", body = PackageInfo),
        (status = 404, description = "Package not found")
    ),
    tag = "hermit"
)]
async fn get_package_info(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(name): Path<String>,
    Query(query): Query<PackageInfoQuery>,
) -> Result<Json<PackageInfo>, ApiError> {
    let guid = query.assembly_guid.as_deref().filter(|g| !g.is_empty());
    let package = state
        .plugins
        .list_packages()
        .await?
        .into_iter()
        .find(|p| {
            p.name.eq_ignore_ascii_case(&name)
                && guid.is_none_or(|g| p.id.to_string().eq_ignore_ascii_case(g))
        })
        .ok_or_else(|| ApiError::NotFound(format!("package {name}")))?;
    Ok(Json(package))
}

/// Query parameters for `GET /Packages/{name}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageInfoQuery {
    /// Optional assembly GUID to disambiguate same-named packages.
    #[serde(default)]
    assembly_guid: Option<String>,
}

/// `POST /Packages/Installed/{name}` — install a package.
///
/// Runtime installation needs a dynamic plugin host (Tier 2); compiled-in plugins
/// cannot be installed at runtime, so this is an honest `400` rather than a faked
/// success.
#[utoipa::path(
    post,
    path = "/Packages/Installed/{name}",
    params(("name" = String, Path, description = "Package name")),
    responses((status = 400, description = "Runtime installation is not supported")),
    tag = "hermit"
)]
async fn install_package(
    State(_state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(_name): Path<String>,
) -> Result<StatusCode, ApiError> {
    Err(ApiError::BadRequest(
        "runtime plugin installation is not supported; plugins are compiled in".to_owned(),
    ))
}

/// `DELETE /Packages/Installing/{packageId}` — cancel a running install.
///
/// No installs run (Tier-1 has no installer), so there is nothing to cancel.
#[utoipa::path(
    delete,
    path = "/Packages/Installing/{packageId}",
    params(("packageId" = String, Path, description = "Package id")),
    responses((status = 404, description = "No such active installation")),
    tag = "hermit"
)]
async fn cancel_package_installation(
    State(_state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(package_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    Err(ApiError::NotFound(format!("installation {package_id}")))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Plugins", get(get_plugins))
        .route(
            "/Plugins/{pluginId}/Configuration",
            get(get_plugin_configuration).post(update_plugin_configuration),
        )
        .route("/Plugins/{pluginId}/{version}/Enable", post(enable_plugin))
        .route(
            "/Plugins/{pluginId}/{version}/Disable",
            post(disable_plugin),
        )
        .route("/Plugins/{pluginId}", delete(uninstall_plugin))
        .route(
            "/Plugins/{pluginId}/{version}",
            delete(uninstall_plugin_by_version),
        )
        .route("/Plugins/{pluginId}/{version}/Image", get(get_plugin_image))
        .route("/Plugins/{pluginId}/Manifest", post(get_plugin_manifest))
        .route(
            "/Repositories",
            get(get_repositories).post(set_repositories),
        )
        .route("/Packages", get(get_packages))
        .route("/Packages/{name}", get(get_package_info))
        .route("/Packages/Installed/{name}", post(install_package))
        .route(
            "/Packages/Installing/{packageId}",
            delete(cancel_package_installation),
        )
}
