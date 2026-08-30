//! `ItemsController` / `UserLibraryController` / `LibraryController` — item
//! queries, counts, ancestors, and deletion under the `/Items` path.
//!
//! Ports the portable `/Items` surface:
//!
//! - `GET  /Items` — a paged, filtered, user-scoped query over the library,
//!   projected to [`BaseItemDto`]s and wrapped in a [`QueryResult`]. The wide
//!   Jellyfin `GetItems` query is mapped onto [`InternalItemsQuery`]; the
//!   persistence layer (`translate_query`) applies every filter.
//! - `GET  /Items/{itemId}` — a single item by id.
//! - `DELETE /Items/{itemId}` — deletes one item (and its subtree).
//! - `DELETE /Items` — deletes several items by id.
//! - `GET  /Items/Counts` — per-kind item counts for a (favourite-filtered) query.
//! - `GET  /Items/{itemId}/Ancestors` — the item's parents, nearest first.
//!
//! The remote/metadata-provider slices of these controllers (search providers,
//! box-set collapsing that needs the un-ported `Folder` OOP tree) are applied by
//! the persistence layer where portable and otherwise left to later waves; the
//! handler maps the request faithfully onto the query struct.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{BaseItemDto, ItemCounts};
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{AuthorizationInfo, DeleteOptions, DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::query_parse::{parse_csv_enums_lenient, parse_csv_uuids, parse_pipe_strings};
use crate::state::AppState;

/// Resolves the effective user for a request: the explicit `user_id` query
/// parameter when present, otherwise the authenticated caller.
///
/// Mirrors Jellyfin's `RequestHelpers.GetUserId`. A `user_id` that resolves to
/// no account is a `400`; a caller with neither an explicit id nor an
/// authenticated user is likewise rejected.
pub(crate) async fn resolve_user(
    state: &AppState,
    auth: &AuthorizationInfo,
    user_id: Option<Uuid>,
) -> Result<UserEntity, ApiError> {
    let effective = user_id.unwrap_or_else(|| auth.user_id());
    if effective.is_nil() {
        return Err(ApiError::BadRequest("no user for request".to_owned()));
    }
    // The auth layer already loaded the caller's row — reuse it instead of
    // re-fetching the identical user on every user-scoped request.
    if effective == auth.user_id()
        && let Some(user) = &auth.user
    {
        return Ok(user.clone());
    }
    state
        .users
        .get_user_by_id(effective)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {effective}")))
}

/// The resolved user's id as a [`Uuid`].
///
/// [`UserEntity::id`] holds the hyphenated `Guid` text the row was written with
/// (`guid_to_db`), so this is exactly the id the lookup used. A row whose id
/// does not parse means a corrupt `Users` table — a state upstream cannot even
/// express, because C# `User.Id` *is* a `Guid`. Report it as a backend failure
/// (`500`) rather than degrading to the nil GUID, which would silently scope the
/// request to *no* user: empty user data, no parental filtering, and a `UserId`
/// no client can act on.
pub(crate) fn user_uuid(user: &UserEntity) -> Result<Uuid, ApiError> {
    Uuid::parse_str(&user.id).map_err(|_| {
        ApiError::Service(ferrofin_traits::ServiceError::Backend(
            "stored user id is not a guid".to_owned(),
        ))
    })
}

/// Resolves the effective user *optionally*: like [`resolve_user`] but a nil
/// effective id yields [`None`] rather than a `400`.
///
/// Mirrors the controllers (counts/ancestors/delete) that accept an API-key
/// caller with no user — Jellyfin's `userId.IsNullOrEmpty() ? null : GetUserById`.
pub(crate) async fn resolve_user_opt(
    state: &AppState,
    auth: &AuthorizationInfo,
    user_id: Option<Uuid>,
) -> Result<Option<UserEntity>, ApiError> {
    let effective = user_id.unwrap_or_else(|| auth.user_id());
    if effective.is_nil() {
        return Ok(None);
    }
    // Same reuse as `resolve_user`: the auth layer already holds this row.
    if effective == auth.user_id()
        && let Some(user) = &auth.user
    {
        return Ok(Some(user.clone()));
    }
    Ok(state.users.get_user_by_id(effective).await?)
}

/// The query parameters honoured by `GET /Items`.
///
/// Every field is optional. Comma/pipe-delimited multi-value parameters arrive as
/// a raw [`String`] and are split + parsed in [`get_items`]; this keeps the serde
/// surface simple while matching Jellyfin's `CommaDelimited`/`PipeDelimited`
/// model binders.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The index of the first item to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of items to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Whether to search descendants recursively.
    #[serde(default)]
    recursive: Option<bool>,
    /// A free-text search term.
    #[serde(default)]
    search_term: Option<String>,
    /// Localizes the query to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`BaseItemKind`](ferrofin_model::data::BaseItemKind) set to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited item kinds to exclude.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited [`MediaType`](ferrofin_model::data::MediaType) set.
    #[serde(default)]
    media_types: Option<String>,
    /// Comma-delimited [`ItemSortBy`](ferrofin_model::live_tv::ItemSortBy) columns.
    #[serde(default)]
    sort_by: Option<String>,
    /// Comma-delimited [`SortOrder`](ferrofin_model::dto::SortOrder) directions.
    #[serde(default)]
    sort_order: Option<String>,
    /// Comma-delimited [`ItemFilter`](ferrofin_model::querying::ItemFilter) flags.
    #[serde(default)]
    filters: Option<String>,
    /// Comma-delimited [`ItemFields`](ferrofin_model::querying::ItemFields) to populate
    /// on each returned DTO (e.g. `Path`, `Genres`). Absent/empty ⇒ the base DTO.
    #[serde(default)]
    fields: Option<String>,
    /// Comma-delimited explicit item ids to fetch.
    #[serde(default)]
    ids: Option<String>,
    /// Comma-delimited item ids to exclude.
    #[serde(default)]
    exclude_item_ids: Option<String>,
    /// Pipe-delimited genre names.
    #[serde(default)]
    genres: Option<String>,
    /// Pipe-delimited tags.
    #[serde(default)]
    tags: Option<String>,
    /// Pipe-delimited official ratings.
    #[serde(default)]
    official_ratings: Option<String>,
    /// Comma-delimited production years.
    #[serde(default)]
    years: Option<String>,
    /// Comma-delimited genre ids.
    #[serde(default)]
    genre_ids: Option<String>,
    /// Comma-delimited studio ids.
    #[serde(default)]
    studio_ids: Option<String>,
    /// Comma-delimited person ids.
    #[serde(default)]
    person_ids: Option<String>,
    /// Comma-delimited artist ids.
    #[serde(default)]
    artist_ids: Option<String>,
    /// Comma-delimited album ids.
    #[serde(default)]
    album_ids: Option<String>,
    /// Restrict to favourited items.
    #[serde(default)]
    is_favorite: Option<bool>,
    /// Restrict to played / unplayed items.
    #[serde(default)]
    is_played: Option<bool>,
    /// Restrict to movies.
    #[serde(default)]
    is_movie: Option<bool>,
    /// Restrict to series.
    #[serde(default)]
    is_series: Option<bool>,
    /// Restrict to 4K items.
    #[serde(default, rename = "is4K")]
    is_4k: Option<bool>,
    /// Restrict to HD items. The alias covers jellyfin-web's stable filter
    /// dialog, which sends `IsHD` — the server's key fold only lowercases the
    /// first character, leaving `isHD`.
    #[serde(default, alias = "isHD")]
    is_hd: Option<bool>,
    /// Restrict to 3D items (jellyfin-web sends `Is3D` → `is3D`).
    #[serde(default, rename = "is3D")]
    is_3d: Option<bool>,
    /// Comma-delimited [`VideoType`](ferrofin_model::entities::VideoType) set
    /// (`BluRay`, `Dvd`, `Iso`).
    #[serde(default)]
    video_types: Option<String>,
    /// Restrict to items with (or without) subtitle streams.
    #[serde(default)]
    has_subtitles: Option<bool>,
    /// Restrict to items with (or without) a local trailer extra.
    #[serde(default)]
    has_trailer: Option<bool>,
    /// Restrict to items with (or without) a special-feature extra.
    #[serde(default)]
    has_special_feature: Option<bool>,
    /// Restrict to items with (or without) a theme song extra.
    #[serde(default)]
    has_theme_song: Option<bool>,
    /// Restrict to items with (or without) a theme video extra.
    #[serde(default)]
    has_theme_video: Option<bool>,
    /// Exact index number.
    #[serde(default)]
    index_number: Option<i32>,
    /// Exact parent index number.
    #[serde(default)]
    parent_index_number: Option<i32>,
    /// Minimum community rating.
    #[serde(default)]
    min_community_rating: Option<f64>,
    /// Minimum critic rating.
    #[serde(default)]
    min_critic_rating: Option<f64>,
    /// Restrict to items whose name starts with this value.
    #[serde(default)]
    name_starts_with: Option<String>,
    /// Restrict to items whose name sorts at or after this value.
    #[serde(default)]
    name_starts_with_or_greater: Option<String>,
    /// Restrict to items whose name sorts before this value.
    #[serde(default)]
    name_less_than: Option<String>,
    /// A single person name filter.
    #[serde(default)]
    person: Option<String>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
}

