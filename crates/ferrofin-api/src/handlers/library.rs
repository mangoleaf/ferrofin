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
//!   name-sorted, projected to [`BaseItemDto`](ferrofin_model::dto::BaseItemDto).
//! - `GET /Library/PhysicalPaths` — the physical on-disk locations of every
//!   virtual folder, from the
//!   [`VirtualFolderManager`](ferrofin_traits::library::VirtualFolderManager) seam
//!   (`RootFolder.Children.SelectMany(c => c.PhysicalLocations)`).
//! - `GET /Libraries/AvailableOptions` — the library options info, projected
//!   from Ferrofin's compiled-in provider registry (via the `ProviderManager`):
//!   real per-type metadata/image fetchers plus the flat saver/reader/subtitle/
//!   segment lists.
//!
//! The external-source **change-report webhooks** are also ported here, over the
//! [`LibraryMonitor`](ferrofin_traits::library::LibraryMonitor) seam on `AppState`:
//! - `POST /Library/Series/Added` / `Updated` — reports the on-disk path of every
//!   `Series` whose TVDB id matches `tvdbId`.
//! - `POST /Library/Movies/Added` / `Updated` — reports every `Movie` matching
//!   `imdbId` (preferred) or else `tmdbId`; neither ⇒ no items (matching C#).
//! - `POST /Library/Media/Updated` — reports each path in the request body.
//!
//! The `LibraryStructureController` virtual-folder CRUD lives in the sibling
//! [`library_structure`](crate::handlers::library_structure) module. One
//! `LibraryController` branch is a faithful projection of an unmodelled field:
//! - `GET /Items/{itemId}/ThemeMedia`'s soundtrack branch (no soundtrack
//!   provider is ported — it is returned empty, exactly as C#).

