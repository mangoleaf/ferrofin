//! `PluginsController` + `PackageController` — the Tier-1 plugin-manager surface.
//!
//! Ports the plugin/package/repository API over the
//! [`PluginManager`](ferrofin_traits::plugins::PluginManager) seam
//! (`AppState::plugins`), backed by the registry of compile-time plugins the
//! composition root registers (see `docs/PLUGINS_UPSTREAM.md`) plus the WASM
//! plugins staged in `{data_dir}/plugins`. Installing from a configured
//! repository and uninstalling a staged WASM plugin are real (restart-required
//! activation, Jellyfin's model); only uninstalling a *compiled-in* plugin is
//! rejected. Every mutating route (install/uninstall/repository-set/cancel/
//! enable/disable/configuration-write) requires an administrator, porting
//! Jellyfin's `RequiresElevation` policy; a WASM plugin's config JSON is
//! handed straight to the guest, so config writes are guest input. So do the
//! READS: `PluginsController` carries `[Authorize(Policy =
//! Policies.RequiresElevation)]` at CLASS level (v10.11.8
//! Jellyfin.Api/Controllers/PluginsController.cs:25, byte-identical on master)
//! with exactly one `[AllowAnonymous]` override, `GetPluginImage` (:221). A
//! plugin's configuration holds its credentials — Ferrofin used to hand
//! `{"ApiKey":…,"Username":…,"Password":…}` to any authenticated account,
//! guest profiles included.
//!
//! - `GET /Plugins` — installed plugins
//! - `GET|POST /Plugins/{id}/Configuration` — read/write a plugin's config
//!   JSON (write admin)
//! - `POST /Plugins/{id}/{version}/{Enable,Disable}` — toggle a plugin (admin)
//! - `DELETE /Plugins/{id}` / `/Plugins/{id}/{version}` — uninstall (admin;
//!   removes a staged WASM plugin, rejects a compiled-in one)
//! - `GET /Plugins/{id}/{version}/Image` — a plugin's bundled image
//! - `POST /Plugins/{id}/Manifest` — a plugin's manifest (read; `GetPluginManifest`)
//! - `GET /Repositories`, `POST /Repositories` — package-repository list (POST admin)
//! - `GET /Packages`, `GET /Packages/{name}` — the aggregated repository catalog
//! - `POST /Packages/Installed/{name}` — install from a repository (admin)
//! - `DELETE /Packages/Installing/{packageId}` — cancel an install (admin;
//!   none tracked — installs are synchronous)

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ferrofin_model::plugins::{PluginInfo, PluginManifest, PluginStatus};
use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
use ferrofin_traits::plugins::PluginDescriptor;
use uuid::Uuid;

use crate::auth::{RequireAdmin, RequireAuth};
use crate::error::ApiError;
use crate::extract::JsonSeqBody;
use crate::state::AppState;

/// Ports Jellyfin's `RequiresElevation` policy for the plugin-mutating
/// endpoints: an API key, or a user whose policy grants `IsAdministrator`.
///
/// Without this gate, any authenticated account (a guest profile, a stolen
/// playback token) could stage arbitrary code for the next boot.
///
/// Deliberate posture divergence: other admin controllers note that
/// elevation is "deferred to the composition root", but this controller
/// gates **in-handler** — staging executable code is not mutating metadata,
/// and the gate must hold even if the composition changes. Don't "fix" this
/// back for consistency.
async fn require_admin(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
) -> Result<(), ApiError> {
    if auth.is_api_key {
        return Ok(());
    }
    if let Some(user) = &auth.user
        && super::users::is_administrator(state, user).await?
    {
        return Ok(());
    }
    Err(ApiError::Forbidden(
        "administrator access required".to_owned(),
    ))
}

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

/// The `PluginManifest` a Ferrofin plugin has.
///
/// Port of the bundled-plugin arm of `PluginManager.RegisterBundledPlugin`
/// (v10.11.8 Emby.Server.Implementations/Plugins/PluginManager.cs:562), which
/// builds `new PluginManifest { Id, Status = Active, Name, Version }` and leaves
/// every other property at the constructor default — empty strings,
/// `DateTime.MinValue`, `AutoUpdate = true`, no assemblies. That is exactly
/// Ferrofin's case for BOTH plugin tiers: a compiled-in extension and a
/// `ferrofin:plugin` WASM component alike carry only the identity their code
/// declares, and neither ships a manifest document with an owner, a category or
/// a target ABI. A live 10.11.8 answers this route for its own bundled plugins
/// the same way, down to `"description":""` while `GET /Plugins` reports a real
/// description — the two fields have different sources upstream and must not be
/// merged.
fn to_plugin_manifest(d: PluginDescriptor) -> PluginManifest {
    PluginManifest {
        category: String::new(),
        changelog: String::new(),
        description: String::new(),
        id: d.id,
        name: d.name,
        overview: String::new(),
        owner: String::new(),
        target_abi: String::new(),
        timestamp: ferrofin_model::json::datetime::dotnet_min(),
        version: d.version,
        status: if d.enabled {
            PluginStatus::Active
        } else {
            PluginStatus::Disabled
        },
        auto_update: true,
        image_path: None,
        assemblies: Vec::new(),
    }
}

