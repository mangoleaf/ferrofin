//! `PlaystateController` — mark played/unplayed + session playback reporting.
//!
//! Ports Jellyfin's `PlaystateController` (tagged `Session` / `UserData`):
//!
//! - `POST`/`DELETE /UserPlayedItems/{itemId}` — mark an item played / unplayed
//!   for the caller (and every additional user on the session), returning the
//!   refreshed [`UserItemDataDto`].
//! - `POST /Sessions/Playing` — report playback start.
//! - `POST /Sessions/Playing/Progress` — report playback progress.
//! - `POST /Sessions/Playing/Ping` — ping a playback session.
//! - `POST /Sessions/Playing/Stopped` — report playback stop.
//! - `POST`/`DELETE /PlayingItems/{itemId}` (+ `/Progress`) — the obsolete
//!   query-param forms of start/progress/stop, kept for older clients.
//!
//! Faithfulness notes:
//! - `ValidatePlayMethod` downgrades a `Transcode` method to `DirectPlay` when no
//!   transcoding job backs the play-session id. No transcode manager is ported at
//!   this layer, so there is never a job → `Transcode` always becomes
//!   `DirectPlay`, exactly as C# would for a non-transcoded session.
//! - `PingPlaybackSession` / the transcode-kill on stop poke the transcode
//!   manager, which is deferred; the reporting call to [`SessionManager`] still
//!   runs, so the session/play-state bookkeeping is faithful.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use hermit_model::dto::UserItemDataDto;
use hermit_model::session::{
    PlayMethod, PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, RepeatMode,
};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::handlers::session_ctx::{current_session, current_session_id};
use crate::state::AppState;

/// Query parameters for the mark-played route (`datePlayed` + optional `userId`).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarkPlayedQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Optional date the item was played (ISO-8601); defaults to now.
    #[serde(default)]
    date_played: Option<chrono::DateTime<chrono::Utc>>,
}

/// Query parameters for the unmark-played route (optional `userId`).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserIdQuery {
    /// The target user; defaults to the authenticated caller when absent.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// `POST /UserPlayedItems/{itemId}` — marks an item played for the user.
///
/// Port of `PlaystateController.MarkPlayedItem`: resolve the user + item, mark
/// it played (via [`UserDataManager::mark_played`](hermit_traits::library::UserDataManager::mark_played)),
/// then apply the same mark to every additional user on the caller's session.
#[utoipa::path(
    post,
    path = "/UserPlayedItems/{itemId}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("datePlayed" = Option<String>, Query, description = "Optional date the item was played")
    ),
    responses(
        (status = 200, description = "Item marked as played (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "hermit"
)]
async fn mark_played_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<MarkPlayedQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = parse_id(&user.id);
    assert_item_exists(&state, item_id).await?;

    let dto = state
        .user_data
        .mark_played(user_id, item_id, query.date_played)
        .await?;

    for guest in additional_user_ids(&state, &auth).await? {
        state
            .user_data
            .mark_played(guest, item_id, query.date_played)
            .await?;
    }

    Ok(Json(dto))
}

/// `DELETE /UserPlayedItems/{itemId}` — marks an item unplayed for the user.
///
/// Port of `PlaystateController.MarkUnplayedItem`.
#[utoipa::path(
    delete,
    path = "/UserPlayedItems/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Item marked as unplayed (UserItemDataDto)", body = UserItemDataDto),
        (status = 404, description = "Item or user not found")
    ),
    tag = "hermit"
)]
async fn mark_unplayed_item(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<UserIdQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let user = resolve_user(&state, &auth, query.user_id).await?;
    let user_id = parse_id(&user.id);
    assert_item_exists(&state, item_id).await?;

    let dto = state.user_data.mark_unplayed(user_id, item_id).await?;

    for guest in additional_user_ids(&state, &auth).await? {
        state.user_data.mark_unplayed(guest, item_id).await?;
    }

    Ok(Json(dto))
}

/// `POST /Users/{userId}/PlayedItems/{itemId}` — the legacy per-user played
/// route jellyfin-web still calls (the contract form is `/UserPlayedItems/...`).
async fn mark_played_for_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<MarkPlayedQuery>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let user = resolve_user(&state, &auth, Some(user_id)).await?;
    let uid = parse_id(&user.id);
    assert_item_exists(&state, item_id).await?;
    let dto = state
        .user_data
        .mark_played(uid, item_id, query.date_played)
        .await?;
    for guest in additional_user_ids(&state, &auth).await? {
        state
            .user_data
            .mark_played(guest, item_id, query.date_played)
            .await?;
    }
    Ok(Json(dto))
}

