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
//! [`FileSystem`](ferrofin_traits::filesystem::FileSystem) trait, on the
//! blocking pool: a path on a hung network mount must cost a thread of its
//! own, not a runtime worker.

use std::path::Path as StdPath;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::environment_dtos::{DefaultDirectoryBrowserInfoDto, ValidatePathDto};
use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::FileSystem;
use ferrofin_util::current_culture;
use ferrofin_util::path_helper::get_full_path;

use crate::auth::FirstTimeSetupOrElevated;
use crate::error::{ApiError, ProblemStatus};
use crate::extract::{JsonBody, Query};
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
/// Port of `EnvironmentController.GetDirectoryContents`: enumerates the path
/// (`DirectoryInfo` resolves it to a full path first), keeps files/directories
/// per the flags, and orders by full path in the request's culture. A malformed
/// UNC root (`\\x`) returns an empty list, matching the C# guard. A path that is
/// no directory is a 404, and one the server may not read lists empty
/// (`IgnoreInaccessible`).
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
    _auth: FirstTimeSetupOrElevated,
    Query(query): Query<DirectoryContentsQuery>,
) -> Result<Json<Vec<FileSystemEntryInfo>>, ApiError> {
    let path = query
        .path
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| ApiError::BadRequest("missing required 'path'".to_owned()))?;

    // Guard against a bare UNC-root path (`\\x`) as the C# does.
    if path.starts_with(UNC_START_PREFIX) && path.rfind(UNC_SEPARATOR) == Some(1) {
        return Ok(Json(Vec::new()));
    }

    let (include_files, include_directories) = (query.include_files, query.include_directories);
    let culture = current_culture::capture();
    let entries = state
        .blocking_file_system(move |file_system| {
            let mut entries: Vec<FileSystemEntryInfo> =
                list_directory(file_system, &full_path(&path))?
                    .into_iter()
                    .filter(|entry| {
                        let is_dir = entry.type_ == FileSystemEntryType::Directory;
                        (is_dir && include_directories) || (!is_dir && include_files)
                    })
                    .collect();
            let comparer = current_culture::comparer(&culture).ok();
            entries.sort_by(|a, b| {
                comparer.as_ref().map_or_else(
                    || a.path.cmp(&b.path),
                    |comparer| comparer.compare(&a.path, &b.path),
                )
            });
            Ok::<_, ApiError>(entries)
        })
        .await??;
    Ok(Json(entries))
}

/// `Path.GetFullPath`, as `new DirectoryInfo(path)` resolves its argument: every
/// entry's path is listed from this, so a relative path answers with absolute
/// ones and a `..` is gone.
fn full_path(path: &str) -> String {
    let absolute = std::path::absolute(path).map_or_else(
        |_| path.to_owned(),
        |absolute| absolute.to_string_lossy().into_owned(),
    );
    get_full_path(&absolute).to_string_lossy().into_owned()
}

/// `path`'s entries for the browser. A directory the server may not read is
/// empty (`EnumerationOptions.IgnoreInaccessible`); any other failure is the
/// response's.
fn list_directory(
    file_system: &dyn FileSystem,
    path: &str,
) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
    match file_system.try_get_file_system_entries(path) {
        Err(ServiceError::Forbidden(_)) => Ok(Vec::new()),
        listed => listed,
    }
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
    _auth: FirstTimeSetupOrElevated,
    JsonBody(dto): JsonBody<ValidatePathDto>,
) -> Result<StatusCode, ApiError> {
    let path = dto.path.unwrap_or_default();
    let (is_file, validate_writable) = (dto.is_file, dto.validate_writable);
    state
        .blocking_file_system(move |file_system| {
            let exists = match is_file {
                Some(true) => file_system.file_exists(&path),
                Some(false) => file_system.directory_exists(&path),
                None => file_system.file_exists(&path) || file_system.directory_exists(&path),
            };
            if !exists {
                return Err(ApiError::Problem(ProblemStatus::NotFound));
            }
            if is_file.is_none() && validate_writable {
                file_system.validate_writable(&path)?;
            }
            Ok(StatusCode::NO_CONTENT)
        })
        .await?
}

/// `GET /Environment/Drives` — the available drives / root mounts.
///
/// Port of `EnvironmentController.GetDrives`. `get_drives` returns in bounded
/// time, leaving out a mount that does not answer, so it needs no slot of
/// [`AppState::blocking_file_system`]: the library wizard's drive list still
/// opens with every slot held by calls hung on that mount.
#[utoipa::path(
    get,
    path = "/Environment/Drives",
    responses((status = 200, description = "List of entries returned", body = [FileSystemEntryInfo])),
    tag = "ferrofin"
)]
async fn get_drives(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrElevated,
) -> Result<Json<Vec<FileSystemEntryInfo>>, ApiError> {
    let file_system = Arc::clone(&state.file_system);
    let drives = tokio::task::spawn_blocking(move || file_system.get_drives())
        .await
        .map_err(|error| ServiceError::backend(format!("listing the drives: {error}")))?;
    Ok(Json(drives))
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
async fn get_network_shares(_auth: FirstTimeSetupOrElevated) -> Json<Vec<FileSystemEntryInfo>> {
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
    _auth: FirstTimeSetupOrElevated,
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
    _auth: FirstTimeSetupOrElevated,
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
crate::query::query_parameters! {
    DirectoryContentsQuery {} => [("get", "/Environment/DirectoryContents")];
    ParentPathQuery {} => [("get", "/Environment/NetworkShares")];
}