/// A .NET `Version` route segment, as its four `int` components with an absent
/// component read as `-1`.
///
/// `PluginsController` binds `[FromRoute, Required] Version version` on all four
/// `{version}` routes, so ASP.NET rejects an unparseable segment with a 400
/// before the action runs, and `PluginManager.GetPlugin(id, version)` then
/// matches with `p.Version.Equals(version)` (v10.11.8
/// Emby.Server.Implementations/Plugins/PluginManager.cs:293-311). `Version`
/// requires two to four non-negative components and treats an absent one as
/// `-1`, which is why a live 10.11.8 answers 404 for `10.11.8` against an
/// installed `10.11.8.0`.
fn parse_dotnet_version(value: &str) -> Option<[i64; 4]> {
    let mut parts = [-1_i64; 4];
    let fields: Vec<&str> = value.split('.').collect();
    if !(2..=4).contains(&fields.len()) {
        return None;
    }
    for (slot, field) in parts.iter_mut().zip(&fields) {
        // `int.Parse` with `NumberStyles.Integer` — no sign, no separators, and
        // it must fit an `Int32`.
        let parsed: i32 = field.parse().ok()?;
        if parsed < 0 {
            return None;
        }
        *slot = i64::from(parsed);
    }
    Some(parts)
}

/// The plugin at `plugin_id` **and** `version`, porting
/// `IPluginManager.GetPlugin(Guid, Version)`.
///
/// A malformed version is a 400 (ASP.NET's model binder), a version that is not
/// the installed one is a 404. Ferrofin used to discard the segment entirely, so
/// `POST /Plugins/{id}/notaversion/Enable` returned 204 — it enabled the plugin
/// on the strength of a string that is not a version at all.
async fn plugin_at_version(
    state: &AppState,
    plugin_id: Uuid,
    version: &str,
) -> Result<PluginDescriptor, ApiError> {
    let want = parse_dotnet_version(version)
        .ok_or_else(|| ApiError::BadRequest(format!("The value '{version}' is not valid.")))?;
    let found = state
        .plugins
        .get_plugin(plugin_id)
        .await?
        .filter(|d| parse_dotnet_version(&d.version) == Some(want));
    found.ok_or_else(|| ApiError::NotFound(format!("plugin {plugin_id} {version}")))
}

/// `GET /Plugins` — list the installed plugins.
#[utoipa::path(
    get,
    path = "/Plugins",
    responses(
        (status = 200, description = "Installed plugins returned", body = [PluginInfo]),
        (status = 403, description = "Administrator access required")
    ),
    tag = "ferrofin"
)]
async fn get_plugins(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
) -> Result<Json<Vec<PluginInfo>>, ApiError> {
    require_admin(&state, &auth).await?;
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
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "ferrofin"
)]
async fn get_plugin_configuration(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    // A plugin's configuration is where its credentials live; the C# gates this
    // read at class level (PluginsController.cs:25).
    require_admin(&state, &auth).await?;
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
    tag = "ferrofin"
)]
async fn update_plugin_configuration(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
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
        (status = 400, description = "Version is not a valid version string"),
        (status = 404, description = "Plugin or version not found")
    ),
    tag = "ferrofin"
)]
async fn enable_plugin(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((plugin_id, version)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
    plugin_at_version(&state, plugin_id, &version).await?;
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
        (status = 400, description = "Version is not a valid version string"),
        (status = 404, description = "Plugin or version not found")
    ),
    tag = "ferrofin"
)]
async fn disable_plugin(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((plugin_id, version)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
    plugin_at_version(&state, plugin_id, &version).await?;
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
    tag = "ferrofin"
)]
async fn uninstall_plugin(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
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
    tag = "ferrofin"
)]
async fn uninstall_plugin_by_version(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((plugin_id, version)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
    plugin_at_version(&state, plugin_id, &version).await?;
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
        (status = 400, description = "Version is not a valid version string"),
        (status = 404, description = "Plugin, version or image not found")
    ),
    tag = "ferrofin"
)]
async fn get_plugin_image(
    State(state): State<AppState>,
    Path((plugin_id, version)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    // No auth extractor: this is the ONE action upstream marks
    // `[AllowAnonymous]` (v10.11.8 PluginsController.cs:221), against the
    // controller's class-level `RequiresElevation`.
    plugin_at_version(&state, plugin_id, &version).await?;
    match state.plugins.plugin_image(plugin_id).await? {
        Some(image) => Ok((
            [
                (header::CONTENT_TYPE, image.content_type),
                // `Response.Headers.ContentDisposition = "attachment"` before
                // the `PhysicalFile(...)`, in both trees.
                (header::CONTENT_DISPOSITION, "attachment".to_owned()),
            ],
            image.data,
        )
            .into_response()),
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
        (status = 200, description = "Manifest returned", body = PluginManifest),
        (status = 403, description = "Administrator access required"),
        (status = 404, description = "Plugin not found")
    ),
    tag = "ferrofin"
)]
async fn get_plugin_manifest(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(plugin_id): Path<Uuid>,
) -> Result<Json<PluginManifest>, ApiError> {
    require_admin(&state, &auth).await?;
    let Some(d) = state.plugins.get_plugin(plugin_id).await? else {
        return Err(ApiError::NotFound(format!("plugin {plugin_id}")));
    };
    // The vendored contract carries no `PluginManifest` schema component, which
    // is why the Layer-1 sweep could never see the shape — but
    // `MediaBrowser.Common/Plugins/PluginManifest.cs` pins it exactly, down to
    // the per-property camelCase `[JsonPropertyName]`s and the `guid` spelling
    // of the id. Ferrofin used to hand-roll five PascalCase keys and a
    // hyphenated `Id` here, which shares not one key with what a client reads.
    Ok(Json(to_plugin_manifest(d)))
}

