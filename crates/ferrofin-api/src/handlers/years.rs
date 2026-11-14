//! `YearsController` — browse production years and resolve one by value.
//!
//! Ports:
//!
//! - `GET /Years` — the library's production years as a
//!   [`QueryResult<BaseItemDto>`].
//! - `GET /Years/{year}` — a single year by value.
//!
//! `GET /Years` reads the year rows from
//! [`LibraryManager::get_years`](ferrofin_traits::library::LibraryManager::get_years)
//! and projects them like the other by-name lists (`by_name::project_item_rows`,
//! over `DtoService::get_base_item_dtos`); `GET /Years/{year}` projects its
//! year with `DtoService::get_base_item_dto`, as upstream's `GetYear` does.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::by_name::{additional_dto_options, project_item_rows, resolve_parent};
use crate::handlers::items::resolve_user_or_api_key;
use crate::handlers::query_parse::{parse_csv_enums_lenient, parse_order_by};
use crate::state::AppState;

/// The query parameters honoured by `GET /Years`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct YearsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Localizes the browse to a specific parent when set.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Whether to search descendants recursively (Jellyfin defaults to `true`).
    #[serde(default)]
    recursive: Option<bool>,
    /// Comma-delimited sort keys (C# `sortBy`), paired with [`Self::sort_order`].
    #[serde(default)]
    sort_by: Option<String>,
    /// Comma-delimited sort directions (C# `sortOrder`).
    #[serde(default)]
    sort_order: Option<String>,
    /// Comma-delimited `BaseItemKind`s the year extraction is restricted to.
    #[serde(default)]
    include_item_types: Option<String>,
    /// Comma-delimited `BaseItemKind`s the year extraction excludes.
    #[serde(default)]
    exclude_item_types: Option<String>,
    /// Comma-delimited media types the year extraction is restricted to.
    #[serde(default)]
    media_types: Option<String>,
    /// Comma-delimited [`ItemFields`](ferrofin_model::querying::ItemFields) to
    /// populate on each DTO. Absent/empty ⇒ the base DTO.
    #[serde(default)]
    fields: Option<String>,
    /// Whether image information is populated (C# default `true`).
    #[serde(default)]
    enable_images: Option<bool>,
    /// The maximum number of images to return, per image type.
    #[serde(default)]
    image_type_limit: Option<i32>,
    /// Comma-delimited [`ImageType`](ferrofin_model::entities::ImageType) set to
    /// populate. Empty ⇒ every type, as upstream.
    #[serde(default)]
    enable_image_types: Option<String>,
    /// Whether user data is populated.
    #[serde(default)]
    enable_user_data: Option<bool>,
}

/// `GET /Years` — the library's production years.
///
/// Port of `YearsController.GetYears`.
#[utoipa::path(
    get,
    path = "/Years",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Years returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_years(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<YearsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_or_api_key(&state, &auth, query.user_id).await?;
    let recursive = query.recursive.unwrap_or(true);
    // C# `GetParentItem(parentId, userId)` falls back to the **user root
    // folder** when no parent was named, and `recursive: false` then walks that
    // root's direct children — the library folders, which carry no
    // `ProductionYear`, so an unscoped `recursive=false` legitimately returns
    // nothing. Leaving the parent unset made it return every year in the
    // library instead. The root is only resolved when it can change the answer.
    // With no user (an API key) upstream walks the `AggregateFolder`'s
    // children instead, the library folders again, and answers the same
    // nothing.
    //
    // A `parentId` naming no item, the zero id included, is `GetParentItem`'s
    // `ArgumentException`: a 400.
    let parent = resolve_parent(&state, &auth, query.parent_id, user.as_deref()).await?;
    let playlist = parent.as_ref().is_some_and(|row| {
        BaseItemKind::from_stored_type_name(&row.type_) == Some(BaseItemKind::Playlist)
    });
    // A parent that is not a folder (`!parentItem.IsFolder`, the stored
    // `IsFolder`) is the only item: `new[] { parentItem }.Where(Filter)`
    // (v12.4 `YearsController.cs:111-128`), so the answer is its own year,
    // if the type and media-type filters keep it, whether or not it is an
    // alternate version or an owned item.
    let only_item = parent.as_ref().is_some_and(|row| !row.is_folder);
    let parent_id = match query.parent_id {
        Some(_) if only_item => Uuid::nil(),
        Some(parent) => parent,
        None if !recursive => state
            .library
            .get_user_root_folder()
            .await?
            .and_then(|row| Uuid::parse_str(&row.id).ok())
            .unwrap_or_else(Uuid::nil),
        None => Uuid::nil(),
    };
    // `FilterItem()` upstream: the year extraction sees only the items that
    // survive the type/media-type filters, so `?includeItemTypes=Series` yields
    // the years of the series and nothing else.
    let include_item_types = parse_csv_enums_lenient(query.include_item_types.as_deref());
    let exclude_item_types = parse_csv_enums_lenient(query.exclude_item_types.as_deref());
    let media_types = parse_csv_enums_lenient(query.media_types.as_deref());
    // The user row moves into the query and is borrowed back out for the DTO
    // projection below — it is the auth context's shared row
    // (`resolve_user_or_api_key`), and the query holds the same handle.
    let mut internal = ferrofin_traits::options::InternalItemsQuery {
        user,
        start_index: query.start_index,
        limit: query.limit,
        recursive,
        parent_id,
        item_ids: query.parent_id.filter(|_| only_item).into_iter().collect(),
        include_owned_items: only_item,
        include_item_types,
        exclude_item_types,
        media_types,
        order_by: parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref()),
        ..ferrofin_traits::options::InternalItemsQuery::default()
    };
    if playlist {
        playlist_children(&mut internal);
    }
    let result = state.library.get_years(&internal).await?;
    let options = additional_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.enable_user_data,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
    );
    let projected = project_item_rows(&state, result, &options, internal.user.as_deref()).await?;
    Ok(Json(projected))
}

