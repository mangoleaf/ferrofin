//! `EnvironmentController` — server-side filesystem browsing.
//!
//! Ports the `[Authorize(FirstTimeSetupOrElevated)]` filesystem endpoints used by
//! the setup/library UIs:
//! - `GET /Environment/DirectoryContents` — a directory's entries (filtered by
//!   file/directory), ordered by full path.
//! - `POST /Environment/ValidatePath` — existence + optional writable check.
//! - `GET /Environment/Drives` — the available drives / root mounts.
//! - `GET /Environment/ParentPath` — the parent of a path.
//! - `GET /Environment/DefaultDirectoryBrowser` — the default browse root (none).
//! - `GET /Environment/NetworkShares` — always the empty array (deprecated).
//!
//! Filesystem access goes through the injected
//! [`FileSystem`](ferrofin_traits::filesystem::FileSystem) trait.

use std::path::Path as StdPath;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::environment_dtos::{DefaultDirectoryBrowserInfoDto, ValidatePathDto};
use ferrofin_model::io::FileSystemEntryInfo;

use crate::auth::FirstTimeSetupOrAuth;
use crate::error::ApiError;
use crate::extract::JsonBody;
use crate::state::AppState;

/// The leading marker of a UNC path (`\\server\share`).
const UNC_START_PREFIX: &str = "\\\\";
/// The UNC path separator.
const UNC_SEPARATOR: char = '\\';

/// Query parameters for `GET /Environment/DirectoryContents`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectoryContentsQuery {
    /// The path to enumerate.
    #[serde(default)]
    path: Option<String>,
    /// Whether to include files.
    #[serde(default)]
    include_files: bool,
    /// Whether to include directories.
    #[serde(default)]
    include_directories: bool,
}