/// `GET /Items` — a paged, filtered, user-scoped library query.
///
/// Port of `ItemsController.GetItems`. The wide query is mapped onto
/// [`InternalItemsQuery`]; the collection-type dispatch and search-provider
/// ranking that need the un-ported `Folder` OOP tree are deferred, but every
/// persistable filter is honoured. A non-recursive `parentId` browse of a
/// box-set or playlist surfaces its manual `LinkedChildren` members (the SQL
/// merge that mirrors C# `Folder.GetChildren`; see `translate_query`).
#[utoipa::path(
    get,
    path = "/Items",
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `ferrofin-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses((status = 200, description = "Items returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;

    // The user row moves into the query and is borrowed back out for the DTO
    // projection below — `resolve_user` already cloned it off the auth context,
    // and a second full copy of every string on it buys nothing.
    let mut internal = InternalItemsQuery {
        user: Some(user),
        start_index: query.start_index,
        limit: query.limit,
        recursive: query.recursive.unwrap_or(false),
        search_term: query.search_term.clone(),
        include_item_types: parse_csv_enums_lenient(query.include_item_types.as_deref()),
        exclude_item_types: parse_csv_enums_lenient(query.exclude_item_types.as_deref()),
        media_types: parse_csv_enums_lenient(query.media_types.as_deref()),
        order_by: parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref()),
        item_ids: parse_csv_uuids(query.ids.as_deref())?,
        exclude_item_ids: parse_csv_uuids(query.exclude_item_ids.as_deref())?,
        genres: parse_pipe_strings(query.genres.as_deref()),
        tags: parse_pipe_strings(query.tags.as_deref()),
        official_ratings: parse_pipe_strings(query.official_ratings.as_deref()),
        years: parse_csv_i32(query.years.as_deref())?,
        genre_ids: parse_csv_uuids(query.genre_ids.as_deref())?,
        studio_ids: parse_csv_uuids(query.studio_ids.as_deref())?,
        person_ids: parse_csv_uuids(query.person_ids.as_deref())?,
        artist_ids: parse_csv_uuids(query.artist_ids.as_deref())?,
        album_ids: parse_csv_uuids(query.album_ids.as_deref())?,
        is_favorite: query.is_favorite,
        is_played: query.is_played,
        is_movie: query.is_movie,
        is_series: query.is_series,
        is_4k: query.is_4k,
        is_hd: query.is_hd,
        is_3d: query.is_3d,
        video_types: parse_csv_enums_lenient(query.video_types.as_deref()),
        has_subtitles: query.has_subtitles,
        has_trailer: query.has_trailer,
        has_special_feature: query.has_special_feature,
        has_theme_song: query.has_theme_song,
        has_theme_video: query.has_theme_video,
        index_number: query.index_number,
        parent_index_number: query.parent_index_number,
        min_community_rating: query.min_community_rating,
        min_critic_rating: query.min_critic_rating,
        name_starts_with: query.name_starts_with.clone(),
        name_starts_with_or_greater: query.name_starts_with_or_greater.clone(),
        name_less_than: query.name_less_than.clone(),
        person: query.person.clone(),
        enable_total_record_count: query.enable_total_record_count.unwrap_or(true),
        ..InternalItemsQuery::default()
    };
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }
    // C# `ItemsController.GetItems` (ItemsController.cs:307, 525-529) answers a
    // non-recursive, id-less request whose parent resolves to the
    // `UserRootFolder` with `folder.GetChildren(user, true)` — not with a query.
    // The controller owns that decision; the repository applies it only if the
    // parent really is the user root (an ABSENT `parentId` is, per
    // `LibraryManager.GetParentItem(null, userId)`).
    internal.user_root_children = !internal.recursive && internal.item_ids.is_empty();
    // C# `ApplyFilters` translates the `filters` flag set onto the tri-state
    // fields, rejecting contradictory pairs with a `400`. Token parsing is
    // lenient + case-insensitive like ASP.NET's binder: jellyfin-web sends
    // `Filters=IsUnPlayed` (sic), and upstream drops unknown tokens.
    let filters = parse_csv_enums_lenient(query.filters.as_deref());
    internal
        .apply_filters(&filters)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    // A BoxSet/Playlist-typed browse under a normal library is re-rooted: box
    // sets live outside the library tree, so the parent becomes a
    // linked-child-ancestor constraint instead (C# ItemsController's
    // `linkedChildAncestorIds` redirect). Without this, the library's
    // Collections tab can never list anything.
    redirect_container_browse(&state, &mut internal).await;

    let result = state.library.query_items(&internal).await?;
    // Honour the requested `Fields` (Path, Genres, …) — Jellyfin's GetItems builds its
    // DtoOptions from them. Lenient parse: clients still send deprecated field names, which
    // Jellyfin drops rather than 400ing. Absent ⇒ empty ⇒ the base DTO (matches Jellyfin).
    let options = DtoOptions {
        fields: parse_csv_enums_lenient(query.fields.as_deref()),
        ..DtoOptions::default()
    };
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, internal.user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(
        Some(result.start_index),
        Some(result.total_record_count),
        dtos,
    )))
}

/// Query parameters for `GET /Items/{itemId}`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// `GET /Items/{itemId}` — a single item by id.
///
/// Port of `UserLibraryController.GetItem`. A missing item (or user) is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Item returned (BaseItemDto)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    // A single-item fetch backs a detail page, so return the full DTO (overview,
    // genres, people, studios, tags, …) — the field-gated data jellyfin-web's
    // detail view needs. Port of `UserLibraryController.GetItem`, which builds a
    // `DtoOptions` with all fields.
    let options = DtoOptions::with_all_fields(true);
    let dto = state
        .dto
        .get_base_item_dto(&item, &options, Some(&user), None)
        .await?;
    Ok(Json(dto))
}

/// `DELETE /Items/{itemId}` — deletes one item from the library.
///
/// Port of `LibraryController.DeleteItem`. A missing item is a `404`; on success
/// the item (and, for a folder, its subtree) is removed and the handler returns
/// `204`. Physical file deletion is the filesystem layer's job (deferred), so the
/// portable seam deletes the rows only.
#[utoipa::path(
    delete,
    path = "/Items/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Item deleted"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn delete_item(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // Confirm the item exists first so a bogus id is a `404` (C# `GetItemById`
    // null-check) rather than a silently-idempotent `204`.
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    state
        .library
        .delete_item(item_id, &DeleteOptions::default())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `DELETE /Items`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteItemsQuery {
    /// Comma-delimited ids of the items to delete.
    #[serde(default)]
    ids: Option<String>,
}

/// `DELETE /Items` — deletes several items by id.
///
/// Port of `LibraryController.DeleteItems`. Each id must resolve or the whole
/// request is a `404` (matching C#'s per-item `NotFound()` short-circuit).
#[utoipa::path(
    delete,
    path = "/Items",
    responses(
        (status = 204, description = "Items deleted"),
        (status = 404, description = "An item was not found")
    ),
    tag = "ferrofin"
)]
async fn delete_items(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<DeleteItemsQuery>,
) -> Result<StatusCode, ApiError> {
    let ids = parse_csv_uuids(query.ids.as_deref())?;
    for id in ids {
        state
            .library
            .get_item_by_id(id)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("item {id}")))?;
        state
            .library
            .delete_item(id, &DeleteOptions::default())
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `GET /Items/Counts`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CountsQuery {
    /// Optional user whose library the counts are scoped to.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Optional favourite-only filter.
    #[serde(default)]
    is_favorite: Option<bool>,
}

/// `GET /Items/Counts` — per-kind item counts.
///
/// Port of `LibraryController.GetItemCounts`.
#[utoipa::path(
    get,
    path = "/Items/Counts",
    responses((status = 200, description = "Item counts returned (ItemCounts)", body = ItemCounts)),
    tag = "ferrofin"
)]
async fn get_item_counts(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<CountsQuery>,
) -> Result<Json<ItemCounts>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let internal = InternalItemsQuery {
        recursive: true,
        is_virtual_item: Some(false),
        is_favorite: query.is_favorite,
        user,
        ..InternalItemsQuery::default()
    };
    let counts = state.library.get_item_counts(&internal).await?;
    Ok(Json(counts))
}

/// Query parameters for `GET /Items/{itemId}/Ancestors`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AncestorsQuery {
    /// Optional user to scope visibility and attach user data.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// The stored `BaseItems.Type` short name of a row (`AggregateFolder`,
/// `CollectionFolder`, …).
fn short_type(item: &BaseItemEntity) -> &str {
    item.type_.rsplit('.').next().unwrap_or(&item.type_)
}

/// The children of the `UserRootFolder` that are the aggregate's VIRTUAL
/// children rather than its own rows — the plug-in folders
/// `LibraryManager.CreateRootFolder` registers with `AddVirtualChild`
/// (LibraryManager.cs:883, the only call site in 10.11.8: the playlists folder).
///
/// Named as a set because that is what it is in the C#: they are members of the
/// candidate list `TranslateParentItem` searches, and their
/// `BaseItem.PhysicalLocations` is `[Path]` (BaseItem.cs:450-461), so the search
/// matches such a folder against ITSELF.
const AGGREGATE_VIRTUAL_CHILD_TYPES: [&str; 3] = [
    "PlaylistsFolder",
    "ManualPlaylistsFolder",
    "BasePluginFolder",
];

/// Port of `LibraryController.TranslateParentItem` (LibraryController.cs:959-966):
///
/// ```text
/// item.GetParent() is AggregateFolder
///     ? _libraryManager.GetUserRootFolder().GetChildren(user, true)
///         .FirstOrDefault(i => i.PhysicalLocations.Contains(item.Path))
///     : item
/// ```
///
/// A hop whose parent is NOT the aggregate passes through untouched. A hop under
/// the aggregate is looked up in the user root's children — and `GetChildren`
/// there is the concatenated set (`UserRootFolder.cs:96-102`), so the candidates
/// are BOTH groups:
///
/// 1. the user root's own children, the `CollectionFolder` views, whose
///    `PhysicalLocations` are the library's configured locations — a physical
///    library root under the aggregate (as in a database scanned by Jellyfin) is
///    therefore shown as the view containing it;
/// 2. the aggregate's virtual children, whose `PhysicalLocations` is `[Path]` —
///    such a folder matches itself, so the walk continues THROUGH it. Ferrofin
///    used to leave that group out of the candidate set entirely, and a
///    playlist's ancestor chain came back EMPTY where Jellyfin answers
///    `[Playlists, root]`.
///
/// The two groups are disjoint by construction (a library location is never the
/// plug-in folder's own `{data}/playlists` path), so group 2 is tested first —
/// it needs no query.
///
/// `None` is the C# `FirstOrDefault` miss, which ends the walk.
async fn translate_parent_item(
    state: &AppState,
    item: &BaseItemEntity,
    grandparent: Option<&BaseItemEntity>,
) -> Result<Option<BaseItemEntity>, ApiError> {
    if grandparent.is_none_or(|g| short_type(g) != "AggregateFolder") {
        return Ok(Some(item.clone()));
    }
    let Some(path) = item.path.as_deref() else {
        return Ok(None);
    };
    // Candidate group 2: `PhysicalLocations == [Path]`, so `Contains(item.Path)`
    // is true of the item itself.
    if AGGREGATE_VIRTUAL_CHILD_TYPES.contains(&short_type(item)) {
        return Ok(Some(item.clone()));
    }
    // Candidate group 1: the `CollectionFolder` views.
    let folders = state.virtual_folders.get_virtual_folders().await?;
    let Some(view_id) = folders
        .iter()
        .find(|vf| vf.locations.iter().any(|loc| loc == path))
        .and_then(|vf| vf.item_id.as_deref())
        .and_then(|id| Uuid::parse_str(id).ok())
    else {
        return Ok(None);
    };
    Ok(state.library.get_item_by_id(view_id).await?)
}

