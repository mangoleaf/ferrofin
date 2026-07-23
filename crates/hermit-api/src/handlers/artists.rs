//! `ArtistsController` — browse artists / album artists and resolve one by name.
//!
//! Ports:
//!
//! - `GET /Artists` — the library's artists as a [`QueryResult<BaseItemDto>`].
//! - `GET /Artists/AlbumArtists` — the album artists.
//! - `GET /Artists/{name}` — a single artist by name.
//!
//! Jellyfin marks these `[Obsolete("Use GetPersons")]`, but they remain in the
//! contract and TV/mobile clients still call them, so they are ported. The
//! instant-mix / similar / per-name image routes stay on the `501` stub (later
//! batches).

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::data::BaseItemKind;
use hermit_model::dto::BaseItemDto;
use hermit_model::querying::QueryResult;
use hermit_traits::options::DtoOptions;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::by_name::{ByNameItemQuery, ByNameListQuery, project_query_result};
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// `GET /Artists` — the library's artists.
///
/// Port of `ArtistsController.GetArtists`.
#[utoipa::path(
    get,
    path = "/Artists",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Artists returned (QueryResult<BaseItemDto>)")),
    tag = "hermit"
)]
async fn get_artists(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ByNameListQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let internal = query.base_query(Some(user.clone()));
    let result = state.library.get_artists(&internal).await?;
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

/// `GET /Artists/AlbumArtists` — the library's album artists.
///
/// Port of `ArtistsController.GetAlbumArtists`.
#[utoipa::path(
    get,
    path = "/Artists/AlbumArtists",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Album artists returned (QueryResult<BaseItemDto>)")),
    tag = "hermit"
)]
async fn get_album_artists(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ByNameListQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let internal = query.base_query(Some(user.clone()));
    let result = state.library.get_album_artists(&internal).await?;
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

/// `GET /Artists/{name}` — a single artist by name.
///
/// Port of `ArtistsController.GetArtistByName`. Jellyfin lazily creates the
/// artist row when absent; that filesystem side effect is out of scope here, so
/// a missing artist is a `404`.
#[utoipa::path(
    get,
    path = "/Artists/{name}",
    params(("name" = String, Path, description = "The artist name")),
    responses(
        (status = 200, description = "Artist returned (BaseItemDto)"),
        (status = 404, description = "Artist not found")
    ),
    tag = "hermit"
)]
async fn get_artist_by_name(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(name): Path<String>,
    Query(query): Query<ByNameItemQuery>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let item = state
        .library
        .get_named_item(BaseItemKind::MusicArtist, &name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("artist {name}")))?;
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
        .route("/Artists", get(get_artists))
        .route("/Artists/AlbumArtists", get(get_album_artists))
        // Normalized path: the `{name}` position canonicalizes to `{itemId}`
        // (first-seen name at `/Artists/{}` across the vendored table).
        .route("/Artists/{itemId}", get(get_artist_by_name))
}
