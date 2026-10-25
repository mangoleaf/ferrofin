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
//! Five of the six carry `[Authorize(Policy = Policies.LyricManagement)]`
//! upstream — only `GET /Audio/{itemId}/Lyrics` is a plain `[Authorize]` — so
//! they take [`RequireLyricManagement`] rather than [`RequireAuth`]. The
//! permission behind it (`EnableLyricManagement`) defaults to false; only an
//! administrator or an API key passes without it, so this is what stops any
//! other account overwriting or deleting another user's lyrics.
//!
//! Every route is backed by the [`LyricManager`] (the item's `Lyric` streams,
//! which the audio probe records from its `.lrc`/`.txt` sidecars, plus the
//! registered remote providers such as LrcLib). After a write the track's
//! metadata refresh re-probes its lyric streams, as the C# flow does: upload
//! and download queue it at high priority, delete awaits it.

use axum::extract::{Path, State};
use axum::handler::Handler;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use uuid::Uuid;

use crate::auth::{RequireAuth, RequireLyricManagement};
use crate::error::ApiError;
use crate::extract::{Query, upload_body_limit};
use crate::handlers::{queue_high_priority_refresh, run_item_refresh};
use crate::state::AppState;

/// Ensures the item exists **and is an audio item**, returning `404` otherwise.
///
/// Mirrors the C# `GetItemById<Audio>(itemId, User.GetUserId())` null check that
/// every route in `LyricsController` performs first: a non-audio id is a `404`
/// on all six routes, not an accepted target for a lyric write. `Audio` covers
/// its `AudioBook` subclass, so both kinds pass.
async fn require_item(
    state: &AppState,
    item_id: Uuid,
    auth: &ferrofin_traits::options::AuthorizationInfo,
) -> Result<(), ApiError> {
    let is_audio = state
        .library
        .get_item_by_id_for_user(item_id, auth.user.as_ref())
        .await?
        .is_some_and(|item| {
            let short = item.type_.rsplit('.').next().unwrap_or(&item.type_);
            matches!(short, "Audio" | "AudioBook")
        });
    if !is_audio {
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
    tag = "ferrofin"
)]
async fn get_lyrics(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<Json<LyricDto>, ApiError> {
    require_item(&state, item_id, &auth).await?;
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
/// Port of `LyricsController.UploadLyrics`: the request body is the lyric
/// file's bytes, in any encoding the manager's reader takes (a UTF-16 file
/// is no `400`); the format is the uploaded file's extension. `400` when the
/// body is empty or the file name has no extension; `404` for a missing item.
/// On success queues a high-priority metadata refresh (as the C# does).
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
    tag = "ferrofin"
)]
async fn upload_lyrics(
    State(state): State<AppState>,
    RequireLyricManagement(auth): RequireLyricManagement,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UploadLyricsQuery>,
    body: axum::body::Bytes,
) -> Result<Json<LyricDto>, ApiError> {
    require_item(&state, item_id, &auth).await?;
    if body.is_empty() {
        return Err(ApiError::BadRequest("no lyrics uploaded".to_owned()));
    }
    let format = upload_format(&query.file_name);
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

/// The lyric format an uploaded file name gives:
/// `Path.GetExtension(fileName).RightPart('.')`, the text after the last dot
/// of the name's last `/`-separated segment (.NET's only separator on Linux,
/// where `\` is an ordinary character). Unlike [`std::path::Path::extension`], a
/// leading dot counts (`.lrc` is `lrc`); a trailing one gives nothing.
fn upload_format(file_name: &str) -> &str {
    let name = file_name.rsplit('/').next().unwrap_or(file_name);
    name.rfind('.').map_or("", |dot| &name[dot + 1..])
}

/// `DELETE /Audio/{itemId}/Lyrics` — delete the item's stored lyrics.
///
/// Port of `LyricsController.DeleteLyrics`: `204` on success, `404` for a
/// missing item. `LyricManager.DeleteLyricsAsync` ends with an awaited
/// `audio.RefreshMetadata`, which drops the deleted files' streams, so the
/// refresh runs before the `204` and `HasLyrics` is false by then. A refresh
/// that fails, or that the shutdown stops, is an error status even though
/// the files are already deleted, as upstream's awaited refresh throws.
#[utoipa::path(
    delete,
    path = "/Audio/{itemId}/Lyrics",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 204, description = "Lyric deleted"),
        (status = 404, description = "Item not found")
    ),
    tag = "ferrofin"
)]
async fn delete_lyrics(
    State(state): State<AppState>,
    RequireLyricManagement(auth): RequireLyricManagement,
    Path(item_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_item(&state, item_id, &auth).await?;
    state.lyrics.delete_lyrics(item_id).await?;
    run_item_refresh(&state, item_id).await?;
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
    tag = "ferrofin"
)]
async fn search_remote_lyrics(
    State(state): State<AppState>,
    RequireLyricManagement(auth): RequireLyricManagement,
    Path(item_id): Path<Uuid>,
) -> Result<Json<Vec<RemoteLyricInfoDto>>, ApiError> {
    require_item(&state, item_id, &auth).await?;
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
    tag = "ferrofin"
)]
async fn download_remote_lyrics(
    State(state): State<AppState>,
    RequireLyricManagement(auth): RequireLyricManagement,
    Path((item_id, lyric_id)): Path<(Uuid, String)>,
) -> Result<Json<LyricDto>, ApiError> {
    require_item(&state, item_id, &auth).await?;
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
    tag = "ferrofin"
)]
async fn get_remote_lyrics(
    State(state): State<AppState>,
    RequireLyricManagement(_auth): RequireLyricManagement,
    Path(lyric_id): Path<String>,
) -> Result<Json<LyricDto>, ApiError> {
    // `LyricManager.GetRemoteLyricsAsync`: route the namespaced id to its
    // provider, fetch + parse, no item and no sidecar write. `null` → 404.
    match state.lyrics.get_remote_lyrics(&lyric_id).await? {
        Some(dto) => Ok(Json(dto)),
        None => Err(ApiError::NotFound(format!("remote lyric {lyric_id}"))),
    }
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Audio/{itemId}/Lyrics",
            get(get_lyrics)
                .post(upload_lyrics.layer(upload_body_limit()))
                .delete(delete_lyrics),
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
crate::query::query_parameters! {
    UploadLyricsQuery {} => [("post", "/Audio/{itemId}/Lyrics")];
}
