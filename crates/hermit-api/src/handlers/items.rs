//! `ItemsController` / `UserLibraryController` — item queries and lookup.
//!
//! Ports two First-Light actions:
//!
//! - `GET /Items` — a paged query over the library, projected to
//!   [`BaseItemDto`]s and wrapped in a [`QueryResult`].
//! - `GET /Items/{itemId}` — a single item by id.
//!
//! Only the paging + user-scoping subset of Jellyfin's very wide `GetItems`
//! query is honoured here (`userId`, `startIndex`, `limit`, `recursive`); the
//! remaining filters register as accepted query parameters and are applied as
//! the query builder is ported.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_db::entities::users::UserEntity;
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;
use hermit_traits::options::{AuthorizationInfo, DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
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
    state
        .users
        .get_user_by_id(effective)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {effective}")))
}

/// The (paging) query parameters honoured by `GET /Items`.
///
/// The full Jellyfin query is far wider; the remaining parameters are accepted
/// but not yet applied.
#[derive(Debug, serde::Deserialize)]
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
}

/// `GET /Items` — a paged, user-scoped library query.
///
/// Port of `ItemsController.GetItems` (paging subset).
#[utoipa::path(
    get,
    path = "/Items",
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `hermit-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses((status = 200, description = "Items returned (QueryResult<BaseItemDto>)")),
    tag = "hermit"
)]
async fn get_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let internal = InternalItemsQuery {
        user: Some(user.clone()),
        start_index: query.start_index,
        limit: query.limit,
        recursive: query.recursive.unwrap_or(false),
        ..InternalItemsQuery::default()
    };
    let result = state.library.query_items(&internal).await?;
    let options = DtoOptions::with_all_fields(false);
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, Some(&user), None, true)
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
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `hermit-model` DTO defect),
    // overflowing the OpenAPI generator when inlined.
    responses(
        (status = 200, description = "Item returned (BaseItemDto)"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
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
    let options = DtoOptions::with_all_fields(false);
    let dto = state
        .dto
        .get_base_item_dto(&item, &options, Some(&user), None)
        .await?;
    Ok(Json(dto))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items", get(get_items))
        .route("/Items/{itemId}", get(get_item))
}