/// `GET /Environment/DirectoryContents` — a directory's entries.
///
/// Port of `EnvironmentController.GetDirectoryContents`: enumerates the path,
/// keeps files/directories per the flags, and orders by full path. A malformed
/// UNC root (`\\x`) returns an empty list, matching the C# guard.
#[utoipa::path(
    get,
    path = "/Environment/DirectoryContents",
    params(
        ("path" = String, Query, description = "The path."),
        ("includeFiles" = Option<bool>, Query, description = "Include files."),
        ("includeDirectories" = Option<bool>, Query, description = "Include directories.")
    ),
    responses((status = 200, description = "Directory contents returned", body = [FileSystemEntryInfo])),
    tag = "ferrofin"
)]
async fn get_directory_contents(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
    Query(query): Query<DirectoryContentsQuery>,
) -> Result<Json<Vec<FileSystemEntryInfo>>, ApiError> {
    let path = query
        .path
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing required 'path'".to_owned()))?;

    // Guard against a bare UNC-root path (`\\x`) as the C# does.
    if path.starts_with(UNC_START_PREFIX) && path.rfind(UNC_SEPARATOR) == Some(1) {
        return Ok(Json(Vec::new()));
    }

    let mut entries: Vec<FileSystemEntryInfo> = state
        .file_system
        .get_file_system_entries(&path)
        .into_iter()
        .filter(|entry| {
            let is_dir = matches!(
                entry.type_,
                ferrofin_model::io::FileSystemEntryType::Directory
            );
            (is_dir && query.include_directories) || (!is_dir && query.include_files)
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Json(entries))
}

/// `POST /Environment/ValidatePath` — validate a path exists (and is writable).
///
/// Port of `EnvironmentController.ValidatePath`: when `isFile` is set, checks
/// file/directory existence accordingly; otherwise requires either to exist and,
/// if `validateWritable`, proves the directory is writable. `404` when the path
/// is absent, `204` on success.
#[utoipa::path(
    post,
    path = "/Environment/ValidatePath",
    request_body = ValidatePathDto,
    responses(
        (status = 204, description = "Path validated"),
        (status = 404, description = "Path not found")
    ),
    tag = "ferrofin"
)]
async fn validate_path(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
    JsonBody(dto): JsonBody<ValidatePathDto>,
) -> Result<StatusCode, ApiError> {
    let path = dto.path.unwrap_or_default();
    match dto.is_file {
        Some(true) => {
            if !state.file_system.file_exists(&path) {
                return Err(ApiError::NotFound(format!("file {path}")));
            }
        }
        Some(false) => {
            if !state.file_system.directory_exists(&path) {
                return Err(ApiError::NotFound(format!("directory {path}")));
            }
        }
        None => {
            if !state.file_system.file_exists(&path) && !state.file_system.directory_exists(&path) {
                return Err(ApiError::NotFound(format!("path {path}")));
            }
            if dto.validate_writable {
                state.file_system.validate_writable(&path)?;
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Environment/Drives` — the available drives / root mounts.
///
/// Port of `EnvironmentController.GetDrives`.
#[utoipa::path(
    get,
    path = "/Environment/Drives",
    responses((status = 200, description = "List of entries returned", body = [FileSystemEntryInfo])),
    tag = "ferrofin"
)]
async fn get_drives(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
) -> Json<Vec<FileSystemEntryInfo>> {
    Json(state.file_system.get_drives())
}

/// `GET /Environment/NetworkShares` — network paths (always empty; deprecated).
///
/// Port of `EnvironmentController.GetNetworkShares`, which returns an empty array
/// on modern Jellyfin.
#[utoipa::path(
    get,
    path = "/Environment/NetworkShares",
    responses((status = 200, description = "Empty array returned", body = [FileSystemEntryInfo])),
    tag = "ferrofin"
)]
async fn get_network_shares(_auth: FirstTimeSetupOrAuth) -> Json<Vec<FileSystemEntryInfo>> {
    Json(Vec::new())
}

/// Query parameters carrying a single required `path`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParentPathQuery {
    /// The path whose parent to compute.
    #[serde(default)]
    path: Option<String>,
}

/// `GET /Environment/ParentPath` — the parent directory of a path.
///
/// Port of `EnvironmentController.GetParentPath`: returns `Path.GetDirectoryName`,
/// with a UNC-share fallback. A path with no parent yields JSON `null`.
#[utoipa::path(
    get,
    path = "/Environment/ParentPath",
    params(("path" = String, Query, description = "The path.")),
    responses((status = 200, description = "Parent path returned", body = Option<String>)),
    tag = "ferrofin"
)]
async fn get_parent_path(
    _auth: FirstTimeSetupOrAuth,
    Query(query): Query<ParentPathQuery>,
) -> Result<Json<Option<String>>, ApiError> {
    let path = query
        .path
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing required 'path'".to_owned()))?;

    let mut parent = StdPath::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned());

    if parent.is_none() {
        // UNC-share fallback (C#): split at the last separator if it is a UNC path.
        if let Some(index) = path.rfind(UNC_SEPARATOR)
            && path.starts_with(UNC_SEPARATOR)
        {
            let candidate = &path[..index];
            if !candidate
                .trim_start_matches(UNC_SEPARATOR)
                .trim()
                .is_empty()
            {
                parent = Some(candidate.to_owned());
            }
        }
    }

    Ok(Json(parent))
}

/// `GET /Environment/DefaultDirectoryBrowser` — the default browse root (none).
///
/// Port of `EnvironmentController.GetDefaultDirectoryBrowser`, which returns a
/// DTO with a `null` path.
#[utoipa::path(
    get,
    path = "/Environment/DefaultDirectoryBrowser",
    responses((status = 200, description = "Default directory browser returned", body = DefaultDirectoryBrowserInfoDto)),
    tag = "ferrofin"
)]
async fn get_default_directory_browser(
    _auth: FirstTimeSetupOrAuth,
) -> Json<DefaultDirectoryBrowserInfoDto> {
    Json(DefaultDirectoryBrowserInfoDto::default())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Environment/DirectoryContents",
            get(get_directory_contents),
        )
        .route("/Environment/ValidatePath", post(validate_path))
        .route("/Environment/Drives", get(get_drives))
        .route("/Environment/NetworkShares", get(get_network_shares))
        .route("/Environment/ParentPath", get(get_parent_path))
        .route(
            "/Environment/DefaultDirectoryBrowser",
            get(get_default_directory_browser),
        )
}
