//! `DynamicHlsController` + `HlsSegmentController` + the transcode branches of
//! `VideosController` / `UniversalAudioController` — the live HLS/transcode routes.
//!
//! These wire the now-real transcode runtime + `ferrofin-hls` playlist generator to
//! the HTTP surface, entirely through the [`HlsStreamManager`] seam
//! (`ferrofin-traits`). The heavyweight `StreamState`/arg-building/ffmpeg-spawn
//! machinery lives in the implementation crate (`ferrofin-mediaencoding`); every
//! handler here only:
//!
//! 1. parses the request's route + query into an [`HlsStreamRequest`], and
//! 2. calls the seam and serves the result — either a playlist string (with the
//!    HLS `application/vnd.apple.mpegurl` content type) or a [`ServedFile`] streamed from
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
use ferrofin_traits::media_encoding::{HlsStreamRequest, ServedFile};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::state::AppState;

/// The MIME type for an HLS playlist (`.m3u8`), matching Jellyfin's
/// `MimeTypes.GetMimeType("playlist.m3u8")`.
// Jellyfin's own `MimeTypes` table has `.m3u8` only in its reverse (mime → extension)
// map, so `GetMimeType("playlist.m3u8")` falls through to the `MimeTypes` NuGet
// package's mime-db lookup: `application/vnd.apple.mpegurl`. (The vendored OpenAPI
// still advertises `application/x-mpegURL` — that is the attribute, not the runtime.)
const HLS_PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

