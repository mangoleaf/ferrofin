//! `GenresController` — browse genres and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Genres` — the library's genres as a [`QueryResult<BaseItemDto>`].
//! - `GET /Genres/{genreName}` — a single genre by name.
//!
//! Jellyfin routes the list to `GetMusicGenres` when the localized parent is a
//! music collection folder; resolving a parent's collection type needs the
//! un-ported `GetParentItem`, so the base (non-music) `get_genres` path is used
//! here and the music-collection branch is noted deferred. The dedicated
//! `MusicGenresController` still serves music genres directly.

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

/// `GET /Genres` — the library's genres.
///
/// Port of `GenresController.GetGenres` (base, non-music-collection path).
#[utoipa::path(
    get,
    path = "/Genres",
    // Body schema omitted: `BaseItemDto` is self-referential and its derived
    // `utoipa::ToSchema` recurses without bound (a `ferrofin-model` DTO defect).
    responses((status = 200, description = "Genres returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_genres(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ByNameListQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let internal = query.base_query(Some(user.clone()));
    let result = state.library.get_genres(&internal).await?;
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

/// `GET /Genres/{genreName}` — a single genre by name.
///
/// Port of `GenresController.GetGenre`. Jellyfin returns an empty `Genre` when
/// none matches; here a missing genre is a `404` so clients get a clear signal
/// rather than a blank body.
#[utoipa::path(
    get,
    path = "/Genres/{genreName}",
    params(("genreName" = String, Path, description = "The genre name")),
    responses(
        (status = 200, description = "Genre returned (BaseItemDto)"),
        (status = 404, description = "Genre not found")
    ),
    tag = "ferrofin"
)]
async fn get_genre(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(genre_name): Path<String>,
    Query(query): Query<ByNameItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = state
        .library
        .get_named_item(BaseItemKind::Genre, &genre_name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("genre {genre_name}")))?;
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
        .route("/Genres", get(get_genres))
        .route("/Genres/{genreName}", get(get_genre))
}
