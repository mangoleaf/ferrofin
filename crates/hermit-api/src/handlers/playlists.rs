//! `PlaylistsController` — create/read/update playlists, membership, and shares.
//!
//! Ports every route of `Jellyfin.Api.Controllers.PlaylistsController`:
//!
//! - `POST /Playlists` — create a playlist (query or body payload).
//! - `GET /Playlists/{playlistId}` — read a playlist's shares + item ids.
//! - `POST /Playlists/{playlistId}` — update a playlist's name/members/shares.
//! - `GET /Playlists/{playlistId}/Items` — the playlist's member items, paged.
//! - `POST /Playlists/{playlistId}/Items` — add items (optionally at a position).
//! - `DELETE /Playlists/{playlistId}/Items` — remove entries by id.
//! - `POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}` — reorder.
//! - `GET /Playlists/{playlistId}/Users` — the playlist's share list.
//! - `GET /Playlists/{playlistId}/Users/{userId}` — one user's permission.
//! - `POST /Playlists/{playlistId}/Users/{userId}` — set a user's permission.
//! - `DELETE /Playlists/{playlistId}/Users/{userId}` — revoke a user's share.
//!
//! Every route is behind `[Authorize]`, so it takes the [`RequireAuth`]
//! extractor (a missing/invalid token is `401`). The `404 Playlist not found`
//! path is faithful (it flows from
//! [`PlaylistManager::get_playlist_for_user`](hermit_traits::collections::PlaylistManager)).
//!
//! Per-user **shares** are persisted (the `PlaylistShares` table): the
//! `GET/POST/DELETE /Playlists/{id}/Users` routes read and write real permissions,
//! and `GET /Playlists/{id}` reports them. Still deferred: the playlist row has no
//! `OwnerUserId`, so the C# owner/share `403 Forbid` access branches aren't
//! evaluated — an authenticated caller is treated as permitted.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::dto::BaseItemDto;
use hermit_model::dto::PlaylistDto;
use hermit_model::entities_media::PlaylistUserPermissions;
use hermit_model::playlists::{
    CreatePlaylistDto, PlaylistCreationRequest, PlaylistCreationResult, PlaylistUpdateRequest,
    PlaylistUserUpdateRequest, UpdatePlaylistDto, UpdatePlaylistUserDto,
};
use hermit_model::querying::QueryResult;
use hermit_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::query_parse::parse_csv_uuids;
use crate::state::AppState;

/// The query parameters for the obsolete query-string variant of `POST /Playlists`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatePlaylistQuery {
    /// The playlist name (obsolete query form).
    #[serde(default)]
    name: Option<String>,
    /// Comma-delimited item ids (obsolete query form).
    #[serde(default)]
    ids: Option<String>,
    /// The user id (obsolete query form).
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// `POST /Playlists` — creates a new playlist.
///
/// Port of `PlaylistsController.CreatePlaylist`. Query parameters (obsolete) take
/// precedence over the body, matching the C# merge: `ids` fall back to the body,
/// and `name`/`userId` prefer the query when present.
#[utoipa::path(
    post,
    path = "/Playlists",
    request_body = CreatePlaylistDto,
    responses((status = 200, description = "Playlist created (PlaylistCreationResult)")),
    tag = "hermit"
)]
async fn create_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<CreatePlaylistQuery>,
    body: Option<Json<CreatePlaylistDto>>,
) -> Result<Json<PlaylistCreationResult>, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let query_ids = parse_csv_uuids(query.ids.as_deref())?;
    let item_id_list = if query_ids.is_empty() {
        body.ids.clone()
    } else {
        query_ids
    };

    // `userId` (query) ?? body.UserId ?? caller — then default to the caller when
    // still nil (mirrors `RequestHelpers.GetUserId`).
    let mut user_id = query.user_id.or(body.user_id).unwrap_or_else(Uuid::nil);
    if user_id.is_nil() {
        user_id = auth.user_id();
    }

    let request = PlaylistCreationRequest {
        name: query.name.or(body.name),
        item_id_list,
        media_type: body.media_type,
        user_id,
        users: body.users,
        public: body.is_public,
    };
    let result = state.playlists.create_playlist(&request).await?;
    Ok(Json(result))
}