/// The `?static` / segment-shaping query parameters common to every HLS route.
///
/// Only the fields the software transcode path consumes are captured; the full
/// device-profile/codec/bitrate matrix the C# DTO accepts is resolved from server
/// defaults inside the [`HlsStreamManager`] implementation. Unknown parameters are
/// ignored here but preserved verbatim in the raw query string (see
/// [`build_request`]).
/// Every field carries a PascalCase `alias`: the PlaybackInfo-negotiated
/// `TranscodingUrl` (built by `StreamInfo::to_url`) uses PascalCase parameters,
/// while the regenerated playlist's segment URLs lowercase the first character
/// — both spellings must parse or the master-playlist request silently drops
/// the negotiated limits.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HlsQuery {
    /// The pinned media source id, if any.
    #[serde(default, alias = "MediaSourceId")]
    media_source_id: Option<String>,
    /// The open live stream to transcode, when the client opened one.
    #[serde(default, alias = "LiveStreamId")]
    live_stream_id: Option<String>,
    /// The playback-session id this stream belongs to.
    #[serde(default, alias = "PlaySessionId")]
    play_session_id: Option<String>,
    /// The requesting device id (kill/keep-alive scope).
    #[serde(default, alias = "DeviceId")]
    device_id: Option<String>,
    /// The desired segment container (`ts`/`mp4`).
    #[serde(default, alias = "SegmentContainer")]
    segment_container: Option<String>,
    /// The desired segment length in seconds.
    #[serde(default, alias = "SegmentLength")]
    segment_length: Option<i32>,
    /// The resume offset in ticks; starts the fMP4 init transcode at the resume
    /// segment so the cached init matches the seek-offset segments (avoids the
    /// resume spinner). Baked into playlist init/segment URLs on a resume.
    #[serde(default, alias = "StartTimeTicks")]
    start_time_ticks: Option<i64>,
    /// The desired output audio codec.
    #[serde(default, alias = "AudioCodec")]
    audio_codec: Option<String>,
    /// The desired output video codec.
    #[serde(default, alias = "VideoCodec")]
    video_codec: Option<String>,
    /// The transcoding profile's max audio channels (drives the `-ac` downmix).
    #[serde(default, alias = "TranscodingMaxAudioChannels")]
    transcoding_max_audio_channels: Option<i32>,
    /// The negotiated video bitrate cap in bit/s (`-maxrate` + downscale).
    ///
    /// The contract (and every Jellyfin client) spells this `videoBitRate`
    /// (capital `R`, `DynamicHlsController`'s `int? videoBitRate`), while the
    /// PlaybackInfo `TranscodingUrl` emits `VideoBitrate`. All four spellings
    /// must parse: dropping the contract form silently lost the negotiated cap
    /// (no `-maxrate`/`-bufsize`, and a source-bitrate BANDWIDTH in the master).
    #[serde(
        default,
        alias = "VideoBitrate",
        alias = "VideoBitRate",
        alias = "videoBitRate"
    )]
    video_bitrate: Option<i32>,
    /// The negotiated audio bitrate cap in bit/s (`audioBitRate` in the
    /// contract; see [`Self::video_bitrate`] for the spellings).
    #[serde(
        default,
        alias = "AudioBitrate",
        alias = "AudioBitRate",
        alias = "audioBitRate"
    )]
    audio_bitrate: Option<i32>,
    /// The maximum output width in pixels (bounds the scale filter).
    #[serde(default, alias = "MaxWidth")]
    max_width: Option<i32>,
    /// The maximum output height in pixels.
    #[serde(default, alias = "MaxHeight")]
    max_height: Option<i32>,
    /// The maximum output framerate.
    #[serde(default, alias = "MaxFramerate")]
    max_framerate: Option<f32>,
    /// Whether `-c:v copy` is permitted (PlaybackInfo appends `false` when the
    /// client forbade it).
    #[serde(default, alias = "AllowVideoStreamCopy")]
    allow_video_stream_copy: Option<bool>,
    /// Whether `-c:a copy` is permitted.
    #[serde(default, alias = "AllowAudioStreamCopy")]
    allow_audio_stream_copy: Option<bool>,
    /// Whether the client asked for a static (direct) stream.
    #[serde(default, rename = "static", alias = "Static")]
    is_static: Option<bool>,
    /// The requested video profile (the `CODECS` profile byte of a re-encode).
    #[serde(default, alias = "Profile")]
    profile: Option<String>,
    /// The requested video level (the `CODECS` level of a re-encode).
    #[serde(default, alias = "Level")]
    level: Option<String>,
    /// The requested output framerate.
    #[serde(default, alias = "Framerate")]
    framerate: Option<f32>,
    /// The requested fixed output width.
    #[serde(default, alias = "Width")]
    width: Option<i32>,
    /// The requested fixed output height.
    #[serde(default, alias = "Height")]
    height: Option<i32>,
    /// The minimum segment count a live playlist waits for before serving.
    #[serde(default, alias = "MinSegments")]
    min_segments: Option<i32>,
    /// The subtitle stream to deliver or burn in.
    #[serde(default, alias = "SubtitleStreamIndex")]
    subtitle_stream_index: Option<i32>,
    /// The negotiated subtitle delivery method name.
    #[serde(default, alias = "SubtitleMethod")]
    subtitle_method: Option<String>,
    /// The client's transcode reasons (forwarded into the master's variant URL).
    #[serde(default, alias = "TranscodeReasons")]
    transcode_reasons: Option<String>,
    /// Whether text subtitles are listed as a group in the master playlist
    /// (route-specific default: `false` for `master.m3u8`, `true` for `live.m3u8`).
    #[serde(default, alias = "EnableSubtitlesInManifest")]
    enable_subtitles_in_manifest: Option<bool>,
    /// Whether the master playlist adds two lower-bitrate variants (default `false`).
    #[serde(default, alias = "EnableAdaptiveBitrateStreaming")]
    enable_adaptive_bitrate_streaming: Option<bool>,
    /// Whether the master playlist lists trickplay image playlists (default `true`).
    #[serde(default, alias = "EnableTrickplay")]
    enable_trickplay: Option<bool>,
}

/// The per-request context the HLS seam needs beyond the query: the session
/// token (embedded as `ApiKey` in master-playlist subtitle/trickplay URIs) and
/// whether the peer is on the local network (disables adaptive variants).
///
/// Port of what `DynamicHlsHelper` reads off `HttpContext` (`User.GetToken()`,
/// `GetNormalizedRemoteIP()` → `INetworkManager.IsInLocalNetwork`).
#[derive(Debug, Default)]
struct HlsRequestContext {
    /// The access token presented by the request, if any.
    api_key: Option<String>,
    /// The peer's IP from the connection; `None` when the server was not started
    /// with connect-info (tests) — treated as not local, the conservative answer.
    remote_ip: Option<std::net::IpAddr>,
    /// The route's default for `enableSubtitlesInManifest` when the query omits
    /// it: `false` on the master routes, `true` on `live.m3u8` (upstream's
    /// per-DTO defaults).
    subtitles_in_manifest_default: bool,
}