use axum::extract::{Path, Query, Request, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::configuration::LibraryOptionsResultDto;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities::ExtraType;
use ferrofin_model::entities_media::MetadataProvider;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::{AllThemeMediaResult, QueryResult, ThemeMediaResult};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::{FirstTimeSetupOrAuth, RequireAuth};
use crate::error::ApiError;
use crate::handlers::items::{resolve_user, user_uuid};
use crate::handlers::query_parse::parse_csv_enums_lenient;
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
fn parse_order_by(sort_by: Option<&str>, sort_order: Option<&str>) -> Vec<(ItemSortBy, SortOrder)> {
    let keys: Vec<ItemSortBy> = parse_csv_enums_lenient(sort_by);
    let orders: Vec<SortOrder> = parse_csv_enums_lenient(sort_order);
    keys.into_iter()
        .enumerate()
        .map(|(i, key)| (key, orders.get(i).copied().unwrap_or(SortOrder::Ascending)))
        .collect()
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
    auth: &ferrofin_traits::options::AuthorizationInfo,
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

    let order_by = parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref());

    // Walk up the ancestor chain while empty and inheritance is requested.
    let (owner_id, items) = loop {
        let owner_id = Uuid::parse_str(&owner.id).map_err(|e| {
            ApiError::from(ferrofin_traits::error::ServiceError::backend(format!(
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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
    tag = "ferrofin"
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
struct MediaFoldersQuery {
    /// Optional. Filter by folders marked hidden. Ferrofin models no per-folder
    /// hidden flag, so `true` matches nothing and `false`/absent matches all.
    #[serde(default)]
    is_hidden: Option<bool>,
}

/// `GET /Library/PhysicalPaths` — the physical on-disk paths of every library.
///
/// Port of `LibraryController.GetPhysicalPaths`
/// (`RootFolder.Children.SelectMany(c => c.PhysicalLocations)`): the union of
/// every virtual folder's resolved `.mblink` shortcut targets, served by the
/// [`VirtualFolderManager`](ferrofin_traits::library::VirtualFolderManager) seam.
#[utoipa::path(
    get,
    path = "/Library/PhysicalPaths",
    responses((status = 200, description = "Physical paths returned", body = [String])),
    tag = "ferrofin"
)]
async fn get_physical_paths(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
) -> Result<Json<Vec<String>>, ApiError> {
    Ok(Json(state.virtual_folders.get_physical_paths().await?))
}

/// The query parameters of `GET /Libraries/AvailableOptions`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvailableOptionsQuery {
    /// Optional. The library content (collection) type to scope the options to.
    #[serde(default)]
    library_content_type: Option<ferrofin_model::data::CollectionType>,
    /// Optional. Whether this is a new library (accepted for parity; it only
    /// affects `DefaultEnabled` flags, which are all empty at this seam).
    #[serde(default)]
    is_new_library: bool,
}

/// The representative item types for a collection type.
///
/// Port of `LibraryController.GetRepresentativeItemTypes`: maps a collection type
/// to the item kinds whose metadata/image options a library of that type exposes.
fn representative_item_types(
    content_type: Option<ferrofin_model::data::CollectionType>,
) -> Vec<&'static str> {
    use ferrofin_model::data::CollectionType;
    match content_type {
        Some(CollectionType::boxsets) => vec!["BoxSet"],
        Some(CollectionType::playlists) => vec!["Playlist"],
        Some(CollectionType::movies) => vec!["Movie"],
        Some(CollectionType::tvshows) => vec!["Series", "Season", "Episode"],
        Some(CollectionType::books) => vec!["Book", "AudioBook"],
        Some(CollectionType::music) => vec!["MusicArtist", "MusicAlbum", "Audio", "MusicVideo"],
        Some(CollectionType::homevideos | CollectionType::photos) => vec!["Video", "Photo"],
        Some(CollectionType::musicvideos) => vec!["MusicVideo"],
        _ => vec!["Series", "Season", "Episode", "Movie"],
    }
}

/// `GET /Libraries/AvailableOptions` — the library options info.
///
/// Port of `LibraryController.GetLibraryOptionsInfo`: projects Ferrofin's
/// compiled-in provider registry (via the
/// [`ProviderManager`](ferrofin_traits::providers::ProviderManager)) into the
/// available metadata/image/subtitle/lyric/segment providers, grouped by the
/// representative item types of `libraryContentType`. A provider is listed iff
/// its code is in the build (e.g. Open Subtitles only with the `opensubtitles`
/// feature).
#[utoipa::path(
    get,
    path = "/Libraries/AvailableOptions",
    params(
        ("libraryContentType" = Option<ferrofin_model::data::CollectionType>, Query, description = "Library content type"),
        ("isNewLibrary" = Option<bool>, Query, description = "Whether this is a new library")
    ),
    responses((status = 200, description = "Library options info returned", body = LibraryOptionsResultDto)),
    tag = "ferrofin"
)]
async fn get_available_options(
    State(state): State<AppState>,
    // `[Authorize(FirstTimeSetupOrElevated)]` (method-level): the setup wizard's
    // add-library step reads this before login.
    FirstTimeSetupOrAuth(_auth): FirstTimeSetupOrAuth,
    Query(query): Query<AvailableOptionsQuery>,
) -> Result<Json<LibraryOptionsResultDto>, ApiError> {
    let item_types: Vec<String> = representative_item_types(query.library_content_type)
        .into_iter()
        .map(str::to_owned)
        .collect();
    // `isNewLibrary` only nudges DefaultEnabled in C#; Ferrofin's registry defaults
    // every provider enabled, so the flag needs no special handling.
    let _ = query.is_new_library;
    Ok(Json(
        state
            .providers
            .get_library_options_info(&item_types)
            .await?,
    ))
}

/// `GET /Library/MediaFolders` — the server's media (collection) folders.
///
/// Port of `LibraryController.GetMediaFolders`: the user-root collection folders,
/// name-sorted, projected to [`BaseItemDto`] as a
/// [`QueryResult`](ferrofin_model::querying::QueryResult).
///
/// Resolved against the authenticated user (not the nil user) so the folders and
/// their user data are the caller's. Ferrofin does not model a per-folder hidden
/// flag, so no folder is hidden: an `isHidden=true` filter returns nothing, and
/// `false`/absent returns them all (a faithful projection of "nothing hidden").
#[utoipa::path(
    get,
    path = "/Library/MediaFolders",
    params(("isHidden" = Option<bool>, Query, description = "Filter by folders marked hidden")),
    responses((status = 200, description = "Media folders returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_media_folders(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<MediaFoldersQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, None).await?;
    let user_uuid = user_uuid(&user)?;
    // The media folders are the user-root children — the collection folders plus
    // the auto-provisioned Playlists folder — returned name-sorted by the view seam
    // (C# GetUserRootFolder().Children).
    let folders = if query.is_hidden == Some(true) {
        // No folder carries a hidden flag, so none match `isHidden=true`.
        Vec::new()
    } else {
        state.user_views.get_media_folders(user_uuid).await?
    };
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&folders, &options, Some(&user), None, true)
        .await?;
    Ok(Json(QueryResult::from_items(dtos)))
}

/// Query parameters of `POST /Library/Series/Added` / `Updated`.
///
/// Ports `LibraryController.PostUpdatedSeries`'s `tvdbId` binding.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesUpdatedQuery {
    /// Optional. The TVDB id of the series whose files changed.
    #[serde(default)]
    tvdb_id: Option<String>,
}

