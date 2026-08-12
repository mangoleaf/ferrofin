//! `StudiosController` — browse studios and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Studios` — the library's studios as a [`QueryResult<BaseItemDto>`].
//! - `GET /Studios/{name}` — a single studio by name.
//!
//! The per-name image routes are deferred to Batch 9.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::by_name::{ByNameItemQuery, ByNameListQuery, project_query_result};
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// `GET /Studios` — the library's studios.
///
/// Port of `StudiosController.GetStudios`.
#[utoipa::path(
    get,
    path = "/Studios",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Studios returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_studios(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ByNameListQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let internal = query.base_query(Some(user.clone()));
    let result = state.library.get_studios(&internal).await?;
    let options = DtoOptions::with_all_fields(false);
    let projected = project_query_result(
        &state,
        result,
        &options,
        query.should_include_item_types(),
        Some(&user),
    )
    .await?;
    Ok(Json(projected))
}

/// `GET /Studios/{name}` — a single studio by name.
///
/// Port of `StudiosController.GetStudio`. Jellyfin lazily creates the studio row
/// when absent; that filesystem side effect is out of scope for this seam, so a
/// missing studio is a `404`.
#[utoipa::path(
    get,
    path = "/Studios/{name}",
    params(("name" = String, Path, description = "The studio name")),
    responses(
        (status = 200, description = "Studio returned (BaseItemDto)"),
        (status = 404, description = "Studio not found")
    ),
    tag = "ferrofin"
)]
async fn get_studio(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(name): Path<String>,
    Query(query): Query<ByNameItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = state
        .library
        .get_named_item(BaseItemKind::Studio, &name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("studio {name}")))?;
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
        .route("/Studios", get(get_studios))
        .route("/Studios/{name}", get(get_studio))
}