/// `DELETE /Users/{userId}/PlayedItems/{itemId}` — legacy per-user mark-unplayed.
async fn unmark_played_for_user(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<UserItemDataDto>, ApiError> {
    let user = resolve_user(&state, &auth, Some(user_id)).await?;
    let uid = parse_id(&user.id);
    assert_item_exists(&state, item_id).await?;
    let dto = state.user_data.mark_unplayed(uid, item_id).await?;
    for guest in additional_user_ids(&state, &auth).await? {
        state.user_data.mark_unplayed(guest, item_id).await?;
    }
    Ok(Json(dto))
}

/// `POST /Sessions/Playing` — reports playback has started.
///
/// Port of `PlaystateController.ReportPlaybackStart`.
#[utoipa::path(
    post,
    path = "/Sessions/Playing",
    request_body = PlaybackStartInfo,
    responses((status = 204, description = "Playback start recorded")),
    tag = "hermit"
)]
async fn report_playback_start(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(mut info): Json<PlaybackStartInfo>,
) -> Result<StatusCode, ApiError> {
    info.play_method = validate_play_method(info.play_method);
    info.session_id = Some(current_session_id(&state, &auth).await?);
    state.sessions.on_playback_start(&info).await?;
    log_playback_activity(&state, &auth, info.item_id, "is playing", "VideoPlayback").await;
    Ok(StatusCode::NO_CONTENT)
}

/// Records a playback activity-log entry (best-effort — a logging failure must
/// not fail the playback report). Port of `ActivityLogEntryPoint.OnPlayback*`,
/// which writes "{user} is playing {item} on {device}" so the dashboard's
/// Activity feed reflects what's being watched.
async fn log_playback_activity(
    state: &AppState,
    auth: &hermit_traits::options::AuthorizationInfo,
    item_id: Uuid,
    action: &str,
    type_: &str,
) {
    // No user → no entry, matching Jellyfin (it skips user-less playback events).
    let Some(user) = auth.user.as_ref() else {
        return;
    };
    let user_id = Uuid::parse_str(&user.id).ok();
    let device = auth
        .device
        .clone()
        .or_else(|| auth.client.clone())
        .unwrap_or_default();
    let item_name = state
        .library
        .get_item_by_id(item_id)
        .await
        .ok()
        .flatten()
        .and_then(|i| i.name)
        .unwrap_or_default();
    let name = format!("{} {action} {item_name} on {device}", user.username);
    let _ = state
        .activity
        .create_entry(hermit_traits::activity::ActivityLogCreate {
            name,
            type_: type_.to_owned(),
            user_id,
            item_id: Some(item_id),
            severity: hermit_model::activity::LogLevel::Information,
            ..Default::default()
        })
        .await;
}

