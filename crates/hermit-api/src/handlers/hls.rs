//! `DynamicHlsController` + `HlsSegmentController` + the transcode branches of
//! `VideosController` / `UniversalAudioController` — the live HLS/transcode routes.
//!
//! These wire the now-real transcode runtime + `hermit-hls` playlist generator to
//! the HTTP surface, entirely through the [`HlsStreamManager`] seam
//! (`hermit-traits`). The heavyweight `StreamState`/arg-building/ffmpeg-spawn
//! machinery lives in the implementation crate (`hermit-mediaencoding`); every
//! handler here only:
//!
//! 1. parses the request's route + query into an [`HlsStreamRequest`], and
//! 2. calls the seam and serves the result — either a playlist string (with the
//!    HLS `application/x-mpegURL` content type) or a [`ServedFile`] streamed from
//!    the transcode cache with its resolved MIME type.
//!
//! Routes ported:
//! - `GET|HEAD /Videos/{itemId}/master.m3u8`, `GET /Videos/{itemId}/main.m3u8`,
//!   `GET /Videos/{itemId}/live.m3u8`
//! - `GET|HEAD /Audio/{itemId}/master.m3u8`, `GET /Audio/{itemId}/main.m3u8`
//! - the dynamic segment routes `hls1/{playlistId}/{segmentId}.{container}`
//!   (video + audio)
//! - the legacy `HlsSegmentController` routes
//!   (`Videos/{id}/hls/{playlist}/{segment}.{ext}`,
//!   `Videos/{id}/hls/{playlist}/stream.m3u8`,
//!   `Audio/{id}/hls/{segment}/stream.{aac,mp3}`)
//! - `DELETE /Videos/ActiveEncodings` (stop an encoding)
//! - `GET /Videos/{videoId}/{mediaSourceId}/Attachments/{index}` (attachment serve)
//!
//! The transcode branch of `/Videos|Audio/{id}/{container}` and
//! `/Audio/{id}/universal` is folded into the existing direct-play handlers
//! (`videos`/`audio`), which fall back to [`HlsStreamManager::transcode_stream`]
//! only when the item has no direct-playable file.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{delete, get};
use hermit_traits::media_encoding::{HlsStreamRequest, ServedFile};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The MIME type for an HLS playlist (`.m3u8`), matching Jellyfin's
/// `MimeTypes.GetMimeType("playlist.m3u8")`.
const HLS_PLAYLIST_CONTENT_TYPE: &str = "application/x-mpegURL";

/// The `?static` / segment-shaping query parameters common to every HLS route.
///
/// Only the fields the software transcode path consumes are captured; the full
/// device-profile/codec/bitrate matrix the C# DTO accepts is resolved from server
/// defaults inside the [`HlsStreamManager`] implementation. Unknown parameters are
/// ignored here but preserved verbatim in the raw query string (see
/// [`build_request`]).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HlsQuery {
    /// The pinned media source id, if any.
    #[serde(default)]
    media_source_id: Option<String>,
    /// The playback-session id this stream belongs to.
    #[serde(default)]
    play_session_id: Option<String>,
    /// The requesting device id (kill/keep-alive scope).
    #[serde(default)]
    device_id: Option<String>,
    /// The desired segment container (`ts`/`mp4`).
    #[serde(default)]
    segment_container: Option<String>,
    /// The desired segment length in seconds.
    #[serde(default)]
    segment_length: Option<i32>,
    /// The desired output audio codec.
    #[serde(default)]
    audio_codec: Option<String>,
    /// The desired output video codec.
    #[serde(default)]
    video_codec: Option<String>,
    /// Whether the client asked for a static (direct) stream.
    #[serde(default, rename = "static")]
    is_static: Option<bool>,
}