/// `GET /Repositories` — the configured package repositories.
#[utoipa::path(
    get,
    path = "/Repositories",
    responses((status = 200, description = "Repositories returned", body = [RepositoryInfo])),
    tag = "ferrofin"
)]
async fn get_repositories(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
) -> Result<Json<Vec<RepositoryInfo>>, ApiError> {
    Ok(Json(state.plugins.get_repositories().await?))
}

/// `POST /Repositories` — replace the configured package repositories.
#[utoipa::path(
    post,
    path = "/Repositories",
    request_body = [RepositoryInfo],
    responses((status = 204, description = "Repositories updated")),
    tag = "ferrofin"
)]
async fn set_repositories(
    State(state): State<AppState>,
    RequireAdmin(auth): RequireAdmin,
    JsonSeqBody(repositories): JsonSeqBody<Vec<RepositoryInfo>>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
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
    tag = "ferrofin"
)]
async fn get_packages(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
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
    tag = "ferrofin"
)]
async fn get_package_info(
    State(state): State<AppState>,
    RequireAdmin(_auth): RequireAdmin,
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

/// Query parameters for `POST /Packages/Installed/{name}` — what jellyfin-web
/// sends alongside the path name (all optional per the contract).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallPackageQuery {
    /// The package guid; wins over the name when present.
    #[serde(default)]
    assembly_guid: Option<String>,
    /// A specific version to install; newest when absent.
    #[serde(default)]
    version: Option<String>,
    /// Restrict resolution to one repository's versions.
    #[serde(default)]
    repository_url: Option<String>,
}

