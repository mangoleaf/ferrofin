//! `LyricsController` — an audio item's lyrics (get/upload/delete + remote).
//!
//! Ports every route of Jellyfin's `LyricsController`:
//! - `GET /Audio/{itemId}/Lyrics` — the item's stored lyrics.
//! - `POST /Audio/{itemId}/Lyrics` — upload an external lyric file.
//! - `DELETE /Audio/{itemId}/Lyrics` — delete the stored lyrics.
//! - `GET /Audio/{itemId}/RemoteSearch/Lyrics` — search remote providers.
//! - `POST /Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}` — download a remote lyric.
//! - `GET /Providers/Lyrics/{lyricId}` — fetch a remote lyric by id.
//!
//! Lyrics are a **deferred subsystem** (`hermit-core`'s [`LyricManager`] is a
//! stub): reads return empty (`404` for a missing lyric), and the mutating /
//! provider routes surface the manager's "not enabled" rejection. The routes are
//! nonetheless real (not `501`) so a client sees the correct not-found / empty
//! semantics rather than a blanket unimplemented. On a successful item resolve,
//! upload / download also queue a metadata refresh, matching the C# flow.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::queue_high_priority_refresh;
use crate::state::AppState;

/// Ensures the audio item exists, returning `404` otherwise (mirrors the C#
/// `GetItemById<Audio>` null check that every route performs first).
async fn require_item(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    if state.library.get_item_by_id(item_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("item {item_id}")));
    }
    Ok(())
}

/// `GET /Audio/{itemId}/Lyrics` — the item's stored lyrics.
///
/// Port of `LyricsController.GetLyrics`: `404` when the item is missing *or* has
/// no lyrics stored (the C# `NotFound` on a `null` result).
#[utoipa::path(
    get,
    path = "/Audio/{itemId}/Lyrics",
    params(("itemId" = String, Path, description = "Item id")),
    responses(
        (status = 200, description = "Lyrics returned", body = LyricDto),
        (status = 404, description = "No lyrics found")
    ),
    tag = "hermit"
)]
async fn get_lyrics(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<LyricDto>, ApiError> {
    require_item(&state, item_id).await?;
    match state.lyrics.get_lyrics(item_id).await? {
        Some(dto) => Ok(Json(dto)),
        None => Err(ApiError::NotFound(format!("lyrics for item {item_id}"))),
    }
}

/// Query parameters for `POST /Audio/{itemId}/Lyrics`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadLyricsQuery {
    /// Name of the file being uploaded (its extension is the lyric format).
    file_name: String,
}