/// Builds an [`HlsStreamRequest`] for `item_id` from the parsed query and the raw
/// query string (forwarded verbatim into generated segment URLs).
fn build_request(item_id: Uuid, query: HlsQuery, raw_query: Option<String>) -> HlsStreamRequest {
    // Prefix the raw query with '?' so it slots straight into a playlist URL, as
    // the C# `Request.QueryString` does; an empty query stays empty.
    let query_string = match raw_query {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    };
    HlsStreamRequest {
        item_id,
        media_source_id: query.media_source_id,
        play_session_id: query.play_session_id,
        device_id: query.device_id,
        segment_container: query.segment_container,
        segment_length: query.segment_length,
        audio_codec: query.audio_codec,
        video_codec: query.video_codec,
        is_static: query.is_static.unwrap_or(false),
        query_string,
    }
}

/// Serves the transcode branch of a direct-play stream route.
///
/// The `/Videos|Audio/{id}/{container}` (`stream.{container}`) and
/// `/Audio/{id}/universal` routes first attempt direct play (serving the item's
/// on-disk file); when the item has no direct-playable file this runs the
/// progressive-transcode branch (`VideosController.GetVideoStream` /
/// `UniversalAudioController` transcode path) via
/// [`HlsStreamManager::transcode_stream`]. Shared by `videos` and `audio` so the
/// fallback is not duplicated.
///
/// `is_audio` selects the audio transcode path. The caller passes the already
/// axum-extracted `HlsQuery` (the direct-play handlers extract it) so this needs
/// no extra query parser.
pub(crate) async fn transcode_stream_fallback(
    state: &AppState,
    item_id: Uuid,
    is_audio: bool,
    query: HlsStreamRequest,
    request: Request,
) -> Result<Response, ApiError> {
    let mut req = query;
    req.item_id = item_id;
    let file = state.hls.transcode_stream(&req, is_audio).await?;
    served_file_response(file, request).await
}

/// Builds an [`HlsStreamRequest`] (public to `videos`/`audio` so their direct-play
/// handlers can hand the parsed transcode query to [`transcode_stream_fallback`]).
pub(crate) fn request_from_query(
    item_id: Uuid,
    query: HlsQueryPub,
    raw_query: Option<String>,
) -> HlsStreamRequest {
    build_request(item_id, query.0, raw_query)
}

/// A public wrapper around the crate-private [`HlsQuery`] so `videos`/`audio` can
/// name it in their `Query<…>` extractor without exposing its fields.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct HlsQueryPub(HlsQuery);

/// Serves a playlist string as an HLS `.m3u8` response.
fn playlist_response(playlist: String) -> Response {
    (
        [(header::CONTENT_TYPE, HLS_PLAYLIST_CONTENT_TYPE)],
        playlist,
    )
        .into_response()
}