/// `GET /Items/{itemId}/Ancestors` — an item's parents, nearest first.
///
/// Port of `LibraryController.GetAncestors`: walks `GetParent()` from the
/// item, translating each hop through [`translate_parent_item`] when a user
/// is in scope (a physical root becomes the user's view, whose own parent is
/// the `UserRootFolder`), and stops at the first hop that translates to
/// nothing. A missing item is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Ancestors",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Ancestors returned (Vec<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn get_ancestors(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<AncestorsQuery>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let mut chain = state
        .library
        .get_ancestors(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let mut ancestors: Vec<BaseItemEntity> = Vec::with_capacity(chain.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut index = 0;
    while let Some(parent) = chain.get(index) {
        let parent = if user.is_some() {
            match translate_parent_item(&state, parent, chain.get(index + 1)).await? {
                Some(translated) => translated,
                None => break,
            }
        } else {
            parent.clone()
        };
        if !seen.insert(parent.id.to_ascii_uppercase()) {
            break;
        }
        // A translated hop re-roots the walk: the view's parents (the user
        // root), not the physical folder's (the aggregate root), come next.
        let translated = !parent.id.eq_ignore_ascii_case(&chain[index].id);
        let translated_id = Uuid::parse_str(&parent.id).ok();
        ancestors.push(parent);
        if translated && let Some(id) = translated_id {
            chain = state.library.get_ancestors(id).await?.unwrap_or_default();
            index = 0;
        } else {
            index += 1;
        }
    }
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&ancestors, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(dtos))
}