impl HlsRequestContext {
    /// Reads the token and peer address off the request parts.
    fn from_parts(
        auth: &ferrofin_traits::options::AuthorizationInfo,
        parts: &axum::http::request::Parts,
        subtitles_in_manifest_default: bool,
    ) -> Self {
        Self {
            api_key: auth.token.as_ref().map(|t| t.expose().to_owned()),
            // Inserted by the server's `with_connect_info`; absent behind a
            // body-consuming extractor or in tests.
            remote_ip: parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|ci| ci.0.ip()),
            subtitles_in_manifest_default,
        }
    }
}

/// Builds an [`HlsStreamRequest`] for `item_id` from the parsed query, the raw
/// query string (forwarded verbatim into generated segment URLs), and the
/// request context (token + peer locality).
fn build_request(
    item_id: Uuid,
    query: HlsQuery,
    raw_query: Option<String>,
    ctx: HlsRequestContext,
) -> HlsStreamRequest {
    // Prefix the raw query with '?' so it slots straight into a playlist URL, as
    // the C# `Request.QueryString` does; an empty query stays empty.
    let query_string = match raw_query {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    };
    // The struct update stays even while every field is named: the request
    // DTO is still growing, and a missed field must take its DTO default
    // rather than break the build.
    #[allow(clippy::needless_update, reason = "room for the growing request DTO")]
    HlsStreamRequest {
        item_id,
        media_source_id: query.media_source_id,
        live_stream_id: query.live_stream_id,
        play_session_id: query.play_session_id,
        device_id: query.device_id,
        segment_container: query.segment_container,
        segment_length: query.segment_length,
        audio_codec: query.audio_codec,
        video_codec: query.video_codec,
        transcoding_max_audio_channels: query.transcoding_max_audio_channels,
        video_bitrate: query.video_bitrate,
        audio_bitrate: query.audio_bitrate,
        max_width: query.max_width,
        max_height: query.max_height,
        max_framerate: query.max_framerate,
        allow_video_stream_copy: query.allow_video_stream_copy.unwrap_or(true),
        allow_audio_stream_copy: query.allow_audio_stream_copy.unwrap_or(true),
        is_static: query.is_static.unwrap_or(false),
        start_time_ticks: query.start_time_ticks,
        profile: query.profile,
        level: query.level,
        framerate: query.framerate,
        width: query.width,
        height: query.height,
        min_segments: query.min_segments,
        subtitle_stream_index: query.subtitle_stream_index,
        subtitle_method: query.subtitle_method,
        transcode_reasons: query.transcode_reasons,
        enable_subtitles_in_manifest: query
            .enable_subtitles_in_manifest
            .unwrap_or(ctx.subtitles_in_manifest_default),
        enable_adaptive_bitrate_streaming: query.enable_adaptive_bitrate_streaming.unwrap_or(false),
        enable_trickplay: query.enable_trickplay.unwrap_or(true),
        api_key: ctx.api_key,
        is_in_local_network: ctx
            .remote_ip
            .is_some_and(crate::handlers::system::is_in_local_network),
        query_string,
        // Fields this route does not read keep their DTO defaults.
        ..HlsStreamRequest::default()
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
    // The progressive-transcode fallback builds no master playlist, so the
    // token/peer context is irrelevant there.
    build_request(item_id, query.0, raw_query, HlsRequestContext::default())
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

/// Serves a master playlist the way `DynamicHlsHelper.GetMasterPlaylistInternal`
/// does: `Expires: 0` on every response, and a `HEAD` answers with an empty
/// body under the playlist MIME type (the state is still resolved, so a missing
/// item is still a `404`).
fn master_playlist_response(method: &axum::http::Method, playlist: String) -> Response {
    let body = if method == axum::http::Method::HEAD {
        String::new()
    } else {
        playlist
    };
    let mut response = playlist_response(body);
    response
        .headers_mut()
        .insert(header::EXPIRES, header::HeaderValue::from_static("0"));
    response
}

/// `GET|HEAD /Videos/{itemId}/master.m3u8` — the video master playlist.
async fn get_video_master_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    parts: axum::http::request::Parts,
) -> Result<Response, ApiError> {
    let ctx = HlsRequestContext::from_parts(&auth, &parts, false);
    let req = build_request(item_id, query, raw, ctx);
    let playlist = state.hls.master_playlist(&req, false).await?;
    Ok(master_playlist_response(&parts.method, playlist))
}

/// `GET /Videos/{itemId}/main.m3u8` — the video variant playlist.
async fn get_video_variant_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    parts: axum::http::request::Parts,
) -> Result<Response, ApiError> {
    let ctx = HlsRequestContext::from_parts(&auth, &parts, false);
    let req = build_request(item_id, query, raw, ctx);
    let playlist = state.hls.variant_playlist(&req, false).await?;
    Ok(playlist_response(playlist))
}