/// `POST /Packages/Installed/{name}` — install a package from the configured
/// repositories.
///
/// The manager downloads, verifies (checksum + component validation), and
/// stages the WASM plugin; it activates on the next restart
/// (`SystemInfo.HasPendingRestart` flips true, matching Jellyfin's flow).
#[utoipa::path(
    post,
    path = "/Packages/Installed/{name}",
    params(
        ("name" = String, Path, description = "Package name"),
        ("assemblyGuid" = Option<String>, Query, description = "Package guid (wins over name)"),
        ("version" = Option<String>, Query, description = "Version to install (newest when absent)"),
        ("repositoryUrl" = Option<String>, Query, description = "Restrict to one repository"),
    ),
    responses(
        (status = 204, description = "Package installed; restart required to activate"),
        (status = 404, description = "No such package or version"),
    ),
    tag = "ferrofin"
)]
async fn install_package(
    State(state): State<AppState>,
    RequireAdmin(auth): RequireAdmin,
    Path(name): Path<String>,
    Query(query): Query<InstallPackageQuery>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
    let assembly_guid = match query.assembly_guid.as_deref().filter(|g| !g.is_empty()) {
        Some(raw) => Some(uuid::Uuid::parse_str(raw).map_err(|_| {
            ApiError::BadRequest(format!("assemblyGuid `{raw}` is not a valid GUID"))
        })?),
        None => None,
    };
    state
        .plugins
        .install_package(
            &name,
            assembly_guid,
            query.version.as_deref().filter(|v| !v.is_empty()),
            query.repository_url.as_deref().filter(|r| !r.is_empty()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Packages/Installing/{packageId}` — cancel a running install.
///
/// No installs run (Tier-1 has no installer), so there is nothing to cancel.
#[utoipa::path(
    delete,
    path = "/Packages/Installing/{packageId}",
    params(("packageId" = String, Path, description = "Package id")),
    responses((status = 404, description = "No such active installation")),
    tag = "ferrofin"
)]
async fn cancel_package_installation(
    State(state): State<AppState>,
    RequireAdmin(auth): RequireAdmin,
    Path(package_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state, &auth).await?;
    Err(ApiError::NotFound(format!("installation {package_id}")))
}

/// Credential-bearing request headers a guest must never see (the WIT
/// guarantees the resolved identity fields are the only auth facts).
const CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "cookie",
    "x-emby-token",
    "x-emby-authorization",
    "x-mediabrowser-token",
];

/// Framing and hop-by-hop response headers belong to the transport, not
/// the guest — forwarding them fights hyper's own framing.
const RESERVED_RESPONSE_HEADERS: [&str; 7] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
];

/// Max inbound body for a plugin-routed request — an abuse guard on an
/// anonymous surface, not a tuning knob (plugin APIs move JSON, not media).
const PLUGIN_REQUEST_BODY_MAX: usize = 1024 * 1024;

/// `ANY /Plugins/{pluginId}/web/{*path}` — a runtime plugin's own URL space.
///
/// Reachable WITHOUT authentication (plugin pages load assets via plain
/// `<script src>` tags, exactly like upstream plugin controllers marked
/// `[AllowAnonymous]`); the caller's resolved identity is forwarded so the
/// GUEST gates sensitive paths. The guest runs sandboxed, deadline-bound and
/// breaker-protected; unknown or disabled plugins 404.
async fn plugin_web_request(
    axum::extract::State(state): axum::extract::State<AppState>,
    method: axum::http::Method,
    Path((plugin_id, rest)): Path<(Uuid, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    let Some(dispatch) = state.plugin_routes.clone() else {
        return Err(ApiError::NotFound("no runtime plugin host".to_owned()));
    };
    // The caller's identity, resolved by the auth middleware — forwarded as
    // plain facts, never the token. Admin is a policy read, same as the
    // admin gate above, but non-fatal here.
    let auth = request
        .extensions()
        .get::<ferrofin_traits::options::AuthorizationInfo>()
        .cloned()
        .unwrap_or_default();
    let is_admin = require_admin(&state, &auth).await.is_ok();
    let body = axum::body::to_bytes(request.into_body(), PLUGIN_REQUEST_BODY_MAX)
        .await
        .map_err(|_| {
            ApiError::BadRequest(format!(
                "request body exceeds the {PLUGIN_REQUEST_BODY_MAX}-byte plugin route limit"
            ))
        })?;
    // The WIT guarantees a guest NEVER sees credentials: strip the auth
    // headers and the `api_key` query parameter before forwarding — the
    // resolved identity fields are the only auth facts a plugin gets.
    let query = raw_query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| {
            let key = pair.split('=').next().unwrap_or_default();
            !key.eq_ignore_ascii_case("api_key") && !key.eq_ignore_ascii_case("apikey")
        })
        .collect::<Vec<_>>()
        .join("&");
    let web_request = ferrofin_traits::plugins::PluginWebRequest {
        method: method.to_string(),
        path: format!("/{rest}"),
        query,
        headers: headers
            .iter()
            .filter(|(n, _)| !CREDENTIAL_HEADERS.contains(&n.as_str()))
            .map(|(n, v)| {
                (
                    n.to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: (!body.is_empty()).then(|| body.to_vec()),
        user_id: auth.user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok()),
        is_admin,
        is_authenticated: auth.is_authenticated,
    };
    let Some(reply) = dispatch.handle(plugin_id, web_request).await? else {
        return Err(ApiError::NotFound(format!("plugin {plugin_id}")));
    };
    let mut response = axum::http::Response::builder().status(
        axum::http::StatusCode::from_u16(reply.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
    );
    for (name, value) in reply.headers {
        // Invalid guest-supplied header names/values are dropped, not fatal.
        if RESERVED_RESPONSE_HEADERS
            .iter()
            .any(|r| name.eq_ignore_ascii_case(r))
        {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            axum::http::HeaderValue::try_from(value.as_str()),
        ) {
            response = response.header(n, v);
        }
    }
    Ok(response
        .body(axum::body::Body::from(reply.body))
        .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()))
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
        // The runtime plugins' own URL space (any method — the guest routes).
        .route(
            "/Plugins/{pluginId}/web/{*path}",
            axum::routing::any(plugin_web_request),
        )
}