/// Query parameters for `GET /UserItems/Resume`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The index of the first item to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of items to return.
    #[serde(default)]
    limit: Option<i32>,
    /// A free-text search term.
    #[serde(default)]
    search_term: Option<String>,
    /// Localizes the query to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// Comma-delimited [`MediaType`](ferrofin_model::data::MediaType) set.
    #[serde(default)]
    media_types: Option<String>,
    /// Comma-delimited kinds to exclude.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited kinds to include.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Whether to compute the total record count (defaults `true`).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
}

/// `GET /UserItems/Resume` — the user's resumable (in-progress) items.
///
/// Port of `ItemsController.GetResumeItems`. The query is date-played-descending,
/// resumable, non-virtual, recursive; the excluded-active-session and
/// latest-item-exclude folder walks (which need the session list / the OOP child
/// tree) are deferred, so this returns every in-progress item the flat query
/// yields.
#[utoipa::path(
    get,
    path = "/UserItems/Resume",
    responses((status = 200, description = "Resumable items returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_resume_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ResumeQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;

    let user = resolve_user(&state, &auth, query.user_id).await?;
    // The user row moves into the query and is borrowed back out for the DTO
    // projection below — `resolve_user` already cloned it off the auth context,
    // and a second full copy of every string on it buys nothing.
    let mut internal = InternalItemsQuery {
        user: Some(user),
        start_index: query.start_index,
        limit: query.limit,
        recursive: true,
        is_resumable: Some(true),
        is_virtual_item: Some(false),
        collapse_box_set_items: Some(false),
        include_owned_items: true,
        search_term: query.search_term.clone(),
        media_types: parse_csv_enums_lenient(query.media_types.as_deref()),
        include_item_types: parse_csv_enums_lenient(query.include_item_types.as_deref()),
        exclude_item_types: parse_csv_enums_lenient(query.exclude_item_types.as_deref()),
        order_by: vec![(ItemSortBy::DatePlayed, SortOrder::Descending)],
        enable_total_record_count: query.enable_total_record_count.unwrap_or(true),
        ..InternalItemsQuery::default()
    };
    if let Some(parent) = query.parent_id {
        internal.parent_id = parent;
    }

    let result = state.library.query_items(&internal).await?;
    let options = DtoOptions::with_all_fields(false);
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, internal.user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(
        Some(result.start_index),
        Some(result.total_record_count),
        dtos,
    )))
}

