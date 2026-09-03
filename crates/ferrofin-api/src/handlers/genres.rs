//! `GenresController` — browse genres and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Genres` — the library's genres as a [`QueryResult<BaseItemDto>`].
//! - `GET /Genres/{genreName}` — a single genre by name.
//!
//! Jellyfin routes the list to `GetMusicGenres` when the localized parent is a
//! music collection folder; the base (non-music) `get_genres` path is used here
//! and the dedicated `MusicGenresController` serves music genres directly.
//! Since both now run the SAME query (only the by-name row kind returned
//! differs — `BaseItemRepository.ByName.cs:44-52`), the parent-collection
//! branch changes which of the two row kinds a music library's Genres tab
//! lists; it is an open work item on `GET /Genres`, tracked in
//! `suite/parity/classifications.json`, not something this file silently drops.

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
    let internal = query.base_query(Some(user.clone()))?;
    let result = state.library.get_genres(&internal).await?;
    // C# `GenresController` builds its options as
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

/// `GET /Genres/{genreName}` — a single genre by name.
///
/// Port of `GenresController.GetGenre` (v10.11.8 and master carry the same
/// body; master's only delta on the file is dropping `.AddClientFields(User)`,
/// which Ferrofin follows).
///
/// Three behaviours, not one: a plain name materializes through
/// `CreateItemByName`, a name containing `BaseItem.SlugChar` is looked up
/// through the `&` / `/` / `?` substitutions WITHOUT creating anything, and a
/// miss returns the default-constructed `Genre` — never a 404. See
/// [`resolve_by_name_or_slug`].
#[utoipa::path(
    get,
    path = "/Genres/{genreName}",
    params(("genreName" = String, Path, description = "The genre name")),
    // No 404 arm: upstream's `item ??= new Genre()` makes this route
    // unconditionally a 200.
    responses((status = 200, description = "Genre returned (BaseItemDto)")),
    tag = "ferrofin"
)]
async fn get_genre(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(genre_name): Path<String>,
    Query(query): Query<ByNameItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    // `item ??= new Genre()`: upstream serializes a default-constructed
    // entity rather than 404ing, so this route has no not-found arm at all.
    let item = match resolve_by_name_or_slug(&state, BaseItemKind::Genre, &genre_name).await? {
        Some(item) => item,
        None => state.library.empty_by_name_item(BaseItemKind::Genre),
    };
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