/// The children `GetYears` walks under a playlist (v12.4
/// `YearsController.cs:112-123`). With a user, `GetRecursiveChildren(user, …)`
/// and `GetChildren(user, true)` both reach `Playlist.GetPlayableItems`
/// (`Playlist.cs:142-171`): its linked entries, never recursing, less every
/// folder entry. With none, a recursive walk takes every linked entry and
/// what is under it, which the query already does, and a non-recursive one
/// takes `Children`, which a playlist never has (`LoadChildren`, `:131`).
///
/// `GET /Items?parentId=<playlist>` reaches neither override: its
/// `Folder.GetItemsInternal` calls the `GetChildren` overload with a count,
/// which `Playlist` does not override, so its folder entries are listed
/// there, as in Ferrofin.
fn playlist_children(query: &mut ferrofin_traits::options::InternalItemsQuery) {
    if query.user.is_some() {
        query.recursive = false;
        query.is_folder = Some(false);
    } else if !query.recursive {
        query.physical_children_only = true;
    }
}

/// The query parameters for `GET /Years/{year}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct YearQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    user_id: Option<Uuid>,
}

/// `GET /Years/{year}` — a single year by value.
///
/// Port of `YearsController.GetYear`. A year with no materialized row is a
/// `404`; a **non-positive** year is a `400`, because
/// `LibraryManager.GetYear` throws `ArgumentOutOfRangeException("Years less
/// than or equal to 0 are invalid.")` before it ever reaches the controller's
/// `NotFound()` (`LibraryManager.cs:1020-1030`), and ASP.NET renders that as
/// `500`/`400` rather than a not-found. Measured against 10.11.8: `/Years/0`
/// and `/Years/-1` are `400` there and were `404` here.
#[utoipa::path(
    get,
    path = "/Years/{year}",
    params(("year" = i32, Path, description = "The production year")),
    responses(
        (status = 200, description = "Year returned (BaseItemDto)"),
        (status = 400, description = "Year less than or equal to 0"),
        (status = 404, description = "Year not found")
    ),
    tag = "ferrofin"
)]
async fn get_year(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(year): Path<i32>,
    Query(query): Query<YearQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user_or_api_key(&state, &auth, query.user_id).await?;
    if year <= 0 {
        return Err(ApiError::BadRequest(format!(
            "years less than or equal to 0 are invalid: {year}"
        )));
    }
    let item = state
        .library
        .get_named_item(BaseItemKind::Year, &year.to_string())
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("year {year}")))?;
    let options = DtoOptions::default();
    let dto = state
        .dto
        .get_base_item_dto(&item, &options, user.as_deref(), None)
        .await?;
    Ok(Json(dto))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Years", get(get_years))
        .route("/Years/{year}", get(get_year))
}
crate::query::query_parameters! {
    YearsQuery {
        "sortBy" => ',',
        "sortOrder" => ',',
        "includeItemTypes" => ',',
        "excludeItemTypes" => ',',
        "mediaTypes" => ',',
        "fields" => ',',
        "enableImageTypes" => ',',
    } => [("get", "/Years")];
    YearQuery {} => [("get", "/Years/{year}")];
}