/// Query parameters of `POST /Library/Movies/Added` / `Updated`.
///
/// Ports `LibraryController.PostUpdatedMovies`'s `tmdbId` / `imdbId` binding.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoviesUpdatedQuery {
    /// Optional. The TMDb id of the movie whose files changed.
    #[serde(default)]
    tmdb_id: Option<String>,
    /// Optional. The IMDb id of the movie whose files changed (preferred over
    /// `tmdbId` when both are supplied).
    #[serde(default)]
    imdb_id: Option<String>,
}

/// One path entry of a [`MediaUpdateInfoDto`] batch.
///
/// Port of Jellyfin's `MediaUpdateInfoPathDto`. `UpdateType` (`Created` /
/// `Modified` / `Deleted`) is accepted for contract parity but unused — the
/// monitor only needs the changed path.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaUpdateInfoPathDto {
    /// The changed media path.
    #[serde(default)]
    path: Option<String>,
    /// The kind of change (`Created` / `Modified` / `Deleted`). Accepted for
    /// parity; not read.
    #[serde(default)]
    update_type: Option<String>,
}

/// The request body of `POST /Library/Media/Updated`.
///
/// Port of Jellyfin's `MediaUpdateInfoDto`: a batch of changed media paths.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MediaUpdateInfoDto {
    /// The list of path updates to report.
    #[serde(default)]
    updates: Vec<MediaUpdateInfoPathDto>,
}

/// Reports every changed `path` to the library monitor.
///
/// Ports the `foreach (item) _libraryMonitor.ReportFileSystemChanged(item.Path)`
/// loop shared by all three webhook actions. Empty paths are skipped (the C#
/// item paths are never empty for a real item; the monitor also rejects empties).
async fn report_paths(
    state: &AppState,
    paths: impl IntoIterator<Item = String>,
) -> Result<(), ApiError> {
    for path in paths {
        if path.is_empty() {
            continue;
        }
        state
            .library_monitor
            .report_file_system_changed(&path)
            .await?;
    }
    Ok(())
}

/// Resolves the on-disk paths of every item of `kind` whose `provider` id equals
/// `value` (case-insensitive), via the [`LibraryManager`] query seam.
///
/// Mirrors the C# `GetItemList(IncludeItemTypes = kind).Where(i =>
/// i.GetProviderId(provider) == value)` selection, but pushes the exact
/// provider-id match into the query
/// ([`InternalItemsQuery::any_provider_id_equals`]) so the database does the
/// filtering. Items without a stored path contribute nothing.
async fn paths_by_provider_id(
    state: &AppState,
    kind: BaseItemKind,
    provider: MetadataProvider,
    value: &str,
) -> Result<Vec<String>, ApiError> {
    let items = state
        .library
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![kind],
            any_provider_id_equals: vec![(provider.as_name().to_owned(), value.to_owned())],
            ..InternalItemsQuery::default()
        })
        .await?;
    Ok(items.into_iter().filter_map(|i| i.path).collect())
}