/// Parses a comma-delimited list of `i32` values (Jellyfin's `years`).
fn parse_csv_i32(raw: Option<&str>) -> Result<Vec<i32>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i32>()
                .map_err(|_| ApiError::BadRequest(format!("invalid integer {s:?}")))
        })
        .collect()
}

/// Builds the `order_by` list from parallel `sort_by`/`sort_order` lists.
///
/// Mirrors `RequestHelpers.GetOrderBy`: each sort column is paired with the
/// order at the same index, falling back to the last supplied order (then
/// ascending) when fewer orders than columns are given.
pub(crate) fn parse_order_by(
    sort_by: Option<&str>,
    sort_order: Option<&str>,
) -> Vec<(
    ferrofin_model::live_tv::ItemSortBy,
    ferrofin_model::dto::SortOrder,
)> {
    use ferrofin_model::dto::SortOrder;
    let columns: Vec<ferrofin_model::live_tv::ItemSortBy> = parse_csv_enums_lenient(sort_by);
    let orders: Vec<SortOrder> = parse_csv_enums_lenient(sort_order);
    columns
        .into_iter()
        .enumerate()
        .map(|(i, column)| {
            let order = orders
                .get(i)
                // C# RequestHelpers.GetOrderBy pads missing orders with the
                // FIRST requested order, not the last.
                .or_else(|| orders.first())
                .copied()
                .unwrap_or(SortOrder::Ascending);
            (column, order)
        })
        .collect()
}

