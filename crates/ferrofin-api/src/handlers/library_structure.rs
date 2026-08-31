//! `LibraryStructureController` — the virtual-folder (library-structure) admin
//! routes.
//!
//! Ports every route of Jellyfin's `LibraryStructureController`
//! (`[Route("Library/VirtualFolders")]`):
//!
//! - `GET  /Library/VirtualFolders` — list the configured virtual folders.
//! - `POST /Library/VirtualFolders` — add a virtual folder.
//! - `DELETE /Library/VirtualFolders` — remove a virtual folder.
//! - `POST /Library/VirtualFolders/Name` — rename a virtual folder.
//! - `POST /Library/VirtualFolders/LibraryOptions` — replace a library's options.
//! - `POST /Library/VirtualFolders/Paths` — add a media path to a library.
//! - `POST /Library/VirtualFolders/Paths/Update` — update a media path.
//! - `DELETE /Library/VirtualFolders/Paths` — remove a media path.
//!
//! Each handler calls the [`VirtualFolderManager`](ferrofin_traits::library::VirtualFolderManager)
//! seam on [`AppState`], which is backed at the composition root by the
//! filesystem `FerrofinVirtualFolderManager` (see `handlers::library` for the two
//! `LibraryController` structure reads, `PhysicalPaths` + `AvailableOptions`).
//!
//! Every mutation ends with the C# `ILibraryMonitor` stop/start dance (so the
//! filesystem watcher's root set tracks the new structure and realtime-option
//! toggles) and honors the `refreshLibrary` query flag by queueing a scan —
//! mirroring the `finally` block of each C# controller action.

use axum::Router;
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use uuid::Uuid;

use crate::auth::FirstTimeSetupOrAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// Restarts the library monitor so its watch set matches the just-mutated
/// library structure, then queues a full scan when the request asked for one —
/// the `ILibraryMonitor` stop/start + `refreshLibrary` dance every C#
/// `LibraryStructureController` action ends with. Best-effort: a watcher or
/// queue failure must not fail the admin request whose mutation already
/// succeeded.
async fn after_structure_change(state: &AppState, refresh_library: bool, scope: Option<Uuid>) {
    if let Err(err) = state.library_monitor.stop().await {
        tracing::warn!(%err, "failed to stop library monitor");
    }
    if let Err(err) = state.library_monitor.start().await {
        tracing::warn!(%err, "failed to restart library monitor");
    }
    if refresh_library {
        // A freshly-added library scans scoped: a full pass re-probes every
        // existing item (hours on a big install) and plans the new library's
        // items LAST, so an interrupted scan leaves it empty — observed live
        // as "music library scanning is broken".
        let result = match scope {
            Some(library_id) => state.library.queue_library_scan_scoped(library_id).await,
            None => state.library.queue_library_scan().await,
        };
        if let Err(err) = result {
            tracing::warn!(%err, "failed to queue the requested library refresh");
        }
    }
}

/// Resolves a just-mutated virtual folder's `CollectionFolder` id by name, for
/// scoping the follow-up scan.
///
/// Best-effort. `get_virtual_folders` mints the row (and its path-derived id)
/// for every directory it enumerates, so a just-renamed library resolves; the
/// `None` arm is reachable only when the directory does not project an id at all
/// (it is not under the user-views root, or its name is not valid UTF-8). The
/// caller must treat `None` as "unscoped", i.e. a FULL scan — see the note on
/// [`rename_virtual_folder`].
async fn library_id_by_name(state: &AppState, name: &str) -> Option<Uuid> {
    state
        .virtual_folders
        .get_virtual_folders()
        .await
        .ok()?
        .into_iter()
        .find(|vf| vf.name.as_deref() == Some(name))
        .and_then(|vf| {
            vf.item_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
        })
}