/// `POST /Library/Series/Added` / `Updated` — report changed series files.
///
/// Port of `LibraryController.PostUpdatedSeries`: reports the path of every
/// `Series` whose TVDB id equals `tvdbId`. With no `tvdbId` no series match, so
/// nothing is reported — the same faithful no-op the C# `Where` yields. Always
/// `204`.
#[utoipa::path(
    post,
    path = "/Library/Series/Updated",
    params(("tvdbId" = Option<String>, Query, description = "The TVDB id of the updated series")),
    responses((status = 204, description = "Report success")),
    tag = "ferrofin"
)]
async fn post_updated_series(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<SeriesUpdatedQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    if let Some(tvdb_id) = query.tvdb_id.as_deref().filter(|v| !v.is_empty()) {
        let paths = paths_by_provider_id(
            &state,
            BaseItemKind::Series,
            MetadataProvider::Tvdb,
            tvdb_id,
        )
        .await?;
        report_paths(&state, paths).await?;
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /Library/Movies/Added` / `Updated` — report changed movie files.
///
/// Port of `LibraryController.PostUpdatedMovies`: reports the path of every
/// `Movie` matching `imdbId` (preferred), else `tmdbId`. With neither supplied
/// no movies match (C# assigns an empty list), so nothing is reported. Always
/// `204`.
#[utoipa::path(
    post,
    path = "/Library/Movies/Updated",
    params(
        ("tmdbId" = Option<String>, Query, description = "The TMDb id of the updated movie"),
        ("imdbId" = Option<String>, Query, description = "The IMDb id of the updated movie")
    ),
    responses((status = 204, description = "Report success")),
    tag = "ferrofin"
)]
async fn post_updated_movies(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<MoviesUpdatedQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    // IMDb takes precedence over TMDb, matching the C# `if/else if` chain.
    let selector = query
        .imdb_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| (MetadataProvider::Imdb, v))
        .or_else(|| {
            query
                .tmdb_id
                .as_deref()
                .filter(|v| !v.is_empty())
                .map(|v| (MetadataProvider::Tmdb, v))
        });

    if let Some((provider, value)) = selector {
        let paths = paths_by_provider_id(&state, BaseItemKind::Movie, provider, value).await?;
        report_paths(&state, paths).await?;
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `POST /Library/Media/Updated` — report a batch of changed media paths.
///
/// Port of `LibraryController.PostUpdatedMedia`: reports each `Path` in the
/// request body. C# throws `ArgumentException` (mapped to `400`) on a null path;
/// here a missing/empty path in an entry is a [`ApiError::BadRequest`]. Always
/// `204` on success.
#[utoipa::path(
    post,
    path = "/Library/Media/Updated",
    request_body = (),
    responses(
        (status = 204, description = "Report success"),
        (status = 400, description = "An update entry has no path")
    ),
    tag = "ferrofin"
)]
async fn post_updated_media(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Json(dto): Json<MediaUpdateInfoDto>,
) -> Result<axum::http::StatusCode, ApiError> {
    let mut paths = Vec::with_capacity(dto.updates.len());
    for update in dto.updates {
        let path = update
            .path
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ApiError::BadRequest("Item path can't be null.".to_owned()))?;
        let _ = update.update_type; // Accepted for parity; the monitor ignores it.
        paths.push(path);
    }
    report_paths(&state, paths).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
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
        .route("/Library/PhysicalPaths", get(get_physical_paths))
        .route("/Libraries/AvailableOptions", get(get_available_options))
        .route("/Library/Series/Added", post(post_updated_series))
        .route("/Library/Series/Updated", post(post_updated_series))
        .route("/Library/Movies/Added", post(post_updated_movies))
        .route("/Library/Movies/Updated", post(post_updated_movies))
        .route("/Library/Media/Updated", post(post_updated_media))
}
