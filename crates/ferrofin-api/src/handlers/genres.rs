//! `GenresController` — browse genres and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Genres` — the library's genres as a [`QueryResult<BaseItemDto>`].
//! - `GET /Genres/{genreName}` — a single genre by name.
//!
//! Like Jellyfin, the list answers from `GetMusicGenres` when the parent is a
//! music or music-videos collection folder: the scanner gives music items only
//! `MusicGenre` rows, so the `Genre` rows would leave a music library's Genres
//! tab empty. Both run the same query; only the by-name row kind returned
//! differs (`BaseItemRepository.ByName.cs:44-54`).

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::extract::Query;
use crate::handlers::by_name::{
    ByNameItemQuery, ByNameListQuery, project_query_result, resolve_by_name_or_slug,
};
use crate::handlers::items::resolve_user_or_api_key;
use crate::state::AppState;

/// `GET /Genres` — the library's genres.
///
/// Port of `GenresController.GetGenres`, including its switch to
/// `GetMusicGenres` for a music or music-videos library parent.
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
    let user = resolve_user_or_api_key(&state, &auth, query.user_id).await?;
    let parent = query.parent(&state).await?;
    let internal = query.base_query(user.clone())?;
    let result = if is_music_collection_folder(parent.as_ref()) {
        state.library.get_music_genres(&internal).await?
    } else {
        state.library.get_genres(&internal).await?
    };
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
        user.as_ref(),
    )
    .await?;
    Ok(Json(projected))
}

/// `parentItem is ICollectionFolder` with `CollectionType` `music` or
/// `musicvideos` (`GenresController.cs:141-150`).
///
/// A library's collection type is stored in its row's `Data` blob, the same
/// field its DTO's `CollectionType` comes from. Another kind of parent, no
/// stored type, or no parent keeps the plain genres answer.
fn is_music_collection_folder(parent: Option<&BaseItemEntity>) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    if parent.type_.rsplit('.').next() != Some("CollectionFolder") {
        return false;
    }
    let data: serde_json::Value = parent
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    data.get("CollectionType")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| {
            kind.eq_ignore_ascii_case("music") || kind.eq_ignore_ascii_case("musicvideos")
        })
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
    let user = resolve_user_or_api_key(&state, &auth, query.user_id).await?;
    // `item ??= new Genre()`: upstream serializes a default-constructed
    // entity rather than 404ing, so this route has no not-found arm at all.
    let item = match resolve_by_name_or_slug(&state, BaseItemKind::Genre, &genre_name).await? {
        Some(item) => item,
        None => state.library.empty_by_name_item(BaseItemKind::Genre),
    };
    let options = DtoOptions::default();
    let dto = state
        .dto
        .get_base_item_dto(&item, &options, user.as_ref(), None)
        .await?;
    Ok(Json(dto))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Genres", get(get_genres))
        .route("/Genres/{genreName}", get(get_genre))
}
