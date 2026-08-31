//! `PersonsController` — browse people and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Persons` — the library's people as a [`QueryResult<BaseItemDto>`].
//! - `GET /Persons/{name}` — a single person by name.
//!
//! The people list resolves each credited person (from the people repository)
//! to its by-name `Person` item, mirroring `ILibraryManager.GetPeopleItems`. The
//! per-name image routes are Batch 9.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::{ItemFilter, QueryResult};
use ferrofin_traits::options::{DtoOptions, InternalPeopleQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::by_name::{ByNameItemQuery, additional_dto_options, project_item_rows};
use crate::handlers::items::{resolve_user, user_uuid};
use crate::handlers::query_parse::parse_csv_enums_lenient;
use crate::state::AppState;

/// The query parameters honoured by `GET /Persons`.
///
/// The wider Jellyfin query (image/user-data toggles) is accepted but only the
/// name/paging/person filters change which people come back.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonsQuery {
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
    /// A case-insensitive term the person name must contain.
    #[serde(default)]
    search_term: Option<String>,
    /// Restrict to people whose name starts with this value.
    #[serde(default)]
    name_starts_with: Option<String>,
    /// Restrict to people whose name sorts before this value.
    #[serde(default)]
    name_less_than: Option<String>,
    /// Restrict to people whose name sorts at or after this value.
    #[serde(default)]
    name_starts_with_or_greater: Option<String>,
    /// Restrict to people the caller has (not) favourited.
    #[serde(default)]
    is_favorite: Option<bool>,
    /// Comma-delimited [`ItemFilter`] flags (`IsFavorite`, …).
    #[serde(default)]
    filters: Option<String>,
    /// Restrict to people appearing in this item.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    appears_in_item_id: Option<Uuid>,
    /// Localizes the browse to a specific parent when set.
    #[serde(
        default,
        deserialize_with = "crate::handlers::query_parse::empty_as_none_uuid"
    )]
    parent_id: Option<Uuid>,
    /// Comma-delimited person types to include (`Actor`, `Director`, …).
    #[serde(default)]
    person_types: Option<String>,
    /// Comma-delimited person types to exclude.
    #[serde(default)]
    exclude_person_types: Option<String>,
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

/// Splits a comma-delimited query value into trimmed, non-empty items.
fn comma_list(value: Option<&str>) -> Vec<String> {
    value
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// `GET /Persons` — the library's people.
///
/// Port of `PersonsController.GetPersons`.
#[utoipa::path(
    get,
    path = "/Persons",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Persons returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_persons(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<PersonsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = user_uuid(&user)?;
    // C# folds `filters ∋ IsFavorite` onto the tri-state when `isFavorite` is
    // absent (PersonsController: `!isFavorite.HasValue && isFavoriteInFilters`).
    let filters = parse_csv_enums_lenient::<ItemFilter>(query.filters.as_deref());
    let is_favorite = query
        .is_favorite
        .or_else(|| filters.contains(&ItemFilter::IsFavorite).then_some(true));
    let people_query = InternalPeopleQuery {
        start_index: query.start_index,
        limit: query.limit.unwrap_or(0),
        parent_id: query.parent_id,
        person_types: comma_list(query.person_types.as_deref()),
        exclude_person_types: comma_list(query.exclude_person_types.as_deref()),
        appears_in_item_id: query.appears_in_item_id.unwrap_or_else(Uuid::nil),
        name_contains: query.search_term.clone(),
        name_starts_with: query.name_starts_with.clone(),
        name_less_than: query.name_less_than.clone(),
        name_starts_with_or_greater: query.name_starts_with_or_greater.clone(),
        user_id: Some(user_id),
        is_favorite,
        ..InternalPeopleQuery::default()
    };
    let result = state.library.get_people_items(&people_query).await?;
    let options = additional_dto_options(
        query.fields.as_deref(),
        query.enable_images,
        query.enable_user_data,
        query.image_type_limit,
        query.enable_image_types.as_deref(),
    );
    // People carry no aggregated counts (Jellyfin passes none here).
    let projected = project_item_rows(&state, result, &options, Some(&user)).await?;
    Ok(Json(projected))
}

/// `GET /Persons/{name}` — a single person by name.
///
/// Port of `PersonsController.GetPerson`. A missing person is a `404`, matching
/// the C# `NotFound()`.
#[utoipa::path(
    get,
    path = "/Persons/{name}",
    params(("name" = String, Path, description = "The person name")),
    responses(
        (status = 200, description = "Person returned (BaseItemDto)"),
        (status = 404, description = "Person not found")
    ),
    tag = "ferrofin"
)]
async fn get_person(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(name): Path<String>,
    Query(query): Query<ByNameItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = state
        .library
        .get_named_item(BaseItemKind::Person, &name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("person {name}")))?;
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
        .route("/Persons", get(get_persons))
        .route("/Persons/{name}", get(get_person))
}
