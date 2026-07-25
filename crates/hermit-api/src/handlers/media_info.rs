//! `MediaInfoController` — playback-info resolution.
//!
//! Ports `GET`/`POST /Items/{itemId}/PlaybackInfo`: resolves the item's playable
//! [`MediaSourceInfo`](hermit_model::dto::MediaSourceInfo)s for the requesting
//! user via the [`MediaSourceManager`](hermit_traits::library::MediaSourceManager)
//! and returns them in a [`PlaybackInfoResponse`]. The `POST` body (a device
//! profile + stream selections) is accepted and ignored for now; both verbs
//! share one handler, matching Jellyfin's two actions.

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hermit_model::media_info::{LiveStreamRequest, LiveStreamResponse, PlaybackInfoResponse};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// Query parameters for the playback-info endpoints.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackInfoQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// Resolves the playback info for `item_id` and the effective user.
///
/// Shared by the `GET` and `POST` handlers. `allow_media_probe` and
/// `enable_path_substitution` mirror the C# defaults for the basic path.
async fn playback_info(
    state: &AppState,
    auth: &hermit_traits::options::AuthorizationInfo,
    item_id: Uuid,
    user_id: Option<Uuid>,
) -> Result<PlaybackInfoResponse, ApiError> {
    let user = resolve_user(state, auth, user_id).await?;
    let resolved_user_id = Uuid::parse_str(&user.id).unwrap_or_else(|_| Uuid::nil());
    let media_sources = state
        .media_sources
        .get_playback_media_sources(item_id, resolved_user_id, true, true)
        .await?;
    Ok(PlaybackInfoResponse {
        media_sources,
        // The client threads this id through every playback-progress report; C#
        // mints a fresh GUID per PlaybackInfo call, so a null here breaks reporting.
        play_session_id: Some(Uuid::new_v4().to_string()),
        error_code: None,
    })
}

/// `GET /Items/{itemId}/PlaybackInfo` — playback info for the item.
///
/// Port of `MediaInfoController.GetPlaybackInfo`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/PlaybackInfo",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Playback info returned", body = PlaybackInfoResponse)),
    tag = "hermit"
)]
async fn get_playback_info(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<PlaybackInfoQuery>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    Ok(Json(
        playback_info(&state, &auth, item_id, query.user_id).await?,
    ))
}

/// `POST /Items/{itemId}/PlaybackInfo` — playback info with a posted profile.
///
/// Port of `MediaInfoController.GetPostedPlaybackInfo`. The posted device
/// profile / stream selections are accepted and ignored for the basic path; the
/// resolved sources are identical to the `GET` form.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/PlaybackInfo",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 200, description = "Playback info returned", body = PlaybackInfoResponse)),
    tag = "hermit"
)]
async fn post_playback_info(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<PlaybackInfoQuery>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    // The posted profile is not yet honoured; drop it explicitly.
    let _ = body;
    Ok(Json(
        playback_info(&state, &auth, item_id, query.user_id).await?,
    ))
}

/// Query parameters for `POST /LiveStreams/Open`.
///
/// Mirrors the flat query form of `MediaInfoController.OpenLiveStream`; the
/// posted `OpenLiveStreamDto` body (device profile + the same fields) is accepted
/// and folded in where the query is absent, matching the C# `?? dto?.Field`
/// precedence (query wins).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenLiveStreamQuery {
    /// The open token identifying the source to open.
    #[serde(default)]
    open_token: Option<String>,
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// The play session id.
    #[serde(default)]
    play_session_id: Option<String>,
    /// The maximum streaming bitrate.
    #[serde(default)]
    max_streaming_bitrate: Option<i32>,
    /// The start time in ticks.
    #[serde(default)]
    start_time_ticks: Option<i64>,
    /// The audio stream index.
    #[serde(default)]
    audio_stream_index: Option<i32>,
    /// The subtitle stream index.
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
    /// The maximum number of audio channels.
    #[serde(default)]
    max_audio_channels: Option<i32>,
    /// The item id whose source is opened.
    #[serde(default)]
    item_id: Option<Uuid>,
}

