//! `MusicGenresController` — browse music genres and resolve one by name.
//!
//! Ports:
//!
//! - `GET /MusicGenres` — the library's music genres as a
//!   [`QueryResult<BaseItemDto>`].
//! - `GET /MusicGenres/{genreName}` — a single music genre by name.
//!
//! The instant-mix and per-name image routes for music genres live in their own
//! controllers (`instant_mix.rs`, `image.rs`) and are registered there.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::by_name::{
    ByNameItemQuery, ByNameListQuery, project_query_result, resolve_by_name_or_slug,
};
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// `GET /MusicGenres` — the library's music genres.
///
/// Port of `MusicGenresController.GetMusicGenres`.
#[utoipa::path(
    get,
    path = "/MusicGenres",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Music genres returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_music_genres(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ByNameListQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let internal = query.base_query(Some(user.clone()));
    let result = state.library.get_music_genres(&internal).await?;
    // C# `MusicGenresController` builds its options as
    // `new DtoOptions { Fields = fields }.AddAdditionalDtoOptions(enableImages,
    // false, imageTypeLimit, enableImageTypes)` — note the literal `false` for
    // `enableUserData`, so upstream never emits a `UserData` block on these rows.
    let options = query.dto_options(Some(false));
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

/// `GET /MusicGenres/{genreName}` — a single music genre by name.
///
/// Port of `MusicGenresController.GetMusicGenre`. It carries the SAME slug
/// branch as `GenresController.GetGenre` — `CreateItemByName` for a plain
/// name, the non-creating `&` / `/` / `?` lookups for a name containing
/// `BaseItem.SlugChar` — but NOT the same fallback: this controller keeps
/// `if (item is null) return NotFound()` on both trees, where the genres one
/// has `item ??= new Genre()`. The asymmetry is upstream's, and it is the port.
/// (Master additionally marks this action `[Obsolete("Use GetGenre instead")]`;
/// it is still routed and still behaves this way.)
#[utoipa::path(
    get,
    path = "/MusicGenres/{genreName}",
    params(("genreName" = String, Path, description = "The music genre name")),
    responses(
        (status = 200, description = "Music genre returned (BaseItemDto)"),
        (status = 404, description = "Music genre not found")
    ),
    tag = "ferrofin"
)]
async fn get_music_genre(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(genre_name): Path<String>,
    Query(query): Query<ByNameItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    // NOT the genres controller's `item ??= new Genre()`: this one really does
    // `if (item is null) return NotFound()` on BOTH trees (v10.11.8
    // MusicGenresController.cs:164-167, master :165-168), so a slug that
    // resolves nothing is a 404 here and a 200 there. Measured live against a
    // 10.11.8 container: `/MusicGenres/R-B` J=404.
    let item = resolve_by_name_or_slug(&state, BaseItemKind::MusicGenre, &genre_name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("music genre {genre_name}")))?;
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
        .route("/MusicGenres", get(get_music_genres))
        .route("/MusicGenres/{genreName}", get(get_music_genre))
}