/// `POST /Sessions/Playing/Progress` — reports playback progress.
///
/// Port of `PlaystateController.ReportPlaybackProgress`.
#[utoipa::path(
    post,
    path = "/Sessions/Playing/Progress",
    request_body = PlaybackProgressInfo,
    responses((status = 204, description = "Playback progress recorded")),
    tag = "hermit"
)]
async fn report_playback_progress(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(mut info): Json<PlaybackProgressInfo>,
) -> Result<StatusCode, ApiError> {
    info.play_method = validate_play_method(info.play_method);
    info.session_id = Some(current_session_id(&state, &auth).await?);
    // The controller reports client-driven progress, so `is_automated` is false.
    state.sessions.on_playback_progress(&info, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for `POST /Sessions/Playing/Ping`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PingQuery {
    /// The playback session id to ping.
    play_session_id: String,
}

/// `POST /Sessions/Playing/Ping` — pings a playback session.
///
/// Port of `PlaystateController.PingPlaybackSession`. The ping targets the
/// transcode manager (deferred), so this validates the parameter and returns
/// `204` — the transcode keep-alive is a no-op without a transcoding job.
#[utoipa::path(
    post,
    path = "/Sessions/Playing/Ping",
    params(("playSessionId" = String, Query, description = "Playback session id")),
    responses((status = 204, description = "Playback session pinged")),
    tag = "hermit"
)]
async fn ping_playback_session(
    State(_state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Query(query): Query<PingQuery>,
) -> Result<StatusCode, ApiError> {
    // C# `_transcodeManager.PingTranscodingJob(playSessionId, null)`; the
    // transcode manager is deferred, so keeping the job alive is a no-op. The
    // required `playSessionId` is still validated by the extractor.
    tracing::debug!(play_session_id = %query.play_session_id, "ping playback session");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Sessions/Playing/Stopped` — reports playback has stopped.
///
/// Port of `PlaystateController.ReportPlaybackStopped`.
#[utoipa::path(
    post,
    path = "/Sessions/Playing/Stopped",
    request_body = PlaybackStopInfo,
    responses((status = 204, description = "Playback stop recorded")),
    tag = "hermit"
)]
async fn report_playback_stopped(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Json(mut info): Json<PlaybackStopInfo>,
) -> Result<StatusCode, ApiError> {
    // The transcode-job kill (C# `KillTranscodingJobs`) is deferred; the play-
    // state bookkeeping below is the portable slice.
    info.session_id = Some(current_session_id(&state, &auth).await?);
    let item_id = info.item_id;
    state.sessions.on_playback_stopped(&info).await?;
    log_playback_activity(
        &state,
        &auth,
        item_id,
        "has finished playing",
        "VideoPlaybackStopped",
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for the obsolete `POST /PlayingItems/{itemId}` start form.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStartQuery {
    #[serde(default)]
    media_source_id: Option<String>,
    #[serde(default)]
    audio_stream_index: Option<i32>,
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
    #[serde(default)]
    play_method: Option<PlayMethod>,
    #[serde(default)]
    live_stream_id: Option<String>,
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    can_seek: Option<bool>,
}

/// `POST /PlayingItems/{itemId}` — obsolete playback-start form.
///
/// Port of `PlaystateController.OnPlaybackStart`: build a [`PlaybackStartInfo`]
/// from the query params and forward to the same reporting path.
#[utoipa::path(
    post,
    path = "/PlayingItems/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 204, description = "Play start recorded")),
    tag = "hermit"
)]
async fn on_playback_start(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<LegacyStartQuery>,
) -> Result<StatusCode, ApiError> {
    let mut info = PlaybackStartInfo {
        can_seek: query.can_seek.unwrap_or(false),
        item_id,
        media_source_id: query.media_source_id,
        audio_stream_index: query.audio_stream_index,
        subtitle_stream_index: query.subtitle_stream_index,
        play_method: query.play_method.unwrap_or(PlayMethod::Transcode),
        play_session_id: query.play_session_id,
        live_stream_id: query.live_stream_id,
        ..PlaybackStartInfo::default()
    };
    info.play_method = validate_play_method(info.play_method);
    info.session_id = Some(current_session_id(&state, &auth).await?);
    state.sessions.on_playback_start(&info).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for the obsolete `POST /PlayingItems/{itemId}/Progress` form.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyProgressQuery {
    #[serde(default)]
    media_source_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<i64>,
    #[serde(default)]
    audio_stream_index: Option<i32>,
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
    #[serde(default)]
    volume_level: Option<i32>,
    #[serde(default)]
    play_method: Option<PlayMethod>,
    #[serde(default)]
    live_stream_id: Option<String>,
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    repeat_mode: Option<RepeatMode>,
    #[serde(default)]
    is_paused: Option<bool>,
    #[serde(default)]
    is_muted: Option<bool>,
}

/// `POST /PlayingItems/{itemId}/Progress` — obsolete progress form.
///
/// Port of `PlaystateController.OnPlaybackProgress`.
#[utoipa::path(
    post,
    path = "/PlayingItems/{itemId}/Progress",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 204, description = "Play progress recorded")),
    tag = "hermit"
)]
async fn on_playback_progress(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<LegacyProgressQuery>,
) -> Result<StatusCode, ApiError> {
    let mut info = PlaybackProgressInfo {
        item_id,
        position_ticks: query.position_ticks,
        is_muted: query.is_muted.unwrap_or(false),
        is_paused: query.is_paused.unwrap_or(false),
        media_source_id: query.media_source_id,
        audio_stream_index: query.audio_stream_index,
        subtitle_stream_index: query.subtitle_stream_index,
        volume_level: query.volume_level,
        play_method: query.play_method.unwrap_or(PlayMethod::Transcode),
        play_session_id: query.play_session_id,
        live_stream_id: query.live_stream_id,
        repeat_mode: query.repeat_mode.unwrap_or(RepeatMode::RepeatNone),
        ..PlaybackProgressInfo::default()
    };
    info.play_method = validate_play_method(info.play_method);
    info.session_id = Some(current_session_id(&state, &auth).await?);
    state.sessions.on_playback_progress(&info, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for the obsolete `DELETE /PlayingItems/{itemId}` stop form.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStopQuery {
    #[serde(default)]
    media_source_id: Option<String>,
    #[serde(default)]
    next_media_type: Option<String>,
    #[serde(default)]
    position_ticks: Option<i64>,
    #[serde(default)]
    live_stream_id: Option<String>,
    #[serde(default)]
    play_session_id: Option<String>,
}

/// `DELETE /PlayingItems/{itemId}` — obsolete playback-stop form.
///
/// Port of `PlaystateController.OnPlaybackStopped`.
#[utoipa::path(
    delete,
    path = "/PlayingItems/{itemId}",
    params(("itemId" = String, Path, description = "The item id")),
    responses((status = 204, description = "Playback stop recorded")),
    tag = "hermit"
)]
async fn on_playback_stopped(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Path(item_id): Path<Uuid>,
    Query(query): Query<LegacyStopQuery>,
) -> Result<StatusCode, ApiError> {
    let mut info = PlaybackStopInfo {
        item_id,
        position_ticks: query.position_ticks,
        media_source_id: query.media_source_id,
        play_session_id: query.play_session_id,
        live_stream_id: query.live_stream_id,
        next_media_type: query.next_media_type,
        ..PlaybackStopInfo::default()
    };
    info.session_id = Some(current_session_id(&state, &auth).await?);
    state.sessions.on_playback_stopped(&info).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Parses a stored user-entity id string to a [`Uuid`], falling back to nil.
fn parse_id(raw: &str) -> Uuid {
    Uuid::parse_str(raw).unwrap_or_else(|_| Uuid::nil())
}

/// C# `ValidatePlayMethod`: without a transcoding job (no transcode manager is
/// ported here), a `Transcode` method downgrades to `DirectPlay`; other methods
/// pass through unchanged.
fn validate_play_method(method: PlayMethod) -> PlayMethod {
    if method == PlayMethod::Transcode {
        PlayMethod::DirectPlay
    } else {
        method
    }
}

/// Asserts the item exists (C# `GetItemById` null-check → `404`).
async fn assert_item_exists(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    Ok(())
}

/// The ids of the additional (guest) users on the caller's current session.
///
/// Mirrors the `session.AdditionalUsers` iteration in C# `MarkPlayedItem` /
/// `MarkUnplayedItem`: each guest is confirmed to still resolve (C#
/// `GetUserById` null-check → `404`) before its id is returned.
async fn additional_user_ids(
    state: &AppState,
    auth: &hermit_traits::options::AuthorizationInfo,
) -> Result<Vec<Uuid>, ApiError> {
    let session = current_session(state, auth).await?;
    let Some(additional) = session.additional_users else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::with_capacity(additional.len());
    for guest in additional {
        state
            .users
            .get_user_by_id(guest.user_id)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("user {}", guest.user_id)))?;
        ids.push(guest.user_id);
    }
    Ok(ids)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Users/{userId}/PlayedItems/{itemId}",
            post(mark_played_for_user).delete(unmark_played_for_user),
        )
        .route(
            "/UserPlayedItems/{itemId}",
            post(mark_played_item).delete(mark_unplayed_item),
        )
        .route("/Sessions/Playing", post(report_playback_start))
        .route("/Sessions/Playing/Progress", post(report_playback_progress))
        .route("/Sessions/Playing/Ping", post(ping_playback_session))
        .route("/Sessions/Playing/Stopped", post(report_playback_stopped))
        .route(
            "/PlayingItems/{itemId}",
            post(on_playback_start).delete(on_playback_stopped),
        )
        .route(
            "/PlayingItems/{itemId}/Progress",
            post(on_playback_progress),
        )
}