/// `GET /Library/VirtualFolders` — the configured virtual folders.
///
/// Port of `LibraryStructureController.GetVirtualFolders`.
#[utoipa::path(
    get,
    path = "/Library/VirtualFolders",
    responses((status = 200, description = "Virtual folders retrieved", body = [VirtualFolderInfo])),
    tag = "ferrofin"
)]
async fn get_virtual_folders(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
) -> Result<Json<Vec<VirtualFolderInfo>>, ApiError> {
    Ok(Json(state.virtual_folders.get_virtual_folders().await?))
}

/// The request body of `POST /Library/VirtualFolders`.
///
/// Port of `AddVirtualFolderDto`: an optional wrapper carrying the library
/// options. The name/collection-type/paths are bound from the query string.
#[derive(Debug, Default, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "PascalCase")]
struct AddVirtualFolderBody {
    /// The library options; defaulted when the body (or field) is absent.
    #[serde(default)]
    library_options: Option<LibraryOptions>,
}

/// The query parameters of `POST /Library/VirtualFolders`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddVirtualFolderQuery {
    /// The name of the virtual folder.
    #[serde(default)]
    name: Option<String>,
    /// The collection type of the library.
    #[serde(default)]
    collection_type: Option<CollectionTypeOptions>,
    /// The media paths (comma-delimited).
    #[serde(default)]
    paths: Option<String>,
    /// Whether to queue a library scan after adding.
    #[serde(default)]
    refresh_library: bool,
}

/// `POST /Library/VirtualFolders` — add a virtual folder.
///
/// Port of `LibraryStructureController.AddVirtualFolder`: the name must be
/// non-empty (`400` otherwise, mirroring the C# `RegularExpression` guard), the
/// query `paths` (if any) override the body's `PathInfos`, and each media path
/// must exist on disk. The optional `AddVirtualFolderDto` body supplies the rest
/// of the library options.
#[utoipa::path(
    post,
    path = "/Library/VirtualFolders",
    params(
        ("name" = Option<String>, Query, description = "The name of the virtual folder"),
        ("collectionType" = Option<CollectionTypeOptions>, Query, description = "The collection type"),
        ("paths" = Option<String>, Query, description = "Comma-delimited media paths"),
        ("refreshLibrary" = Option<bool>, Query, description = "Whether to refresh the library")
    ),
    request_body = Option<AddVirtualFolderBody>,
    responses((status = 204, description = "Folder added")),
    tag = "ferrofin"
)]
async fn add_virtual_folder(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Query(query): Query<AddVirtualFolderQuery>,
    body: Option<Json<AddVirtualFolderBody>>,
) -> Result<StatusCode, ApiError> {
    let name = query.name.unwrap_or_default();
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "Library name cannot be empty or have leading/trailing spaces.".to_owned(),
        ));
    }

    let mut options = body
        .and_then(|Json(b)| b.library_options)
        .unwrap_or_default();

    // The query `paths` (comma-delimited) override the body's PathInfos, exactly
    // as the C# controller assigns `libraryOptions.PathInfos` when `paths` is
    // non-empty.
    if let Some(raw) = query.paths.as_deref() {
        let paths: Vec<MediaPathInfo> = raw
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| MediaPathInfo { path: p.to_owned() })
            .collect();
        if !paths.is_empty() {
            options.path_infos = paths;
        }
    }

    state
        .virtual_folders
        .add_virtual_folder(&name, query.collection_type, &options)
        .await?;
    let scope = library_id_by_name(&state, &name).await;
    after_structure_change(&state, query.refresh_library, scope).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The query parameters of `DELETE /Library/VirtualFolders`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveVirtualFolderQuery {
    /// The name of the folder.
    #[serde(default)]
    name: Option<String>,
    /// Whether to queue a library scan after the removal.
    #[serde(default)]
    refresh_library: bool,
}