/// `GET /Videos/{itemId}/live.m3u8` — the video live playlist.
async fn get_video_live_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    parts: axum::http::request::Parts,
) -> Result<Response, ApiError> {
    // `GetLiveHlsStream` defaults `EnableSubtitlesInManifest` to true.
    let ctx = HlsRequestContext::from_parts(&auth, &parts, true);
    let req = build_request(item_id, query, raw, ctx);
    let playlist = state.hls.live_playlist(&req).await?;
    Ok(playlist_response(playlist))
}

/// `GET|HEAD /Audio/{itemId}/master.m3u8` — the audio master playlist.
async fn get_audio_master_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    parts: axum::http::request::Parts,
) -> Result<Response, ApiError> {
    let ctx = HlsRequestContext::from_parts(&auth, &parts, false);
    let req = build_request(item_id, query, raw, ctx);
    let playlist = state.hls.master_playlist(&req, true).await?;
    Ok(master_playlist_response(&parts.method, playlist))
}

/// `GET /Audio/{itemId}/main.m3u8` — the audio variant playlist.
async fn get_audio_variant_playlist(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    parts: axum::http::request::Parts,
) -> Result<Response, ApiError> {
    let ctx = HlsRequestContext::from_parts(&auth, &parts, false);
    let req = build_request(item_id, query, raw, ctx);
    let playlist = state.hls.variant_playlist(&req, true).await?;
    Ok(playlist_response(playlist))
}

/// Parses the axum-captured `{segmentId}` segment (the `.container` suffix was
/// dropped by path normalization) into a segment index, `400` on a bad value.
/// Shared with the trickplay tile handler (`{index}.jpg`).
pub(super) fn parse_segment_index(segment_id: &str) -> Result<i32, ApiError> {
    // The captured value can be `<index>` or `<index>.<ext>` when the client hit
    // the un-normalized form; take the leading integer either way.
    let head = segment_id.split('.').next().unwrap_or(segment_id);
    head.parse::<i32>()
        .map_err(|_| ApiError::BadRequest(format!("invalid segment id {segment_id:?}")))
}

/// `GET /Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}` — a video
/// segment. Starts (or reuses) the transcode and serves the segment file.
///
/// Authenticated: C# `DynamicHlsController` carries a class-level `[Authorize]`
/// with no `[AllowAnonymous]`, so the `hls1` segment routes are gated upstream —
/// unlike the legacy `hls` segment routes below, which upstream leaves open with
/// an explicit comment about Chrome omitting the query string. `RequireAuth`
/// accepts the `api_key`/`ApiKey` query parameter, which is how players that
/// cannot set an `Authorization` header on a segment URL authenticate.
async fn get_video_hls_segment(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, _playlist_id, segment_id)): Path<(Uuid, String, String)>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let index = parse_segment_index(&segment_id)?;
    let req = build_request(item_id, query, raw, HlsRequestContext::default());
    let file = state.hls.dynamic_segment(&req, index, false).await?;
    served_file_response(file, request).await
}