/// Re-roots a BoxSet/Playlist-typed browse from a normal library parent onto a
/// linked-child-ancestor constraint (port of `ItemsController.GetItems`'s
/// `linkedChildAncestorIds` block).
///
/// Applies only when the request is for exactly `[BoxSet]` or `[Playlist]`,
/// a parent is set, and that parent is neither itself a box set/playlist nor
/// the matching `boxsets`/`playlists` library. Best-effort: an unresolvable
/// parent leaves the query untouched (the plain parent scoping then returns
/// empty, as before).
async fn redirect_container_browse(
    state: &AppState,
    internal: &mut ferrofin_traits::options::InternalItemsQuery,
) {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::CollectionTypeOptions;

    // Playlists have no `CollectionTypeOptions` variant (upstream's options
    // enum has none either — the playlists "library" is the manual playlists
    // folder, caught by the container check below), so its target type is None.
    let target_collection_type = match internal.include_item_types.as_slice() {
        [BaseItemKind::BoxSet] => Some(CollectionTypeOptions::boxsets),
        [BaseItemKind::Playlist] => None,
        _ => return,
    };
    if internal.parent_id.is_nil() {
        return;
    }
    let Ok(Some(parent)) = state.library.get_item_by_id(internal.parent_id).await else {
        return;
    };
    // The parent is itself a container of the requested kind → a direct
    // children browse, no re-rooting (C# `item is not BoxSet/Playlist`).
    let short = parent.type_.rsplit('.').next().unwrap_or(&parent.type_);
    if short == "BoxSet" || short == "Playlist" || short == "ManualPlaylistsFolder" {
        return;
    }
    // A browse of the boxsets/playlists library itself keeps plain parent
    // scoping (C# `itemCollectionType != targetCollectionType`).
    let parent_collection_type = state
        .virtual_folders
        .get_virtual_folders()
        .await
        .ok()
        .and_then(|folders| {
            folders
                .into_iter()
                .find(|vf| {
                    vf.item_id
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&parent.id))
                })
                .and_then(|vf| vf.collection_type)
        });
    if target_collection_type.is_some() && parent_collection_type == target_collection_type {
        return;
    }
    internal.linked_child_ancestor_ids = vec![internal.parent_id];
    internal.set_parent(None);
}