/// `DELETE /Library/VirtualFolders` — remove a virtual folder.
///
/// Port of `LibraryStructureController.RemoveVirtualFolder`: a missing folder is
/// a `404` (mirroring the C# `FileNotFoundException`).
#[utoipa::path(
    delete,
    path = "/Library/VirtualFolders",
    params(
        ("name" = Option<String>, Query, description = "The name of the folder"),
        ("refreshLibrary" = Option<bool>, Query, description = "Whether to refresh the library")
    ),
    responses(
        (status = 204, description = "Folder removed"),
        (status = 404, description = "Folder not found")
    ),
    tag = "ferrofin"
)]
async fn remove_virtual_folder(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Query(query): Query<RemoveVirtualFolderQuery>,
) -> Result<StatusCode, ApiError> {
    let name = query.name.unwrap_or_default();
    state.virtual_folders.remove_virtual_folder(&name).await?;
    after_structure_change(&state, query.refresh_library, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The query parameters of `POST /Library/VirtualFolders/Name`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameVirtualFolderQuery {
    /// The current name of the virtual folder.
    #[serde(default)]
    name: Option<String>,
    /// The new name.
    #[serde(default)]
    new_name: Option<String>,
    /// Whether to queue a library scan after the rename.
    #[serde(default)]
    refresh_library: bool,
}

/// `POST /Library/VirtualFolders/Name` — rename a virtual folder.
///
/// Port of `LibraryStructureController.RenameVirtualFolder`: a missing `name`/
/// `newName` is a `400` (C# `ArgumentNullException`), a missing source folder is
/// a `404`, and a target name that already exists is a `409`.
#[utoipa::path(
    post,
    path = "/Library/VirtualFolders/Name",
    params(
        ("name" = Option<String>, Query, description = "The current name"),
        ("newName" = Option<String>, Query, description = "The new name"),
        ("refreshLibrary" = Option<bool>, Query, description = "Whether to refresh the library")
    ),
    responses(
        (status = 204, description = "Folder renamed"),
        (status = 404, description = "Library does not exist"),
        (status = 409, description = "Library already exists")
    ),
    tag = "ferrofin"
)]
async fn rename_virtual_folder(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Query(query): Query<RenameVirtualFolderQuery>,
) -> Result<StatusCode, ApiError> {
    let name = query.name.unwrap_or_default();
    let new_name = query.new_name.unwrap_or_default();
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".to_owned()));
    }
    if new_name.trim().is_empty() {
        return Err(ApiError::BadRequest("newName must not be empty".to_owned()));
    }
    state
        .virtual_folders
        .rename_virtual_folder(&name, &new_name)
        .await?;
    // Deliberate divergence from `LibraryStructureController.RenameVirtualFolder`,
    // which runs `ValidateTopLibraryFolders(ct, removeRoot: true)` only when
    // `refreshLibrary` is set and otherwise just delays 1 s and restarts the
    // monitor. Ferrofin derives a library's item id from its directory path, so
    // the rename necessarily re-keys the row: the manager deletes the row the
    // vacated directory backed (upstream's `ValidateTopLibraryFolders` leg) and
    // the new path mints a fresh one, which cascades the old row's children away.
    // Upstream can skip the refresh because it keeps the stale row (and its
    // children) until the next scan; here, skipping it would leave the renamed
    // library empty. So the rescan is unconditional.
    //
    // It is scoped to the renamed library WHEN its id resolves, which is the
    // normal case. If it does not, the fallback is a FULL library scan — more
    // than upstream would queue for the same request, and on a large install a
    // dashboard rename would then kick a whole pass. That arm is logged at WARN
    // rather than left silent, because a full pass nobody asked for should be
    // attributable.
    tracing::debug!(
        refresh_library = query.refresh_library,
        "renamed a library; rescanning it regardless, because its item id is derived from its path"
    );
    let scope = library_id_by_name(&state, &new_name).await;
    if scope.is_none() {
        tracing::warn!(
            new_name,
            "renamed library has no resolvable item id; falling back to a FULL library scan"
        );
    }
    after_structure_change(&state, true, scope).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The request body of `POST /Library/VirtualFolders/Paths`.