/// `POST /LiveStreams/Open` — open a media source and return its live stream.
///
/// Port of `MediaInfoController.OpenLiveStream`. The device-profile negotiation
/// carried by the posted body is deferred; the query/body scalar parameters are
/// assembled into a [`LiveStreamRequest`] and handed to
/// [`MediaSourceManager::open_live_stream`](hermit_traits::library::MediaSourceManager::open_live_stream),
/// which probes the source and registers it in the open-stream table.
#[utoipa::path(
    post,
    path = "/LiveStreams/Open",
    responses((status = 200, description = "Media source opened", body = LiveStreamResponse)),
    tag = "hermit"
)]
async fn open_live_stream(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<OpenLiveStreamQuery>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<LiveStreamResponse>, ApiError> {
    // The posted `OpenLiveStreamDto` device profile is not yet honoured.
    let _ = body;
    let user_id = query.user_id.unwrap_or_else(|| auth.user_id());
    let request = LiveStreamRequest {
        open_token: query.open_token,
        user_id,
        play_session_id: query.play_session_id,
        max_streaming_bitrate: query.max_streaming_bitrate,
        start_time_ticks: query.start_time_ticks,
        audio_stream_index: query.audio_stream_index,
        subtitle_stream_index: query.subtitle_stream_index,
        max_audio_channels: query.max_audio_channels,
        item_id: query.item_id.unwrap_or_else(Uuid::nil),
        ..Default::default()
    };
    let media_source = state.media_sources.open_live_stream(&request).await?;
    Ok(Json(LiveStreamResponse::new(media_source)))
}

/// Query parameters for `POST /LiveStreams/Close`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseLiveStreamQuery {
    /// The id of the open live stream to close.
    live_stream_id: String,
}

/// `POST /LiveStreams/Close` — close an open media source.
///
/// Port of `MediaInfoController.CloseLiveStream`. Returns `204 No Content` on
/// success, mirroring the controller's `NoContent()`.
#[utoipa::path(
    post,
    path = "/LiveStreams/Close",
    responses((status = 204, description = "Livestream closed")),
    tag = "hermit"
)]
async fn close_live_stream(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<CloseLiveStreamQuery>,
) -> Result<StatusCode, ApiError> {
    state
        .media_sources
        .close_live_stream(&query.live_stream_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The default `Playback/BitrateTest` payload size, in bytes (C# default 102400).
const DEFAULT_BITRATE_TEST_SIZE: usize = 102_400;

/// The maximum `Playback/BitrateTest` payload size, in bytes (C# `Range` upper
/// bound of 100_000_000).
const MAX_BITRATE_TEST_SIZE: usize = 100_000_000;

/// Query parameters for `GET /Playback/BitrateTest`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BitrateTestQuery {
    /// The number of bytes to return; defaults to
    /// [`DEFAULT_BITRATE_TEST_SIZE`].
    #[serde(default)]
    size: Option<usize>,
}

/// `GET /Playback/BitrateTest` — return a buffer of the requested size.
///
/// Port of `MediaInfoController.GetBitrateTestBytes`: the client measures its
/// download bandwidth against a fixed-size payload. The C# body is random bytes;
/// a zero-filled buffer serves the same measurement purpose and is cheaper. The
/// requested size is clamped to `[1, 100_000_000]` to match the controller's
/// `[Range]`, returning `400` when it falls outside.
#[utoipa::path(
    get,
    path = "/Playback/BitrateTest",
    params(("size" = Option<i32>, Query, description = "The buffer size in bytes")),
    responses((status = 200, description = "Test buffer returned")),
    tag = "hermit"
)]
async fn get_bitrate_test(
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<BitrateTestQuery>,
) -> Result<Response, ApiError> {
    let size = query.size.unwrap_or(DEFAULT_BITRATE_TEST_SIZE);
    if size == 0 || size > MAX_BITRATE_TEST_SIZE {
        return Err(ApiError::BadRequest(format!(
            "size must be between 1 and {MAX_BITRATE_TEST_SIZE}"
        )));
    }
    let body = vec![0_u8; size];
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Items/{itemId}/PlaybackInfo",
            get(get_playback_info).post(post_playback_info),
        )
        .route("/LiveStreams/Open", post(open_live_stream))
        .route("/LiveStreams/Close", post(close_live_stream))
        .route("/Playback/BitrateTest", get(get_bitrate_test))
}
