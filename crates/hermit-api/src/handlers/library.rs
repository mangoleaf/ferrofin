//! `LibraryController` — the portable library read/serve/scan routes.
//!
//! Ports the slice of Jellyfin's `LibraryController` whose logic is backed by
//! the DB-portable manager seam:
//!
//! - `GET /Items/{itemId}/ThemeSongs` — the item's `ThemeSong` extras.
//! - `GET /Items/{itemId}/ThemeVideos` — the item's `ThemeVideo` extras.
//! - `GET /Items/{itemId}/ThemeMedia` — both of the above (plus an empty
//!   soundtrack result, matching C#).
//! - `GET /Items/{itemId}/File` — the item's original on-disk file.
//! - `POST /Library/Refresh` — queues a full library scan.
//! - `GET /Library/MediaFolders` — the server's media (collection) folders,
//!   name-sorted, projected to [`BaseItemDto`](hermit_model::dto::BaseItemDto).
//!
//! The remaining `LibraryController`/`LibraryStructureController` routes stay on
//! the shared `501` stub as intentional deferrals, because each depends on a
//! subsystem Hermit does not model at this portable seam:
//! - `GET /Library/PhysicalPaths` — the physical on-disk locations of each
//!   collection folder (`Folder.PhysicalLocations`), which the portable
//!   [`BaseItemEntity`] rows do not carry.
//! - `GET|POST|DELETE /Library/VirtualFolders` and the `Name`/`Paths`/
//!   `LibraryOptions` mutation routes — `ILibraryManager.GetVirtualFolders`/
//!   `AddVirtualFolder`/`RemoveVirtualFolder`/… over the on-disk collection-folder
//!   tree and its per-library `LibraryOptions` persistence, which is deliberately
//!   impl-internal and absent from every trait seam.
//! - `GET /Libraries/AvailableOptions` — assembled from the metadata-plugin
//!   registry (`GetAllMetadataPlugins`) plus the static representative-type /
//!   default-image tables, none of which are ported (no metadata plugins exist at
//!   this seam).
//! - The `isHidden` filter on `/Library/MediaFolders` — the per-folder hidden
//!   flag lives in the un-ported `LibraryOptions`, so the folders are returned
//!   unfiltered (the query still succeeds and the folder set is faithful).
//! - `POST /Library/Series/Added|Updated`, `Movies/Added|Updated`,
//!   `Media/Updated` — the `ILibraryMonitor.ReportFileSystemChanged` hook, which
//!   is a filesystem watcher not surfaced on `AppState`.
//! - `GET /Items/{itemId}/ThemeMedia`'s soundtrack branch (no soundtrack
//!   provider is ported — it is returned empty, exactly as C#).

use axum::extract::{Path, Query, Request, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::dto::BaseItemDto;
use hermit_model::dto::SortOrder;
use hermit_model::entities::ExtraType;
use hermit_model::live_tv::ItemSortBy;
use hermit_model::querying::{AllThemeMediaResult, QueryResult, ThemeMediaResult};
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::handlers::query_parse::parse_csv_enums;
use crate::handlers::streaming::serve_static_file;
use crate::state::AppState;

/// Query parameters shared by the theme-media routes.
///
/// Ports the `userId` / `inheritFromParent` / `sortBy` / `sortOrder` binding of
/// `LibraryController.GetThemeSongs` / `GetThemeVideos` / `GetThemeMedia`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeMediaQuery {
    /// Optional. Filter by user id, and attach user data.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Optional. Whether parents should be searched when the item has none.
    #[serde(default)]
    inherit_from_parent: bool,
    /// Optional. Comma-delimited sort keys.
    #[serde(default)]
    sort_by: Option<String>,
    /// Optional. Comma-delimited sort orders (paired with `sort_by`).
    #[serde(default)]
    sort_order: Option<String>,
}

/// Builds the `order_by` list from the raw `sortBy` / `sortOrder` query values.
///
/// Ports `RequestHelpers.GetOrderBy`: each sort key is paired with the sort
/// order at the same index, falling back to [`SortOrder::Ascending`] when fewer
/// orders than keys are supplied. An unrecognized token is a `400`.
fn parse_order_by(
    sort_by: Option<&str>,
    sort_order: Option<&str>,
) -> Result<Vec<(ItemSortBy, SortOrder)>, ApiError> {
    let keys: Vec<ItemSortBy> = parse_csv_enums(sort_by)?;
    let orders: Vec<SortOrder> = parse_csv_enums(sort_order)?;
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(i, key)| (key, orders.get(i).copied().unwrap_or(SortOrder::Ascending)))
        .collect())
}

