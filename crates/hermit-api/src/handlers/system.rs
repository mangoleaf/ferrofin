//! `SystemController` — server information, lifecycle, logs, and endpoint info.
//!
//! Ports the `SystemController` actions: the full/public [`SystemInfo`], storage
//! usage, ping, restart/shutdown, the server log listing + a single log file,
//! and the request-endpoint info. All but `Ping`/`Info/Public` sit behind
//! Jellyfin auth policies (elevation/local-access) applied at the composition
//! root; the [`RequireAuth`] extractor enforces authentication here.

use axum::extract::{Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::net::EndPointInfo;
use hermit_model::system::{LogFile, PublicSystemInfo, SystemInfo};
use hermit_model::system_info_dtos::SystemStorageDto;
use hermit_traits::net::RequestContext;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Builds a [`RequestContext`] from an axum request's [`Parts`] (headers +
/// query), mirroring the auth middleware's construction so the system manager
/// sees the same request view.
fn context_from_parts(parts: &Parts) -> RequestContext {
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    RequestContext {
        headers,
        query_string: parts.uri.query().map(ToOwned::to_owned),
        remote_endpoint: None,
    }
}

/// `GET /System/Info` — the full system information for an authenticated client.
///
/// Port of `SystemController.GetSystemInfo`. Requires a valid token (Jellyfin's
/// `FirstTimeSetupOrIgnoreParentalControl` policy collapses to "authenticated"
/// at this layer).
#[utoipa::path(
    get,
    path = "/System/Info",
    responses((status = 200, description = "System info returned", body = SystemInfo)),
    tag = "hermit"
)]
async fn get_system_info(
    State(state): State<AppState>,
    _auth: RequireAuth,
    parts: Parts,
) -> Result<Json<SystemInfo>, ApiError> {
    let ctx = context_from_parts(&parts);
    let info = state.system.get_system_info(&ctx).await?;
    Ok(Json(info))
}

/// `GET /System/Info/Public` — the anonymous, public system information.
///
/// Port of `SystemController.GetPublicSystemInfo`. Never requires auth.
#[utoipa::path(
    get,
    path = "/System/Info/Public",
    responses((status = 200, description = "Public system info returned", body = PublicSystemInfo)),
    tag = "hermit"
)]
async fn get_public_system_info(
    State(state): State<AppState>,
    parts: Parts,
) -> Result<Json<PublicSystemInfo>, ApiError> {
    let ctx = context_from_parts(&parts);
    let info = state.system.get_public_system_info(&ctx).await?;
    Ok(Json(info))
}

/// `GET /System/Info/Storage` — the server's storage resource usage.
///
/// Port of `SystemController.GetSystemStorage` (elevation-gated). Projects the
/// domain [`SystemStorageInfo`](hermit_model::system::SystemStorageInfo) into
/// the API [`SystemStorageDto`].
#[utoipa::path(
    get,
    path = "/System/Info/Storage",
    responses((status = 200, description = "System storage info returned", body = SystemStorageDto)),
    tag = "hermit"
)]
async fn get_system_storage(
    State(state): State<AppState>,
    _auth: RequireAuth,
) -> Result<Json<SystemStorageDto>, ApiError> {
    let info = state.system.get_system_storage_info().await?;
    Ok(Json(SystemStorageDto::from_system_storage_info(info)))
}

/// `GET`/`POST /System/Ping` — a liveness probe returning the server name.
///
/// Port of `SystemController.PingSystem`; anonymous (returns `_appHost.Name`).
#[utoipa::path(
    get,
    path = "/System/Ping",
    responses((status = 200, description = "Server name returned", body = String)),
    tag = "hermit"
)]
async fn ping_system(State(state): State<AppState>) -> Json<String> {
    Json(state.app_host.friendly_name())
}