/// `POST /Audio/{itemId}/Lyrics` — upload an external lyric file.
///
/// Port of `LyricsController.UploadLyrics`: the request body is the raw lyric
/// text; the format is the uploaded file's extension. `400` when the body is
/// empty or the file name has no extension; `404` for a missing item. On success
/// queues a high-priority metadata refresh (as the C# does).
#[utoipa::path(
    post,
    path = "/Audio/{itemId}/Lyrics",
    params(
        ("itemId" = String, Path, description = "The item the lyric belongs to"),
        ("fileName" = String, Query, description = "Name of the file being uploaded")
    ),
    responses(
        (status = 200, description = "Lyrics uploaded", body = LyricDto),
        (status = 400, description = "Error processing upload"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn upload_lyrics(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UploadLyricsQuery>,
    body: String,
) -> Result<Json<LyricDto>, ApiError> {
    require_item(&state, item_id).await?;
    if body.is_empty() {
        return Err(ApiError::BadRequest("no lyrics uploaded".to_owned()));
    }
    // The format is the file extension (matches C# `Path.GetExtension`).
    let format = std::path::Path::new(&query.file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if format.is_empty() {
        return Err(ApiError::BadRequest(
            "extension is required on filename".to_owned(),
        ));
    }

    match state.lyrics.save_lyric(item_id, format, &body).await? {
        Some(dto) => {
            queue_high_priority_refresh(&state, item_id).await?;
            Ok(Json(dto))
        }
        None => Err(ApiError::BadRequest("could not save lyrics".to_owned())),
    }
}

/// `DELETE /Audio/{itemId}/Lyrics` — delete the item's stored lyrics.
///
/// Port of `LyricsController.DeleteLyrics`: `204` on success, `404` for a
/// missing item.
#[utoipa::path(
    delete,
    path = "/Audio/{itemId}/Lyrics",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Lyric deleted"),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn delete_lyrics(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id).await?;
    state.lyrics.delete_lyrics(item_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Audio/{itemId}/RemoteSearch/Lyrics` — search remote providers.
///
/// Port of `LyricsController.SearchRemoteLyrics`: `404` for a missing item, else
/// the (possibly empty) provider results.
#[utoipa::path(
    get,
    path = "/Audio/{itemId}/RemoteSearch/Lyrics",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Lyrics retrieved", body = [RemoteLyricInfoDto]),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn search_remote_lyrics(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<Vec<RemoteLyricInfoDto>>, ApiError> {
    require_item(&state, item_id).await?;
    let results = state.lyrics.search_lyrics(item_id).await?;
    Ok(Json(results))
}

/// `POST /Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}` — download a remote lyric.
///
/// Port of `LyricsController.DownloadRemoteLyrics`: `404` for a missing item or
/// when the provider yields nothing; otherwise the downloaded lyric (and a
/// high-priority metadata refresh is queued).
#[utoipa::path(
    post,
    path = "/Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("lyricId" = String, Path, description = "The lyric id")
    ),
    responses(
        (status = 200, description = "Lyric downloaded", body = LyricDto),
        (status = 404, description = "Item not found")
    ),
    tag = "hermit"
)]
async fn download_remote_lyrics(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, lyric_id)): Path<(Uuid, String)>,
) -> Result<Json<LyricDto>, ApiError> {
    require_item(&state, item_id).await?;
    match state.lyrics.download_lyrics(item_id, &lyric_id).await? {
        Some(dto) => {
            queue_high_priority_refresh(&state, item_id).await?;
            Ok(Json(dto))
        }
        None => Err(ApiError::NotFound(format!("remote lyric {lyric_id}"))),
    }
}

/// `GET /Providers/Lyrics/{lyricId}` — fetch a remote lyric by id.
///
/// Port of `LyricsController.GetRemoteLyrics`: `404` when the provider has no
/// such lyric.
#[utoipa::path(
    get,
    path = "/Providers/Lyrics/{lyricId}",
    params(("lyricId" = String, Path, description = "The remote provider item id")),
    responses(
        (status = 200, description = "Lyric returned", body = LyricDto),
        (status = 404, description = "Lyric not found")
    ),
    tag = "hermit"
)]
async fn get_remote_lyrics(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(lyric_id): Path<String>,
) -> Result<Json<LyricDto>, ApiError> {
    // No provider host is wired, so nothing resolves; C# returns `404` on a
    // `null` provider result, which is the same shape a missing lyric yields.
    // A provider rejection (the stub's "not enabled") also maps to not-found so
    // the route reports a stable not-found rather than a `500`.
    match state.lyrics.download_lyrics(Uuid::nil(), &lyric_id).await {
        Ok(Some(dto)) => Ok(Json(dto)),
        Ok(None) | Err(_) => Err(ApiError::NotFound(format!("remote lyric {lyric_id}"))),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Audio/{itemId}/Lyrics",
            get(get_lyrics).post(upload_lyrics).delete(delete_lyrics),
        )
        .route(
            "/Audio/{itemId}/RemoteSearch/Lyrics",
            get(search_remote_lyrics),
        )
        .route(
            "/Audio/{itemId}/RemoteSearch/Lyrics/{lyricId}",
            post(download_remote_lyrics),
        )
        .route("/Providers/Lyrics/{lyricId}", get(get_remote_lyrics))
}