/// Resolves the theme-media owner for `item_id`, projecting its extras of
/// `extra_type` into a [`ThemeMediaResult`].
///
/// Ports the shared body of `GetThemeSongs` / `GetThemeVideos`: the empty guid
/// maps to the (user) root folder; otherwise the item is resolved (`404` when
/// absent). When it owns no matching extras and `inherit_from_parent` is set,
/// the walk climbs the ancestor chain (C# `item.GetParent()` loop) until an
/// owner with extras — or the root — is found. Extras are the DB items owned by
/// that item and tagged with `extra_type`, ordered by `order_by`.
async fn theme_media(
    state: &AppState,
    auth: &hermit_traits::options::AuthorizationInfo,
    query: &ThemeMediaQuery,
    item_id: Uuid,
    extra_type: ExtraType,
) -> Result<ThemeMediaResult, ApiError> {
    let user = resolve_user(state, auth, query.user_id).await?;

    // The empty guid resolves to the (user) root folder; a real id must exist.
    let mut owner = if item_id.is_nil() {
        state
            .library
            .get_user_root_folder()
            .await?
            .ok_or_else(|| ApiError::NotFound("user root folder".to_owned()))?
    } else {
        state
            .library
            .get_item_by_id(item_id)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?
    };

    let order_by = parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref())?;

    // Walk up the ancestor chain while empty and inheritance is requested.
    let (owner_id, items) = loop {
        let owner_id = Uuid::parse_str(&owner.id).map_err(|e| {
            ApiError::from(hermit_traits::error::ServiceError::backend(format!(
                "stored item id is not a uuid: {e}"
            )))
        })?;
        let items = state
            .library
            .get_item_list(&InternalItemsQuery {
                owner_ids: vec![owner_id],
                extra_types: vec![extra_type],
                order_by: order_by.clone(),
                ..InternalItemsQuery::default()
            })
            .await?;
        if !items.is_empty() || !query.inherit_from_parent {
            break (owner_id, items);
        }
        // Climb to the nearest parent; stop at the root.
        let Some(parent) =
            state
                .library
                .get_ancestors(owner_id)
                .await?
                .and_then(|mut ancestors| {
                    if ancestors.is_empty() {
                        None
                    } else {
                        Some(ancestors.remove(0))
                    }
                })
        else {
            break (owner_id, items);
        };
        owner = parent;
    };

    let dtos = state
        .dto
        .get_base_item_dtos(
            &items,
            &DtoOptions::default(),
            Some(&user),
            Some(owner_id),
            true,
        )
        .await?;
    let count = i32::try_from(dtos.len()).unwrap_or(i32::MAX);
    Ok(ThemeMediaResult {
        result: QueryResult::new(Some(0), Some(count), dtos),
        owner_id,
    })
}

