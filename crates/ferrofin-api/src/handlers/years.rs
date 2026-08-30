//! `YearsController` — browse production years and resolve one by value.
//!
//! Ports:
//!
//! - `GET /Years` — the library's production years as a
//!   [`QueryResult<BaseItemDto>`].
//! - `GET /Years/{year}` — a single year by value.
//!
//! Each year is projected through [`DtoService::get_item_by_name_dto`] like the
//! other by-name browses; the year rows come from
//! [`LibraryManager::get_years`](ferrofin_traits::library::LibraryManager::get_years).

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::by_name::{additional_dto_options, project_item_rows};
use crate::handlers::items::{parse_order_by, resolve_user};
use crate::handlers::query_parse::parse_csv_enums_lenient;
use crate::state::AppState;

/// The query parameters honoured by `GET /Years`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct YearsQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Localizes the browse to a specific parent when set.
    #[serde(default)]
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
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let recursive = query.recursive.unwrap_or(true);
    // C# `GetParentItem(parentId, userId)` falls back to the **user root
    // folder** when no parent was named, and `recursive: false` then walks that
    // root's direct children — the library folders, which carry no
    // `ProductionYear`, so an unscoped `recursive=false` legitimately returns
    // nothing. Leaving the parent unset made it return every year in the
    // library instead. The root is only resolved when it can change the answer.
    let parent_id = match query.parent_id.filter(|p| !p.is_nil()) {
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
    // projection below — `resolve_user` already cloned it off the auth context,
    // and a second full copy of every string on it buys nothing.
    let internal = ferrofin_traits::options::InternalItemsQuery {
        user: Some(user),
        start_index: query.start_index,
        limit: query.limit,
        recursive,
        parent_id,
        include_item_types,
        exclude_item_types,
        media_types,
        order_by: parse_order_by(query.sort_by.as_deref(), query.sort_order.as_deref()),
        ..ferrofin_traits::options::InternalItemsQuery::default()
    };
    let result = state.library.get_years(&internal).await?;
    let options = additional_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.enable_user_data,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
    );
    let projected = project_item_rows(&state, result, &options, internal.user.as_ref()).await?;
    Ok(Json(projected))
}

/// The query parameters for `GET /Years/{year}`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct YearQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
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
    let user = resolve_user(&state, &auth, query.user_id).await?;
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
        .get_base_item_dto(&item, &options, Some(&user), None)
        .await?;
    Ok(Json(dto))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Years", get(get_years))
        .route("/Years/{year}", get(get_year))
}