/// `GET /Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}` — an audio
/// segment. Authenticated for the same reason as its video sibling above.
async fn get_audio_hls_segment(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, _playlist_id, segment_id)): Path<(Uuid, String, String)>,
    Query(query): Query<HlsQuery>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let index = parse_segment_index(&segment_id)?;
    let req = build_request(item_id, query, raw, HlsRequestContext::default());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The PlaybackInfo-negotiated `TranscodingUrl` uses PascalCase parameters
    /// (`StreamInfo::to_url`); regenerated playlist URLs lowercase the first
    /// character. Both spellings must reach the same request — dropping the
    /// PascalCase form silently loses the negotiated caps (the 2026-07-30
    /// benchmark's full-4K re-encode) and the psid (psid-scoped kills).
    #[test]
    fn hls_query_parses_pascal_and_camel_case() {
        let pascal: HlsQuery = serde_urlencoded::from_str(
            "DeviceId=d1&PlaySessionId=p1&MediaSourceId=m1&VideoCodec=h264&\
             VideoBitrate=8000000&AudioBitrate=192000&MaxWidth=1920&MaxFramerate=30&\
             TranscodingMaxAudioChannels=2&SegmentContainer=mp4&Static=false",
        )
        .expect("pascal query parses");
        assert_eq!(pascal.device_id.as_deref(), Some("d1"));
        assert_eq!(pascal.play_session_id.as_deref(), Some("p1"));
        assert_eq!(pascal.video_bitrate, Some(8_000_000));
        assert_eq!(pascal.audio_bitrate, Some(192_000));
        assert_eq!(pascal.max_width, Some(1920));
        assert_eq!(pascal.max_framerate, Some(30.0));
        assert_eq!(pascal.transcoding_max_audio_channels, Some(2));

        let camel: HlsQuery = serde_urlencoded::from_str(
            "deviceId=d1&playSessionId=p1&videoBitrate=8000000&maxWidth=1920&\
             allowVideoStreamCopy=false",
        )
        .expect("camel query parses");
        assert_eq!(camel.play_session_id.as_deref(), Some("p1"));
        assert_eq!(camel.video_bitrate, Some(8_000_000));
        assert_eq!(camel.allow_video_stream_copy, Some(false));

        // The OpenAPI contract spells the caps `videoBitRate`/`audioBitRate`
        // (capital R) — the form jellyfin-web and the parity harness send. They
        // used to parse as `None`: no `-maxrate`, no `-b:a`, and the master
        // playlist fell back to the source bitrate.
        let contract: HlsQuery =
            serde_urlencoded::from_str("videoBitRate=1000000&audioBitRate=128000")
                .expect("contract query parses");
        assert_eq!(contract.video_bitrate, Some(1_000_000));
        assert_eq!(contract.audio_bitrate, Some(128_000));
        let contract_pascal: HlsQuery =
            serde_urlencoded::from_str("VideoBitRate=1000000&AudioBitRate=128000")
                .expect("pascal contract query parses");
        assert_eq!(contract_pascal.video_bitrate, Some(1_000_000));
        assert_eq!(contract_pascal.audio_bitrate, Some(128_000));
        let lower: HlsQuery = serde_urlencoded::from_str("audioBitrate=96000").expect("parses");
        assert_eq!(lower.audio_bitrate, Some(96_000));
    }

    #[test]
    fn build_request_carries_the_open_live_stream() {
        // A Live TV client opens the channel, then asks for a transcode of the
        // stream it opened. Without this the planner would resolve the channel's
        // static source and dial the tuner a second time.
        let query: HlsQuery =
            serde_urlencoded::from_str("LiveStreamId=prov_service_source&MediaSourceId=source")
                .expect("parses");
        let req = build_request(
            uuid::Uuid::from_u128(7),
            query,
            None,
            HlsRequestContext::default(),
        );
        assert_eq!(req.live_stream_id.as_deref(), Some("prov_service_source"));
        assert_eq!(req.media_source_id.as_deref(), Some("source"));

        // The camelCase spelling parses too. It also looks like a
        // `ParseStreamOptions` key (lower-case initial), so assert it still
        // reaches the field rather than being swallowed as a stream option.
        let query: HlsQuery =
            serde_urlencoded::from_str("liveStreamId=prov_service_source").expect("parses");
        let req = build_request(
            uuid::Uuid::from_u128(7),
            query,
            None,
            HlsRequestContext::default(),
        );
        assert_eq!(req.live_stream_id.as_deref(), Some("prov_service_source"));

        // An ordinary transcode names no live stream.
        let query: HlsQuery = serde_urlencoded::from_str("MediaSourceId=source").expect("parses");
        let req = build_request(
            uuid::Uuid::from_u128(7),
            query,
            None,
            HlsRequestContext::default(),
        );
        assert_eq!(req.live_stream_id, None);
    }

    #[test]
    fn build_request_maps_caps_and_defaults_allow_copy() {
        let query: HlsQuery =
            serde_urlencoded::from_str("MaxWidth=1280&VideoBitrate=4000000").expect("parses");
        let req = build_request(
            uuid::Uuid::from_u128(7),
            query,
            Some("MaxWidth=1280".into()),
            HlsRequestContext::default(),
        );
        assert_eq!(req.max_width, Some(1280));
        assert_eq!(req.video_bitrate, Some(4_000_000));
        assert!(req.allow_video_stream_copy, "copy allowed by default");
        assert!(req.allow_audio_stream_copy, "copy allowed by default");
        assert_eq!(req.query_string, "?MaxWidth=1280");
        // The master-playlist DTO defaults.
        assert!(!req.enable_subtitles_in_manifest);
        assert!(!req.enable_adaptive_bitrate_streaming);
        assert!(req.enable_trickplay);
        assert_eq!(req.api_key, None);
        assert!(!req.is_in_local_network, "unknown peer is not local");

        let query: HlsQuery =
            serde_urlencoded::from_str("AllowVideoStreamCopy=false").expect("parses");
        let req = build_request(
            uuid::Uuid::from_u128(7),
            query,
            None,
            HlsRequestContext::default(),
        );
        assert!(!req.allow_video_stream_copy, "explicit veto honored");
    }

    /// The master-playlist inputs `DynamicHlsHelper` reads off the query and
    /// the HTTP context: profile/level/framerate/size, the subtitle selection,
    /// the manifest flags, the session token and the peer's locality.
    #[test]
    fn build_request_carries_master_playlist_inputs() {
        let query: HlsQuery = serde_urlencoded::from_str(
            "Profile=high&Level=41&Framerate=30&Width=1280&Height=720&MinSegments=2&\
             SubtitleStreamIndex=3&SubtitleMethod=Hls&TranscodeReasons=ContainerNotSupported&\
             EnableSubtitlesInManifest=true&EnableAdaptiveBitrateStreaming=true&\
             enableTrickplay=false",
        )
        .expect("parses");
        let ctx = HlsRequestContext {
            api_key: Some("tok".to_owned()),
            remote_ip: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                192, 168, 1, 5,
            ))),
            subtitles_in_manifest_default: false,
        };
        let req = build_request(uuid::Uuid::from_u128(7), query, None, ctx);
        assert_eq!(req.profile.as_deref(), Some("high"));
        assert_eq!(req.level.as_deref(), Some("41"));
        assert_eq!(req.framerate, Some(30.0));
        assert_eq!((req.width, req.height), (Some(1280), Some(720)));
        assert_eq!(req.min_segments, Some(2));
        assert_eq!(req.subtitle_stream_index, Some(3));
        assert_eq!(req.subtitle_method.as_deref(), Some("Hls"));
        assert_eq!(
            req.transcode_reasons.as_deref(),
            Some("ContainerNotSupported")
        );
        assert!(req.enable_subtitles_in_manifest);
        assert!(req.enable_adaptive_bitrate_streaming);
        assert!(!req.enable_trickplay);
        assert_eq!(req.api_key.as_deref(), Some("tok"));
        assert!(req.is_in_local_network, "RFC1918 peer is local");

        // The route default fills an omitted `EnableSubtitlesInManifest`
        // (`live.m3u8` defaults it to true); a public peer is not local.
        let query: HlsQuery = serde_urlencoded::from_str("").expect("parses");
        let ctx = HlsRequestContext {
            api_key: None,
            remote_ip: Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8))),
            subtitles_in_manifest_default: true,
        };
        let req = build_request(uuid::Uuid::from_u128(7), query, None, ctx);
        assert!(req.enable_subtitles_in_manifest);
        assert!(!req.is_in_local_network);
    }
}
