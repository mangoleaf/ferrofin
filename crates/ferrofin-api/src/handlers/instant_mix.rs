//! `InstantMixController` — "instant mix" playlists seeded by an item or genre.
//!
//! Ports every InstantMix route:
//!
//! - `GET /Songs/{itemId}/InstantMix`, `/Albums/{itemId}/InstantMix`,
//!   `/Playlists/{itemId}/InstantMix`, `/Artists/{itemId}/InstantMix`,
//!   `/Items/{itemId}/InstantMix` — a mix seeded by the item id.
//! - `GET /MusicGenres/{genreName}/InstantMix` — a mix seeded by a genre name.
//! - `GET /Artists/InstantMix`, `/MusicGenres/InstantMix` — the obsolete
//!   `?id=` query-param variants seeded by an item id.
//!
//! Each resolves the effective user, builds the mix through the
//! [`MusicManager`](ferrofin_traits::library::MusicManager) seam, applies the
//! caller's `limit`, and projects the songs to [`BaseItemDto`]s wrapped in a
//! [`QueryResult`] whose total is the pre-limit count (mirroring C# `GetResult`).

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::BaseItemDto;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::state::AppState;

/// The query parameters common to every InstantMix route.
///
/// The wider Jellyfin query (`fields`, image/user-data toggles) is accepted but
/// only `user_id` and `limit` change the response here; the projection uses the
/// default (all-fields) [`DtoOptions`].
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstantMixQuery {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
}

/// The query parameters for the obsolete `?id=` InstantMix variants.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstantMixByIdQuery {
    /// The seed item id (passed as a query parameter in the obsolete routes).
    #[serde(default)]
    id: Option<Uuid>,
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
}

/// Applies the caller's `limit`, projects the mix to DTOs, and wraps the page in
/// a [`QueryResult`] whose total record count is the pre-limit song count.
///
/// Port of the controller's private `GetResult`.
async fn build_result(
    state: &AppState,
    mut items: Vec<BaseItemEntity>,
    user: Option<&UserEntity>,
    limit: Option<i32>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let total = i32::try_from(items.len()).unwrap_or(i32::MAX);
    if let Some(limit) = limit
        && let Ok(limit) = usize::try_from(limit)
        && limit < items.len()
    {
        items.truncate(limit);
    }
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user, None, true)
        .await?;
    Ok(Json(QueryResult::new(Some(0), Some(total), dtos)))
}

/// Resolves the seed item (a `404` when it does not exist), builds the mix, and
/// returns the projected page. Shared by every by-id InstantMix route.
async fn instant_mix_from_item(
    state: &AppState,
    auth: &ferrofin_traits::options::AuthorizationInfo,
    item_id: Uuid,
    user_id: Option<Uuid>,
    limit: Option<i32>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(state, auth, user_id).await?;
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let user_uuid = user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok());
    let items = state
        .music
        .get_instant_mix_from_item(item_id, user_uuid, &DtoOptions::default())
        .await?;
    build_result(state, items, user.as_ref(), limit).await
}

/// `GET /Songs/{itemId}/InstantMix`.
#[utoipa::path(
    get,
    path = "/Songs/{itemId}/InstantMix",
    params(("itemId" = String, Path, description = "The seed song id")),
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_song(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<InstantMixQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    instant_mix_from_item(&state, &auth, item_id, query.user_id, query.limit).await
}

/// `GET /Albums/{itemId}/InstantMix`.
#[utoipa::path(
    get,
    path = "/Albums/{itemId}/InstantMix",
    params(("itemId" = String, Path, description = "The seed album id")),
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_album(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<InstantMixQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    instant_mix_from_item(&state, &auth, item_id, query.user_id, query.limit).await
}

/// `GET /Playlists/{itemId}/InstantMix`.
#[utoipa::path(
    get,
    path = "/Playlists/{itemId}/InstantMix",
    params(("itemId" = String, Path, description = "The seed playlist id")),
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<InstantMixQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    instant_mix_from_item(&state, &auth, item_id, query.user_id, query.limit).await
}

/// `GET /Artists/{itemId}/InstantMix`.
#[utoipa::path(
    get,
    path = "/Artists/{itemId}/InstantMix",
    params(("itemId" = String, Path, description = "The seed artist id")),
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_artist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<InstantMixQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    instant_mix_from_item(&state, &auth, item_id, query.user_id, query.limit).await
}

/// `GET /Items/{itemId}/InstantMix`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/InstantMix",
    params(("itemId" = String, Path, description = "The seed item id")),
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<InstantMixQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    instant_mix_from_item(&state, &auth, item_id, query.user_id, query.limit).await
}

/// `GET /MusicGenres/{genreName}/InstantMix` — a mix seeded by a genre name.
#[utoipa::path(
    get,
    path = "/MusicGenres/{genreName}/InstantMix",
    params(("genreName" = String, Path, description = "The seed genre name")),
    responses((status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn from_music_genre_name(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(name): Path<String>,
    Query(query): Query<InstantMixQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let user_uuid = user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok());
    let items = state
        .music
        .get_instant_mix_from_genres(&[name], user_uuid, &DtoOptions::default())
        .await?;
    build_result(&state, items, user.as_ref(), query.limit).await
}

/// `GET /Artists/InstantMix` — the obsolete `?id=` artist variant.
#[utoipa::path(
    get,
    path = "/Artists/InstantMix",
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_artist_by_id(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<InstantMixByIdQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let id = query
        .id
        .ok_or_else(|| ApiError::BadRequest("missing id".to_owned()))?;
    instant_mix_from_item(&state, &auth, id, query.user_id, query.limit).await
}

/// `GET /MusicGenres/InstantMix` — the obsolete `?id=` genre variant.
#[utoipa::path(
    get,
    path = "/MusicGenres/InstantMix",
    responses(
        (status = 200, description = "Instant playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn from_music_genre_by_id(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<InstantMixByIdQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let id = query
        .id
        .ok_or_else(|| ApiError::BadRequest("missing id".to_owned()))?;
    instant_mix_from_item(&state, &auth, id, query.user_id, query.limit).await
}

/// Registers this controller's real routes onto `router`.
///
/// The static `/Artists/InstantMix` and `/MusicGenres/InstantMix` are registered
/// before their `{itemId}`/`{genreName}` siblings so axum matches the literal
/// segment first.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Songs/{itemId}/InstantMix", get(from_song))
        .route("/Albums/{itemId}/InstantMix", get(from_album))
        .route("/Playlists/{itemId}/InstantMix", get(from_playlist))
        .route("/Artists/InstantMix", get(from_artist_by_id))
        .route("/Artists/{itemId}/InstantMix", get(from_artist))
        .route("/Items/{itemId}/InstantMix", get(from_item))
        .route("/MusicGenres/InstantMix", get(from_music_genre_by_id))
        .route(
            "/MusicGenres/{genreName}/InstantMix",
            get(from_music_genre_name),
        )
}