/// `POST /System/Restart` — begins the application restart process.
///
/// Port of `SystemController.RestartApplication` (local-access-or-elevation).
#[utoipa::path(
    post,
    path = "/System/Restart",
    responses((status = 204, description = "Server restarted")),
    tag = "hermit"
)]
async fn restart_application(
    State(state): State<AppState>,
    _auth: RequireAuth,
) -> Result<StatusCode, ApiError> {
    state.system.restart().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /System/Shutdown` — begins the application shutdown process.
///
/// Port of `SystemController.ShutdownApplication` (elevation-gated).
#[utoipa::path(
    post,
    path = "/System/Shutdown",
    responses((status = 204, description = "Server shut down")),
    tag = "hermit"
)]
async fn shutdown_application(
    State(state): State<AppState>,
    _auth: RequireAuth,
) -> Result<StatusCode, ApiError> {
    state.system.shutdown().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /System/Logs` — the available server log files, newest first.
///
/// Port of `SystemController.GetServerLogs`: lists `.txt`/`.log` files in the
/// log directory (swallowing read errors to an empty list) and orders them by
/// modified-then-created descending, then by name.
#[utoipa::path(
    get,
    path = "/System/Logs",
    responses((status = 200, description = "Log files returned", body = [LogFile])),
    tag = "hermit"
)]
async fn get_server_logs(
    State(state): State<AppState>,
    _auth: RequireAuth,
) -> Result<Json<Vec<LogFile>>, ApiError> {
    let dir = state.config.application_paths().log_directory_path();
    let files = state.file_system.get_files(&dir, &[".txt", ".log"]);
    let mut logs: Vec<LogFile> = files
        .into_iter()
        .map(|f| LogFile {
            date_created: f.date_created,
            date_modified: f.date_modified,
            size: f.length,
            name: f.name,
        })
        .collect();
    logs.sort_by(|a, b| {
        b.date_modified
            .cmp(&a.date_modified)
            .then_with(|| b.date_created.cmp(&a.date_created))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Json(logs))
}

/// Query parameters for `GET /System/Logs/Log` — the required log file `name`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogFileQuery {
    /// The name of the log file to fetch.
    #[serde(default)]
    name: Option<String>,
}

/// `GET /System/Logs/Log` — the contents of one log file.
///
/// Port of `SystemController.GetLogFile`: finds the file by (case-insensitive)
/// name in the log directory (`404` if absent) and streams it as UTF-8 text.
#[utoipa::path(
    get,
    path = "/System/Logs/Log",
    params(("name" = String, Query, description = "The name of the log file to get.")),
    responses(
        (status = 200, description = "Log file retrieved"),
        (status = 404, description = "Could not find a log file with the name")
    ),
    tag = "hermit"
)]
async fn get_log_file(
    State(state): State<AppState>,
    _auth: RequireAuth,
    Query(query): Query<LogFileQuery>,
) -> Result<Response, ApiError> {
    let name = query
        .name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing required 'name'".to_owned()))?;
    let dir = state.config.application_paths().log_directory_path();
    let file = state
        .file_system
        .get_files(&dir, &[])
        .into_iter()
        .find(|f| f.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| ApiError::NotFound("Log file not found.".to_owned()))?;
    let bytes = state.file_system.read_file(&file.full_name)?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}

/// `GET /System/Endpoint` — information about the request's network endpoint.
///
/// Port of `SystemController.GetEndpointInfo`. The `IsLocal`/`IsInNetwork`
/// determination needs the live remote IP and network config; those are not
/// surfaced at this layer, so both default to `false` (the conservative,
/// non-local answer) rather than being faked.
#[utoipa::path(
    get,
    path = "/System/Endpoint",
    responses((status = 200, description = "Endpoint info returned", body = EndPointInfo)),
    tag = "hermit"
)]
async fn get_endpoint_info(_auth: RequireAuth) -> Json<EndPointInfo> {
    Json(EndPointInfo {
        is_local: false,
        is_in_network: false,
    })
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/System/Info", get(get_system_info))
        .route("/System/Info/Public", get(get_public_system_info))
        .route("/System/Info/Storage", get(get_system_storage))
        .route("/System/Ping", get(ping_system).post(ping_system))
        .route("/System/Restart", post(restart_application))
        .route("/System/Shutdown", post(shutdown_application))
        .route("/System/Logs", get(get_server_logs))
        .route("/System/Logs/Log", get(get_log_file))
        .route("/System/Endpoint", get(get_endpoint_info))
}