///
/// Port of `MediaPathDto`: the library `Name` plus either a raw `Path` or a
/// full `PathInfo` (at least one must be present).
#[derive(Debug, Default, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "PascalCase")]
struct MediaPathBody {
    /// The name of the library.
    #[serde(default)]
    name: Option<String>,
    /// The path to add.
    #[serde(default)]
    path: Option<String>,
    /// The full path info.
    #[serde(default)]
    path_info: Option<MediaPathInfo>,
}

/// The query parameters of `POST /Library/VirtualFolders/Paths` and
/// `DELETE /Library/VirtualFolders/Paths`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaPathQuery {
    /// The name of the library (delete only).
    #[serde(default)]
    name: Option<String>,
    /// The path to remove (delete only).
    #[serde(default)]
    path: Option<String>,
    /// Whether to queue a library scan after the change.
    #[serde(default)]
    refresh_library: bool,
}

/// `POST /Library/VirtualFolders/Paths` — add a media path to a library.
///
/// Port of `LibraryStructureController.AddMediaPath`: the `PathInfo` (or a bare
/// `Path`) is added to the named library; a request with neither is a `400`.
#[utoipa::path(
    post,
    path = "/Library/VirtualFolders/Paths",
    params(("refreshLibrary" = Option<bool>, Query, description = "Whether to refresh the library")),
    request_body = MediaPathBody,
    responses((status = 204, description = "Media path added")),
    tag = "ferrofin"
)]
async fn add_media_path(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Query(query): Query<MediaPathQuery>,
    Json(body): Json<MediaPathBody>,
) -> Result<StatusCode, ApiError> {
    let name = body.name.unwrap_or_default();
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "The name of the library may not be empty.".to_owned(),
        ));
    }
    let path_info = match (body.path_info, body.path) {
        (Some(info), _) => info,
        (None, Some(path)) => MediaPathInfo { path },
        (None, None) => {
            return Err(ApiError::BadRequest(
                "PathInfo and Path can't both be null.".to_owned(),
            ));
        }
    };
    state
        .virtual_folders
        .add_media_path(&name, &path_info)
        .await?;
    after_structure_change(&state, query.refresh_library, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The request body of `POST /Library/VirtualFolders/Paths/Update`.
///
/// Port of `UpdateMediaPathRequestDto`: the library `Name` plus the full
/// `PathInfo` to update (both required).
#[derive(Debug, Default, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "PascalCase")]
struct UpdateMediaPathBody {
    /// The library name.
    #[serde(default)]
    name: Option<String>,
    /// The path info to update.
    #[serde(default)]
    path_info: Option<MediaPathInfo>,
}

/// `POST /Library/VirtualFolders/Paths/Update` — update a media path.
///
/// Port of `LibraryStructureController.UpdateMediaPath`: an empty `Name` is a
/// `400` (C# `ArgumentNullException`).
#[utoipa::path(
    post,
    path = "/Library/VirtualFolders/Paths/Update",
    request_body = UpdateMediaPathBody,
    responses((status = 204, description = "Media path updated")),
    tag = "ferrofin"
)]
async fn update_media_path(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Json(body): Json<UpdateMediaPathBody>,
) -> Result<StatusCode, ApiError> {
    let name = body.name.unwrap_or_default();
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "Name must not be null or empty".to_owned(),
        ));
    }
    let path_info = body
        .path_info
        .ok_or_else(|| ApiError::BadRequest("PathInfo must not be null".to_owned()))?;
    state
        .virtual_folders
        .update_media_path(&name, &path_info)
        .await?;
    // No `refreshLibrary` flag on this route (matching the C# action).
    after_structure_change(&state, false, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Library/VirtualFolders/Paths` — remove a media path.
///
/// Port of `LibraryStructureController.RemoveMediaPath`: an empty `name`/`path`
/// is a `400` (C# `ArgumentException`).
#[utoipa::path(
    delete,
    path = "/Library/VirtualFolders/Paths",
    params(
        ("name" = Option<String>, Query, description = "The name of the library"),
        ("path" = Option<String>, Query, description = "The path to remove"),
        ("refreshLibrary" = Option<bool>, Query, description = "Whether to refresh the library")
    ),
    responses((status = 204, description = "Media path removed")),
    tag = "ferrofin"
)]
async fn remove_media_path(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Query(query): Query<MediaPathQuery>,
) -> Result<StatusCode, ApiError> {
    let name = query.name.unwrap_or_default();
    let path = query.path.unwrap_or_default();
    if name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".to_owned()));
    }
    if path.trim().is_empty() {
        return Err(ApiError::BadRequest("path must not be empty".to_owned()));
    }
    state
        .virtual_folders
        .remove_media_path(&name, &path)
        .await?;
    after_structure_change(&state, query.refresh_library, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The request body of `POST /Library/VirtualFolders/LibraryOptions`.
///
/// Port of `UpdateLibraryOptionsDto`: the library item `Id` plus the new
/// `LibraryOptions`.
#[derive(Debug, Default, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "PascalCase")]
struct UpdateLibraryOptionsBody {
    /// The library item id.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    id: Option<Uuid>,
    /// The library name (Ferrofin's filesystem seam resolves by name; see below).
    #[serde(default)]
    name: Option<String>,
    /// The new library options.
    #[serde(default)]
    library_options: Option<LibraryOptions>,
}

/// `POST /Library/VirtualFolders/LibraryOptions` — replace a library's options.
///
/// Port of `LibraryStructureController.UpdateLibraryOptions`: creates a shortcut
/// for any newly-referenced media path, then persists the new options.
///
/// **Id vs name.** C# looks the library up by its `CollectionFolder` item id
/// (`GetItemById<CollectionFolder>(request.Id)`), and jellyfin-web sends only that
/// `Id` (no `Name`). Ferrofin's virtual-folder seam is keyed by the library `Name`
/// (its directory name), but the `CollectionFolder` row now carries the same
/// deterministic id it projects into `VirtualFolderInfo.ItemId`, so we resolve the
/// posted `Id` back to its name via `get_virtual_folders`. `Name` is still honored
/// when supplied; a request matching no library (by either) is a `404`.
#[utoipa::path(
    post,
    path = "/Library/VirtualFolders/LibraryOptions",
    request_body = UpdateLibraryOptionsBody,
    responses(
        (status = 204, description = "Library updated"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn update_library_options(
    State(state): State<AppState>,
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Json(body): Json<UpdateLibraryOptionsBody>,
) -> Result<StatusCode, ApiError> {
    let options = body.library_options.unwrap_or_default();
    // No name: resolve the library by its CollectionFolder id (what jellyfin-web
    // posts). Match the projected `ItemId` against the persisted virtual folders.
    let name = if let Some(name) = body.name.filter(|n| !n.trim().is_empty()) {
        name
    } else {
        let id = body
            .id
            .ok_or_else(|| ApiError::NotFound("library (no Id or Name supplied)".to_owned()))?;
        let wanted = id.to_string();
        state
            .virtual_folders
            .get_virtual_folders()
            .await?
            .into_iter()
            .find(|f| f.item_id.as_deref() == Some(wanted.as_str()))
            .and_then(|f| f.name)
            .ok_or_else(|| ApiError::NotFound(format!("library {id}")))?
    };
    state
        .virtual_folders
        .update_library_options(&name, &options)
        .await?;
    // Restart the watcher so an `EnableRealtimeMonitor` toggle applies live.
    after_structure_change(&state, false, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Library/VirtualFolders",
            get(get_virtual_folders)
                .post(add_virtual_folder)
                .delete(remove_virtual_folder),
        )
        .route("/Library/VirtualFolders/Name", post(rename_virtual_folder))
        .route(
            "/Library/VirtualFolders/LibraryOptions",
            post(update_library_options),
        )
        .route(
            "/Library/VirtualFolders/Paths",
            post(add_media_path).delete(remove_media_path),
        )
        .route(
            "/Library/VirtualFolders/Paths/Update",
            post(update_media_path),
        )
}
