//! `StudiosController` — browse studios and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Studios` — the library's studios as a [`QueryResult<BaseItemDto>`].
//! - `GET /Studios/{name}` — a single studio by name.
//!
//! The per-name image routes (`/Studios/{name}/Images/{imageType}`) are
//! registered by the image controller and probed by `suite/parity/assets.py`.

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
    let internal = query.base_query(Some(user.clone()))?;
    let result = state.library.get_studios(&internal).await?;
    let options = query.dto_options(query.enable_user_data);
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
/// Port of `StudiosController.GetStudio`, which is
/// `_libraryManager.GetStudio(name)` == `CreateItemByName<Studio>`
/// (`LibraryManager.cs:975-978` on v10.11.8, `:1212-1215` on master —
/// byte-identical). The create happens inside `library.get_named_item`, so the
/// `404` arm below is reachable only if the by-name store fails to mint a row.
/// `StudiosController` has NO slug branch in either tree (only Genres and
/// MusicGenres do), so a hyphenated studio name is looked up literally here.
///
/// `DtoOptions::default()` deliberately follows upstream MASTER (`new
/// DtoOptions()`); v10.11.8 has `new DtoOptions().AddClientFields(User)`, whose
/// only effect is adding `RecursiveItemCount`/`ChildCount` for a handful of
/// legacy clients. Do not "fix" this toward 10.11.8.
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