/// Registers this controller's real routes onto `router`.
///
/// The bare `/Items/{itemId}` slot carries `GET`/`DELETE` (this controller) and
/// `POST` (the item-update controller) on one shared `MethodRouter`, since axum
/// rejects a duplicate method+path registered across two `route` calls.
/// `GET /Users/{userId}/Items` — the path-scoped form of `GET /Items`.
///
/// These `/Users/{userId}/Items…` forms aren't in the 10.11 OpenAPI contract
/// (which prefers `/Items?userId=`), but jellyfin-web's bundled `jellyfin-apiclient`
/// still calls them for core screens (home rows, the item/metadata views), so they
/// are required — a `404` here breaks those screens. Each injects the path
/// `userId` into the query and forwards to the query-scoped handler.
async fn get_items_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    Query(mut query): Query<ItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    get_items(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/Items/Resume` — path-scoped form of `GET /UserItems/Resume`
/// (the home screen's "Continue watching" row).
async fn get_resume_items_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path(user_id): Path<Uuid>,
    Query(mut query): Query<ResumeQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    get_resume_items(state, auth, Query(query)).await
}

/// `GET /Users/{userId}/Items/{itemId}` — path-scoped form of `GET /Items/{itemId}`
/// (item detail + the library metadata editor).
async fn get_item_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
    Query(mut query): Query<ItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    query.user_id = Some(user_id);
    get_item(state, auth, Path(item_id), Query(query)).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items", get(get_items).delete(delete_items))
        .route("/Users/{userId}/Items", get(get_items_for_user))
        .route(
            "/Users/{userId}/Items/Resume",
            get(get_resume_items_for_user),
        )
        .route("/Users/{userId}/Items/{itemId}", get(get_item_for_user))
        .route("/Items/Counts", get(get_item_counts))
        .route("/UserItems/Resume", get(get_resume_items))
        .route(
            "/Items/{itemId}",
            get(get_item)
                .delete(delete_item)
                .post(super::item_update::update_item),
        )
        .route("/Items/{itemId}/Ancestors", get(get_ancestors))
}
#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::querying::ItemFields;

    // GET /Items must honour the camelCase `fields` param (the OpenAPI contract's casing, what
    // jellyfin-web sends) and map it onto DtoOptions. Regression for the bug where the handler
    // hardcoded `with_all_fields(false)` and had no `fields` member, dropping every requested
    // field (e.g. Path) — which made item DTOs diverge from Jellyfin.
    #[test]
    fn items_query_fields_maps_onto_dto_options() {
        let q: ItemsQuery =
            serde_urlencoded::from_str("recursive=true&fields=Path,Genres").expect("parses");
        assert_eq!(q.fields.as_deref(), Some("Path,Genres"));
        let options = DtoOptions {
            fields: parse_csv_enums_lenient(q.fields.as_deref()),
            ..DtoOptions::default()
        };
        assert!(options.contains_field(ItemFields::Path));
        assert!(options.contains_field(ItemFields::Genres));

        // No `fields` ⇒ the base DTO (empty field set), matching Jellyfin's default GetItems.
        let base: ItemsQuery = serde_urlencoded::from_str("recursive=true").expect("parses");
        let base_opts = DtoOptions {
            fields: parse_csv_enums_lenient(base.fields.as_deref()),
            ..DtoOptions::default()
        };
        assert!(!base_opts.contains_field(ItemFields::Path));
    }
}