/// Streams a [`ServedFile`] from disk with its resolved content type.
///
/// Uses the shared [`serve_static_file`](crate::handlers::streaming::serve_static_file)
/// helper (Range/`HEAD`/`404`), then overrides the `Content-Type` with the MIME
/// type the seam resolved for the file (ffmpeg's segment/playlist extensions).
async fn served_file_response(file: ServedFile, request: Request) -> Result<Response, ApiError> {
    let mut response = crate::handlers::streaming::serve_static_file(&file.path, request).await?;
    if let Ok(value) = header::HeaderValue::from_str(&file.content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    Ok(response)
}

use axum::response::IntoResponse;

/// `GET|HEAD /Videos/{itemId}/master.m3u8` — the video master playlist.
async fn get_video_master_playlist(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let req = build_request(item_id, query, raw);
    let playlist = state.hls.master_playlist(&req, false).await?;
    Ok(playlist_response(playlist))
}

/// `GET /Videos/{itemId}/main.m3u8` — the video variant playlist.
async fn get_video_variant_playlist(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let req = build_request(item_id, query, raw);
    let playlist = state.hls.variant_playlist(&req, false).await?;
    Ok(playlist_response(playlist))
}

/// `GET /Videos/{itemId}/live.m3u8` — the video live playlist.
async fn get_video_live_playlist(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let req = build_request(item_id, query, raw);
    let playlist = state.hls.live_playlist(&req).await?;
    Ok(playlist_response(playlist))
}

/// `GET|HEAD /Audio/{itemId}/master.m3u8` — the audio master playlist.
async fn get_audio_master_playlist(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let req = build_request(item_id, query, raw);
    let playlist = state.hls.master_playlist(&req, true).await?;
    Ok(playlist_response(playlist))
}

/// `GET /Audio/{itemId}/main.m3u8` — the audio variant playlist.
async fn get_audio_variant_playlist(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Response, ApiError> {
    let req = build_request(item_id, query, raw);
    let playlist = state.hls.variant_playlist(&req, true).await?;
    Ok(playlist_response(playlist))
}

/// Parses the axum-captured `{segmentId}` segment (the `.container` suffix was
/// dropped by path normalization) into a segment index, `400` on a bad value.
fn parse_segment_index(segment_id: &str) -> Result<i32, ApiError> {
    // The captured value can be `<index>` or `<index>.<ext>` when the client hit
    // the un-normalized form; take the leading integer either way.
    let head = segment_id.split('.').next().unwrap_or(segment_id);
    head.parse::<i32>()
        .map_err(|_| ApiError::BadRequest(format!("invalid segment id {segment_id:?}")))
}

/// `GET /Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}` — a video
/// segment. Starts (or reuses) the transcode and serves the segment file.
async fn get_video_hls_segment(
    State(state): State<AppState>,
    Path((item_id, _playlist_id, segment_id)): Path<(Uuid, String, String)>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let index = parse_segment_index(&segment_id)?;
    let req = build_request(item_id, query, raw);
    let file = state.hls.dynamic_segment(&req, index, false).await?;
    served_file_response(file, request).await
}

/// `GET /Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}` — an audio
/// segment.
async fn get_audio_hls_segment(
    State(state): State<AppState>,
    Path((item_id, _playlist_id, segment_id)): Path<(Uuid, String, String)>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let index = parse_segment_index(&segment_id)?;
    let req = build_request(item_id, query, raw);
    let file = state.hls.dynamic_segment(&req, index, true).await?;
    served_file_response(file, request).await
}

/// The trailing-extension of the request path (e.g. `.ts`, `.mp3`, `.m3u8`).
///
/// The legacy `HlsSegmentController` routes reconstruct the transcode-cache file
/// name as `<captured-id><ext>`, taking `<ext>` from the request path. After path
/// normalization the extension literal is dropped from the route, so it is
/// recovered from the raw URI here.
fn path_extension(uri_path: &str) -> String {
    uri_path
        .rsplit('/')
        .next()
        .and_then(|seg| seg.rfind('.').map(|i| seg[i..].to_owned()))
        .unwrap_or_default()
}

/// `GET /Videos/{itemId}/hls/{playlistId}/{segmentId}.{segmentContainer}` — the
/// legacy video segment serve (`HlsSegmentController.GetHlsVideoSegmentLegacy`).
///
/// Serves the already-produced segment file `<segmentId><ext>` from the transcode
/// cache; `400` on a traversal/miss (mapped from [`ServiceError::InvalidInput`]).
async fn get_hls_video_segment_legacy(
    State(state): State<AppState>,
    Path((_item_id, _playlist_id, segment_id)): Path<(Uuid, String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let ext = path_extension(request.uri().path());
    let file_name = format!(
        "{}{ext}",
        segment_id.split('.').next().unwrap_or(&segment_id)
    );
    let file = state.hls.resolve_transcode_file(&file_name, false).await?;
    served_file_response(file, request).await
}

/// `GET /Videos/{itemId}/hls/{playlistId}/stream.m3u8` — the legacy video
/// playlist serve (`HlsSegmentController.GetHlsPlaylistLegacy`).
///
/// Resolves `<playlistId>.m3u8` inside the transcode cache and serves it as a
/// playlist; a non-playlist match is `400`.
async fn get_hls_playlist_legacy(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((_item_id, playlist_id)): Path<(Uuid, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let file_name = format!(
        "{}.m3u8",
        playlist_id.split('.').next().unwrap_or(&playlist_id)
    );
    let file = state.hls.resolve_transcode_file(&file_name, true).await?;
    served_file_response(file, request).await
}

/// `GET /Audio/{itemId}/hls/{segmentId}/stream.{aac,mp3}` — the legacy audio
/// segment serve (`HlsSegmentController.GetHlsAudioSegmentLegacy`).
async fn get_hls_audio_segment_legacy(
    State(state): State<AppState>,
    Path((_item_id, segment_id)): Path<(Uuid, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let ext = path_extension(request.uri().path());
    let file_name = format!(
        "{}{ext}",
        segment_id.split('.').next().unwrap_or(&segment_id)
    );
    let file = state.hls.resolve_transcode_file(&file_name, false).await?;
    served_file_response(file, request).await
}

/// Query parameters for `DELETE /Videos/ActiveEncodings`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopEncodingQuery {
    /// The requesting client's device id (kill scope).
    device_id: String,
    /// The play session id whose transcode should stop.
    play_session_id: String,
}

/// `DELETE /Videos/ActiveEncodings` — stop an active encoding.
///
/// Port of `HlsSegmentController.StopEncodingProcess`: kills the transcode jobs
/// for `deviceId`/`playSessionId` (deleting their partial files) and returns
/// `204 No Content`.
async fn stop_encoding_process(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<StopEncodingQuery>,
) -> Result<StatusCode, ApiError> {
    let req = HlsStreamRequest {
        device_id: Some(query.device_id),
        play_session_id: Some(query.play_session_id),
        ..HlsStreamRequest::default()
    };
    state.hls.stop_encoding(&req).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /Videos/{videoId}/{mediaSourceId}/Attachments/{index}` — serve an
/// embedded attachment.
///
/// Port of `VideoAttachmentsController.GetAttachment`: resolves the item, extracts
/// attachment `index` from `mediaSourceId` via the [`AttachmentExtractor`], and
/// returns its bytes with the attachment's MIME type (default
/// `application/octet-stream`). A missing item/attachment is `404`.
async fn get_video_attachment(
    State(state): State<AppState>,
    Path((video_id, media_source_id, index)): Path<(Uuid, String, i32)>,
) -> Result<Response, ApiError> {
    if state.library.get_item_by_id(video_id).await?.is_none() {
        return Err(ApiError::NotFound(format!("video {video_id}")));
    }
    let extracted = state
        .attachments
        .get_attachment(video_id, &media_source_id, index)
        .await?;
    let mime = extracted
        .attachment
        .mime_type
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let mut response = (StatusCode::OK, Body::from(extracted.data)).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&mime) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    Ok(response)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Videos/{itemId}/master.m3u8",
            get(get_video_master_playlist).head(get_video_master_playlist),
        )
        .route(
            "/Videos/{itemId}/main.m3u8",
            get(get_video_variant_playlist),
        )
        .route("/Videos/{itemId}/live.m3u8", get(get_video_live_playlist))
        .route(
            "/Audio/{itemId}/master.m3u8",
            get(get_audio_master_playlist).head(get_audio_master_playlist),
        )
        .route("/Audio/{itemId}/main.m3u8", get(get_audio_variant_playlist))
        .route(
            "/Videos/{itemId}/hls1/{playlistId}/{segmentId}",
            get(get_video_hls_segment),
        )
        .route(
            "/Audio/{itemId}/hls1/{playlistId}/{segmentId}",
            get(get_audio_hls_segment),
        )
        .route(
            "/Videos/{itemId}/hls/{playlistId}/{segmentId}",
            get(get_hls_video_segment_legacy),
        )
        .route(
            "/Videos/{itemId}/hls/{playlistId}/stream.m3u8",
            get(get_hls_playlist_legacy),
        )
        .route(
            "/Audio/{itemId}/hls/{segmentId}/stream.aac",
            get(get_hls_audio_segment_legacy),
        )
        .route(
            "/Audio/{itemId}/hls/{segmentId}/stream.mp3",
            get(get_hls_audio_segment_legacy),
        )
        .route("/Videos/ActiveEncodings", delete(stop_encoding_process))
        .route(
            "/Videos/{itemId}/{container}/Attachments/{index}",
            get(get_video_attachment),
        )
}