/// `GET /Items/{itemId}/ThemeSongs` — the item's theme songs.
///
/// Port of `LibraryController.GetThemeSongs`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/ThemeSongs",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("userId" = Option<String>, Query, description = "Optional user id"),
        ("inheritFromParent" = Option<bool>, Query, description = "Search parents when empty"),
        ("sortBy" = Option<String>, Query, description = "Comma-delimited sort keys"),
        ("sortOrder" = Option<String>, Query, description = "Comma-delimited sort orders")
    ),
    responses(
        (status = 200, description = "Theme songs returned", body = ThemeMediaResult),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_theme_songs(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ThemeMediaQuery>,
) -> Result<Json<ThemeMediaResult>, ApiError> {
    Ok(Json(
        theme_media(&state, &auth, &query, item_id, ExtraType::ThemeSong).await?,
    ))
}

/// `GET /Items/{itemId}/ThemeVideos` — the item's theme videos.
///
/// Port of `LibraryController.GetThemeVideos`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/ThemeVideos",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("userId" = Option<String>, Query, description = "Optional user id"),
        ("inheritFromParent" = Option<bool>, Query, description = "Search parents when empty"),
        ("sortBy" = Option<String>, Query, description = "Comma-delimited sort keys"),
        ("sortOrder" = Option<String>, Query, description = "Comma-delimited sort orders")
    ),
    responses(
        (status = 200, description = "Theme videos returned", body = ThemeMediaResult),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_theme_videos(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ThemeMediaQuery>,
) -> Result<Json<ThemeMediaResult>, ApiError> {
    Ok(Json(
        theme_media(&state, &auth, &query, item_id, ExtraType::ThemeVideo).await?,
    ))
}

/// `GET /Items/{itemId}/ThemeMedia` — the item's theme songs and videos.
///
/// Port of `LibraryController.GetThemeMedia`: combines the theme-song and
/// theme-video results; the soundtrack result is always empty (no soundtrack
/// provider is ported), matching C#.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/ThemeMedia",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("userId" = Option<String>, Query, description = "Optional user id"),
        ("inheritFromParent" = Option<bool>, Query, description = "Search parents when empty"),
        ("sortBy" = Option<String>, Query, description = "Comma-delimited sort keys"),
        ("sortOrder" = Option<String>, Query, description = "Comma-delimited sort orders")
    ),
    responses(
        (status = 200, description = "Theme media returned", body = AllThemeMediaResult),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_theme_media(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ThemeMediaQuery>,
) -> Result<Json<AllThemeMediaResult>, ApiError> {
    let theme_songs_result =
        theme_media(&state, &auth, &query, item_id, ExtraType::ThemeSong).await?;
    let theme_videos_result =
        theme_media(&state, &auth, &query, item_id, ExtraType::ThemeVideo).await?;
    Ok(Json(AllThemeMediaResult {
        theme_songs_result,
        theme_videos_result,
        soundtrack_songs_result: ThemeMediaResult::default(),
    }))
}

/// `GET /Items/{itemId}/File` — the original file of an item.
///
/// Port of `LibraryController.GetFile`: resolves the item (`404` when absent),
/// then serves its on-disk `Path` with HTTP `Range`/`HEAD` support (Jellyfin's
/// `PhysicalFile`). The symlink-resolution nicety is left to the file server.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/File",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "File stream returned"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn get_file(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    request: Request,
) -> Result<Response, ApiError> {
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let path = item
        .path
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id} has no file")))?;
    serve_static_file(&path, request).await
}

/// `POST /Library/Refresh` — starts a library scan.
///
/// Port of `LibraryController.RefreshLibrary`: queues a full library scan and
/// returns `204`. C# swallows scan errors and still returns `204`; the queue is
/// a documented no-op at this seam (the scan pipeline is a later wave).
#[utoipa::path(
    post,
    path = "/Library/Refresh",
    responses((status = 204, description = "Library scan started")),
    tag = "hermit"
)]
async fn refresh_library(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<axum::http::StatusCode, ApiError> {
    state.library.queue_library_scan().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// The query parameters accepted by `GET /Library/MediaFolders`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct MediaFoldersQuery {
    /// Optional. Filter by folders marked hidden, or not. Accepted for contract
    /// compatibility but unread — the per-folder hidden flag is not modelled at
    /// this seam (see the module docs), so the folders are returned unfiltered.
    #[serde(default)]
    is_hidden: Option<bool>,
}

/// `GET /Library/MediaFolders` — the server's media (collection) folders.
///
/// Port of `LibraryController.GetMediaFolders`: the user-root collection folders,
/// name-sorted, projected to [`BaseItemDto`] as a
/// [`QueryResult`](hermit_model::querying::QueryResult). The `isHidden` filter and
/// the `LibraryOptions.Enabled` gate need the un-ported per-folder options and are
/// documented deferrals (see the module docs); the folder set and projection are
/// already the final ones.
#[utoipa::path(
    get,
    path = "/Library/MediaFolders",
    params(("isHidden" = Option<bool>, Query, description = "Filter by folders marked hidden")),
    responses((status = 200, description = "Media folders returned (QueryResult<BaseItemDto>)")),
    tag = "hermit"
)]
async fn get_media_folders(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(_query): Query<MediaFoldersQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    // The user-root collection folders are the media folders; the view seam
    // already returns them name-sorted.
    let folders = state.user_views.get_user_views(Uuid::nil()).await?;
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&folders, &options, None, None, true)
        .await?;
    Ok(Json(QueryResult::from_items(dtos)))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items/{itemId}/ThemeSongs", get(get_theme_songs))
        .route("/Items/{itemId}/ThemeVideos", get(get_theme_videos))
        .route("/Items/{itemId}/ThemeMedia", get(get_theme_media))
        .route("/Items/{itemId}/File", get(get_file))
        .route("/Library/Refresh", post(refresh_library))
        .route("/Library/MediaFolders", get(get_media_folders))
}
