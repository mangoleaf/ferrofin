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
use crate::handlers::items::resolve_user;
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
    let mut ancestor_ids = Vec::new();
    if let Some(parent) = query.parent_id {
        ancestor_ids.push(parent);
    }
    // The user row moves into the query and is borrowed back out for the DTO
    // projection below — `resolve_user` already cloned it off the auth context,
    // and a second full copy of every string on it buys nothing.
    let internal = ferrofin_traits::options::InternalItemsQuery {
        user: Some(user),
        start_index: query.start_index,
        limit: query.limit,
        recursive: query.recursive.unwrap_or(true),
        ancestor_ids,
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
/// Port of `YearsController.GetYear`. A non-positive year, or one with no
/// materialized row, is a `404` (Jellyfin rejects `year <= 0` and returns
/// `NotFound` when the year is absent).
#[utoipa::path(
    get,
    path = "/Years/{year}",
    params(("year" = i32, Path, description = "The production year")),
    responses(
        (status = 200, description = "Year returned (BaseItemDto)"),
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
        return Err(ApiError::NotFound(format!("year {year}")));
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