/// `GET /Playlists/{playlistId}` — reads a playlist's shares + item ids.
///
/// Port of `PlaylistsController.GetPlaylist`. Ownership/share fields are deferred
/// (see module docs), so `open_access`/`shares` are reported as their empty
/// defaults and `item_ids` are the playlist's members.
#[utoipa::path(
    get,
    path = "/Playlists/{playlistId}",
    params(("playlistId" = String, Path, description = "The playlist id")),
    responses(
        (status = 200, description = "The playlist (PlaylistDto)"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn get_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(playlist_id): Path<Uuid>,
) -> Result<Json<PlaylistDto>, ApiError> {
    let user_id = auth.user_id();
    state
        .playlists
        .get_playlist_for_user(playlist_id, user_id)
        .await?;
    let items = state
        .playlists
        .get_playlist_items(playlist_id, user_id)
        .await?;
    let item_ids = items
        .iter()
        .filter_map(|i| Uuid::parse_str(&i.id).ok())
        .collect();
    Ok(Json(PlaylistDto {
        open_access: false,
        shares: state.playlists.get_playlist_shares(playlist_id).await?,
        item_ids,
    }))
}

/// `POST /Playlists/{playlistId}` — updates a playlist.
///
/// Port of `PlaylistsController.UpdatePlaylist`. The owner/share `403` branch is
/// deferred (see module docs); a missing playlist is `404`.
#[utoipa::path(
    post,
    path = "/Playlists/{playlistId}",
    request_body = UpdatePlaylistDto,
    params(("playlistId" = String, Path, description = "The playlist id")),
    responses(
        (status = 204, description = "Playlist updated"),
        (status = 403, description = "Access forbidden"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn update_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(playlist_id): Path<Uuid>,
    Json(body): Json<UpdatePlaylistDto>,
) -> Result<StatusCode, ApiError> {
    let calling_user_id = auth.user_id();
    state
        .playlists
        .get_playlist_for_user(playlist_id, calling_user_id)
        .await?;
    let request = PlaylistUpdateRequest {
        id: playlist_id,
        user_id: calling_user_id,
        name: body.name,
        ids: body.ids,
        users: body.users,
        public: body.is_public,
    };
    state.playlists.update_playlist(&request).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The query parameters for `GET /Playlists/{playlistId}/Items`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetPlaylistItemsQuery {
    /// The target user (falls back to the caller).
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The record index to start at.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
}

/// `GET /Playlists/{playlistId}/Items` — the playlist's member items, paged.
///
/// Port of `PlaylistsController.GetPlaylistItems`. Each returned DTO carries its
/// `PlaylistItemId` (the member item id, matching the minimal port's entry-id
/// approximation). The owner/share `403` branch is deferred (see module docs).
#[utoipa::path(
    get,
    path = "/Playlists/{playlistId}/Items",
    params(("playlistId" = String, Path, description = "The playlist id")),
    responses(
        (status = 200, description = "Original playlist returned (QueryResult<BaseItemDto>)"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn get_playlist_items(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(playlist_id): Path<Uuid>,
    Query(query): Query<GetPlaylistItemsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let calling_user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let user = if calling_user_id.is_nil() {
        None
    } else {
        state.users.get_user_by_id(calling_user_id).await?
    };

    let mut items = state
        .playlists
        .get_playlist_items(playlist_id, calling_user_id)
        .await?;
    let total = i32::try_from(items.len()).unwrap_or(i32::MAX);

    if let Some(start) = query.start_index
        && let Ok(start) = usize::try_from(start)
    {
        if start >= items.len() {
            items.clear();
        } else {
            items.drain(0..start);
        }
    }
    if let Some(limit) = query.limit
        && let Ok(limit) = usize::try_from(limit)
        && limit < items.len()
    {
        items.truncate(limit);
    }

    let options = DtoOptions::default();
    let mut dtos = state
        .dto
        .get_base_item_dtos(&items, &options, user.as_ref(), None, true)
        .await?;
    // Tag each DTO with its playlist-entry id (the member item id in the minimal
    // port; C# uses the linked-child entry guid).
    for (dto, item) in dtos.iter_mut().zip(items.iter()) {
        dto.playlist_item_id = Some(item.id.replace('-', ""));
    }

    Ok(Json(QueryResult::new(query.start_index, Some(total), dtos)))
}

/// The query parameters for `POST /Playlists/{playlistId}/Items`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddItemsQuery {
    /// Comma-delimited item ids to add.
    #[serde(default)]
    ids: Option<String>,
    /// The zero-based position to insert at, or the end when absent.
    #[serde(default)]
    position: Option<i32>,
    /// The target user (falls back to the caller).
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// `POST /Playlists/{playlistId}/Items` — adds items to a playlist.
///
/// Port of `PlaylistsController.AddItemToPlaylist`. The owner/share `403` branch
/// is deferred (see module docs); a missing playlist is `404`.
#[utoipa::path(
    post,
    path = "/Playlists/{playlistId}/Items",
    params(("playlistId" = String, Path, description = "The playlist id")),
    responses(
        (status = 204, description = "Items added to playlist"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn add_item_to_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(playlist_id): Path<Uuid>,
    Query(query): Query<AddItemsQuery>,
) -> Result<StatusCode, ApiError> {
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    state
        .playlists
        .get_playlist_for_user(playlist_id, user_id)
        .await?;
    let ids = parse_csv_uuids(query.ids.as_deref())?;
    state
        .playlists
        .add_item_to_playlist(playlist_id, &ids, query.position, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The query parameters for `DELETE /Playlists/{playlistId}/Items`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoveItemsQuery {
    /// Comma-delimited playlist entry ids to remove.
    #[serde(default)]
    entry_ids: Option<String>,
}

/// `DELETE /Playlists/{playlistId}/Items` — removes entries from a playlist.
///
/// Port of `PlaylistsController.RemoveItemFromPlaylist`. The owner/share `403`
/// branch is deferred (see module docs); a missing playlist is `404`.
#[utoipa::path(
    delete,
    path = "/Playlists/{playlistId}/Items",
    params(("playlistId" = String, Path, description = "The playlist id")),
    responses(
        (status = 204, description = "Items removed"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn remove_item_from_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(playlist_id): Path<Uuid>,
    Query(query): Query<RemoveItemsQuery>,
) -> Result<StatusCode, ApiError> {
    let calling_user_id = auth.user_id();
    if !calling_user_id.is_nil() {
        state
            .playlists
            .get_playlist_for_user(playlist_id, calling_user_id)
            .await?;
    }
    let entry_ids: Vec<String> = query
        .entry_ids
        .as_deref()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    state
        .playlists
        .remove_item_from_playlist(&playlist_id.to_string(), &entry_ids)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}` — reorders.
///
/// Port of `PlaylistsController.MoveItem`. Because the normalized axum path
/// captures both the playlist id and the item id under the same positional name
/// (`{itemId}`), the three path segments are read positionally through
/// [`axum::extract::RawPathParams`]. The owner/share `403` branch is deferred
/// (see module docs); a missing playlist is `404`.
#[utoipa::path(
    post,
    path = "/Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}",
    params(
        ("playlistId" = String, Path, description = "The playlist id"),
        ("itemId" = String, Path, description = "The item id"),
        ("newIndex" = i32, Path, description = "The new index")
    ),
    responses(
        (status = 204, description = "Item moved to new index"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn move_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    raw: axum::extract::RawPathParams,
) -> Result<StatusCode, ApiError> {
    let values: Vec<&str> = raw.iter().map(|(_, value)| value).collect();
    let [playlist_id, item_id, new_index] = values.as_slice() else {
        return Err(ApiError::BadRequest("missing path parameters".to_owned()));
    };
    let new_index: i32 = new_index
        .parse()
        .map_err(|_| ApiError::BadRequest(format!("invalid index {new_index:?}")))?;
    let calling_user_id = auth.user_id();
    let playlist_uuid = Uuid::parse_str(playlist_id)
        .map_err(|_| ApiError::BadRequest(format!("invalid id {playlist_id:?}")))?;
    state
        .playlists
        .get_playlist_for_user(playlist_uuid, calling_user_id)
        .await?;
    state
        .playlists
        .move_item(playlist_id, item_id, new_index, calling_user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Playlists/{playlistId}/Users` — the playlist's share list.
///
/// Port of `PlaylistsController.GetPlaylistUsers`. Shares are deferred (see
/// module docs), so an existing playlist yields an empty list; a missing
/// playlist is `404`.
#[utoipa::path(
    get,
    path = "/Playlists/{playlistId}/Users",
    params(("playlistId" = String, Path, description = "The playlist id")),
    responses(
        (status = 200, description = "Found shares (list of PlaylistUserPermissions)"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn get_playlist_users(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(playlist_id): Path<Uuid>,
) -> Result<Json<Vec<PlaylistUserPermissions>>, ApiError> {
    state
        .playlists
        .get_playlist_for_user(playlist_id, auth.user_id())
        .await?;
    Ok(Json(
        state.playlists.get_playlist_shares(playlist_id).await?,
    ))
}

/// `GET /Playlists/{playlistId}/Users/{userId}` — one user's permission.
///
/// Port of `PlaylistsController.GetPlaylistUser`. With shares deferred (see module
/// docs), the caller is reported as owner-equivalent (`can_edit = true`) when it
/// is the requested user, otherwise the permission is absent (`404`).
#[utoipa::path(
    get,
    path = "/Playlists/{playlistId}/Users/{userId}",
    params(
        ("playlistId" = String, Path, description = "The playlist id"),
        ("userId" = String, Path, description = "The user id")
    ),
    responses(
        (status = 200, description = "User permission found (PlaylistUserPermissions)"),
        (status = 404, description = "Playlist or user permissions not found")
    ),
    tag = "hermit"
)]
async fn get_playlist_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((playlist_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PlaylistUserPermissions>, ApiError> {
    let calling_user_id = auth.user_id();
    state
        .playlists
        .get_playlist_for_user(playlist_id, calling_user_id)
        .await?;
    // The caller always has full access to a playlist they can open; any other user
    // is looked up in the playlist's stored shares (`Shares.FirstOrDefault`).
    if user_id == calling_user_id {
        return Ok(Json(PlaylistUserPermissions::new(calling_user_id, true)));
    }
    state
        .playlists
        .get_playlist_shares(playlist_id)
        .await?
        .into_iter()
        .find(|s| s.user_id == user_id)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("User permissions not found".to_owned()))
}

/// `POST /Playlists/{playlistId}/Users/{userId}` — sets a user's permission.
///
/// Port of `PlaylistsController.UpdatePlaylistUser`. The owner-only `403` branch
/// is deferred (see module docs); a missing playlist is `404`. The share upsert
/// itself is a documented no-op in the minimal manager.
#[utoipa::path(
    post,
    path = "/Playlists/{playlistId}/Users/{userId}",
    request_body = UpdatePlaylistUserDto,
    params(
        ("playlistId" = String, Path, description = "The playlist id"),
        ("userId" = String, Path, description = "The user id")
    ),
    responses(
        (status = 204, description = "User's permissions modified"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn update_playlist_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((playlist_id, user_id)): Path<(Uuid, Uuid)>,
    body: Option<Json<UpdatePlaylistUserDto>>,
) -> Result<StatusCode, ApiError> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    state
        .playlists
        .get_playlist_for_user(playlist_id, auth.user_id())
        .await?;
    let request = PlaylistUserUpdateRequest {
        id: playlist_id,
        user_id,
        can_edit: body.can_edit,
    };
    state.playlists.add_user_to_shares(&request).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Playlists/{playlistId}/Users/{userId}` — revokes a user's share.
///
/// Port of `PlaylistsController.RemoveUserFromPlaylist`. The owner/share `403`
/// branch and the share-lookup `404` are deferred (see module docs); a missing
/// playlist is `404`. The revoke itself is a documented no-op in the minimal
/// manager.
#[utoipa::path(
    delete,
    path = "/Playlists/{playlistId}/Users/{userId}",
    params(
        ("playlistId" = String, Path, description = "The playlist id"),
        ("userId" = String, Path, description = "The user id")
    ),
    responses(
        (status = 204, description = "User permissions removed from playlist"),
        (status = 404, description = "Playlist not found")
    ),
    tag = "hermit"
)]
async fn remove_user_from_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((playlist_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let calling_user_id = auth.user_id();
    state
        .playlists
        .get_playlist_for_user(playlist_id, calling_user_id)
        .await?;
    let share = PlaylistUserPermissions::new(user_id, false);
    state
        .playlists
        .remove_user_from_shares(playlist_id, user_id, &share)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers the playlist routes onto `router`.
///
/// The static `/Playlists` is registered alongside the `{playlistId}` siblings;
/// the deeper `.../Items/{itemId}/Move/{newIndex}` route is registered on its
/// normalized (duplicate-`{itemId}`) axum path.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Playlists", post(create_playlist))
        .route(
            "/Playlists/{itemId}",
            get(get_playlist).post(update_playlist),
        )
        .route(
            "/Playlists/{itemId}/Items",
            get(get_playlist_items)
                .post(add_item_to_playlist)
                .delete(remove_item_from_playlist),
        )
        .route(
            "/Playlists/{itemId}/Items/{itemId}/Move/{newIndex}",
            post(move_item),
        )
        .route("/Playlists/{itemId}/Users", get(get_playlist_users))
        .route(
            "/Playlists/{itemId}/Users/{userId}",
            get(get_playlist_user)
                .post(update_playlist_user)
                .delete(remove_user_from_playlist),
        )
}
