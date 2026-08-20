//! `SyncPlayController` — synchronized group playback.
//!
//! Ports Jellyfin's `SyncPlayController`. Group lifecycle (`New`/`Join`/`Leave`/
//! `List`/`{id}`) and the 17 playback requests all resolve the caller's session
//! ([`sync_play_session`]) and drive the real [`SyncPlayManager`], which mutates
//! live group state and pushes `SyncPlayCommand`/`SyncPlayGroupUpdate` messages
//! to member sockets via the session message bus.
//!
//! Every route is gated by the user's `SyncPlayAccess` policy first — the C#
//! `[Authorize(Policy = Policies.SyncPlay*)]` attributes, which land here as
//! [`require_access`] because Ferrofin has no attribute-driven policy layer.
//!
//! The manager is wired at the composition root
//! ([`AppState::with_sync_play`](crate::state::AppState::with_sync_play)); until
//! then these routes return `501`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ferrofin_model::sync_play::{
    BufferRequestDto, GroupInfoDto, IgnoreWaitRequestDto, JoinGroupRequestDto,
    MovePlaylistItemRequestDto, NewGroupRequestDto, NextItemRequestDto, PingRequestDto,
    PlayRequestDto, PreviousItemRequestDto, QueueRequestDto, ReadyRequestDto,
    RemoveFromPlaylistRequestDto, SeekRequestDto, SetPlaylistItemRequestDto,
    SetRepeatModeRequestDto, SetShuffleModeRequestDto,
};
use ferrofin_model::users::SyncPlayUserAccessType;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::stubs::{PlaybackRequest, SyncPlayManager, SyncPlaySession};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::{resolve_user_opt, user_uuid};
use crate::handlers::session_ctx::current_session;
use crate::state::AppState;

/// The wired SyncPlay manager, or `501` when the subsystem is not composed in.
fn manager(state: &AppState) -> Result<&Arc<dyn SyncPlayManager>, ApiError> {
    state.sync_play.as_ref().ok_or(ApiError::NotImplemented)
}

/// Resolves (and logs activity for) the caller's [`SyncPlaySession`] — the
/// session id (group-membership key) plus the user id/name used for
/// participants and access checks. Mirrors C# `RequestHelpers.GetSession`.
async fn sync_play_session(
    state: &AppState,
    auth: &AuthorizationInfo,
) -> Result<SyncPlaySession, ApiError> {
    let session = current_session(state, auth).await?;
    let session_id = session
        .id
        .ok_or_else(|| ApiError::NotFound("Session not found.".to_owned()))?;
    Ok(SyncPlaySession {
        session_id,
        user_id: session.user_id,
        user_name: session.user_name.unwrap_or_default(),
    })
}

/// The SyncPlay authorization requirement a route carries — port of
/// `Jellyfin.Api.Auth.SyncPlayAccessPolicy.SyncPlayAccessRequirementType`.
///
/// Jellyfin also puts `SyncPlayHasAccess` on the controller class, ANDed with
/// each route's own requirement. It is not evaluated separately here because
/// every one of the three below already implies it: `HasAccess` holds when the
/// user may create *or* join groups, or is already in one — which is exactly
/// what `CreateGroup` / `JoinGroup` / `IsInGroup` each establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The shared `Group` postfix is upstream's naming (`SyncPlayAccessRequirementType`),
// kept so the table below reads against the C# it ports.
#[allow(clippy::enum_variant_names)]
enum SyncPlayAccess {
    /// `POST /SyncPlay/New` — the user's policy must allow creating groups.
    CreateGroup,
    /// `Join` / `List` / `{id}` — the policy must allow joining groups.
    JoinGroup,
    /// `Leave` + the 17 playback verbs — the user must already be in a group.
    IsInGroup,
}

/// Enforces one [`SyncPlayAccess`] requirement for the caller, `403` otherwise
/// (C# `SyncPlayAccessHandler`, whose failed requirement is a `ForbidResult`).
async fn require_access(
    state: &AppState,
    auth: &AuthorizationInfo,
    required: SyncPlayAccess,
) -> Result<(), ApiError> {
    let mgr = manager(state)?;
    // An API-key caller has no user row. Upstream evaluates the policy against
    // `GetUserId()` and refuses, so this is a `403` — not the `400` that
    // `resolve_user` reports for a user-less request.
    let Some(user) = resolve_user_opt(state, auth, None).await? else {
        return Err(ApiError::Forbidden(
            "SyncPlay requires a signed-in user.".to_owned(),
        ));
    };
    let access = SyncPlayUserAccessType::from_stored(user.sync_play_access);
    let permitted = match required {
        SyncPlayAccess::CreateGroup => access.can_create_groups(),
        SyncPlayAccess::JoinGroup => access.can_join_groups(),
        SyncPlayAccess::IsInGroup => mgr.is_user_active(user_uuid(&user)?).await?,
    };
    if permitted {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "User does not have access to SyncPlay.".to_owned(),
        ))
    }
}

/// Applies a playback [`PlaybackRequest`] to the caller's group and returns
/// `204` — the shared body of the 17 playback endpoints.
async fn dispatch(
    state: &AppState,
    auth: &AuthorizationInfo,
    request: PlaybackRequest,
) -> Result<StatusCode, ApiError> {
    require_access(state, auth, SyncPlayAccess::IsInGroup).await?;
    let mgr = manager(state)?;
    let session = sync_play_session(state, auth).await?;
    mgr.handle_request(&session, request).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── group lifecycle ────────────────────────────────────────────────────────

/// `POST /SyncPlay/New` — create a group owned by the caller.
async fn new_group(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<NewGroupRequestDto>,
) -> Result<Json<GroupInfoDto>, ApiError> {
    require_access(&state, &auth, SyncPlayAccess::CreateGroup).await?;
    let mgr = manager(&state)?;
    let session = sync_play_session(&state, &auth).await?;
    Ok(Json(mgr.new_group(&session, &body.group_name).await?))
}

/// `POST /SyncPlay/Join` — join an existing group.
async fn join_group(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<JoinGroupRequestDto>,
) -> Result<StatusCode, ApiError> {
    require_access(&state, &auth, SyncPlayAccess::JoinGroup).await?;
    let mgr = manager(&state)?;
    let session = sync_play_session(&state, &auth).await?;
    mgr.join_group(&session, body.group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /SyncPlay/Leave` — leave the caller's current group.
async fn leave_group(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    require_access(&state, &auth, SyncPlayAccess::IsInGroup).await?;
    let mgr = manager(&state)?;
    let session = sync_play_session(&state, &auth).await?;
    mgr.leave_group(&session).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /SyncPlay/List` — the groups visible to the caller.
async fn list_groups(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<GroupInfoDto>>, ApiError> {
    require_access(&state, &auth, SyncPlayAccess::JoinGroup).await?;
    let mgr = manager(&state)?;
    let session = sync_play_session(&state, &auth).await?;
    Ok(Json(mgr.list_groups(&session).await?))
}

/// `GET /SyncPlay/{id}` — info for a single group.
async fn get_group(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
) -> Result<Json<GroupInfoDto>, ApiError> {
    require_access(&state, &auth, SyncPlayAccess::JoinGroup).await?;
    let mgr = manager(&state)?;
    let session = sync_play_session(&state, &auth).await?;
    Ok(Json(mgr.get_group(&session, group_id).await?))
}

// ── playback requests ──────────────────────────────────────────────────────

/// `POST /SyncPlay/SetNewQueue` — set a new play queue.
async fn set_new_queue(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<PlayRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::Play {
            playing_queue: body.playing_queue,
            playing_item_position: body.playing_item_position,
            start_position_ticks: body.start_position_ticks,
        },
    )
    .await
}

/// `POST /SyncPlay/SetPlaylistItem` — change the playing item.
async fn set_playlist_item(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<SetPlaylistItemRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::SetPlaylistItem {
            playlist_item_id: body.playlist_item_id,
        },
    )
    .await
}

/// `POST /SyncPlay/RemoveFromPlaylist` — remove queue entries (or clear).
async fn remove_from_playlist(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<RemoveFromPlaylistRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::RemoveFromPlaylist {
            playlist_item_ids: body.playlist_item_ids,
            clear_playlist: body.clear_playlist,
            clear_playing_item: body.clear_playing_item,
        },
    )
    .await
}

/// `POST /SyncPlay/MovePlaylistItem` — reorder a queue entry.
async fn move_playlist_item(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<MovePlaylistItemRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::MovePlaylistItem {
            playlist_item_id: body.playlist_item_id,
            new_index: body.new_index,
        },
    )
    .await
}

/// `POST /SyncPlay/Queue` — enqueue items.
async fn queue(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<QueueRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::Queue {
            item_ids: body.item_ids,
            mode: body.mode,
        },
    )
    .await
}

/// `POST /SyncPlay/Unpause` — resume group playback.
async fn unpause(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    dispatch(&state, &auth, PlaybackRequest::Unpause).await
}

/// `POST /SyncPlay/Pause` — pause group playback.
async fn pause(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    dispatch(&state, &auth, PlaybackRequest::Pause).await
}

/// `POST /SyncPlay/Stop` — stop group playback.
async fn stop(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    dispatch(&state, &auth, PlaybackRequest::Stop).await
}

/// `POST /SyncPlay/Seek` — seek the group.
async fn seek(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<SeekRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::Seek {
            position_ticks: body.position_ticks,
        },
    )
    .await
}

/// `POST /SyncPlay/Buffering` — signal the caller is buffering.
async fn buffering(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<BufferRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::Buffer {
            when: body.when,
            position_ticks: body.position_ticks,
            is_playing: body.is_playing,
            playlist_item_id: body.playlist_item_id,
        },
    )
    .await
}

/// `POST /SyncPlay/Ready` — signal the caller is ready.
async fn ready(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<ReadyRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::Ready {
            when: body.when,
            position_ticks: body.position_ticks,
            is_playing: body.is_playing,
            playlist_item_id: body.playlist_item_id,
        },
    )
    .await
}

/// `POST /SyncPlay/SetIgnoreWait` — toggle whether the caller is waited for.
async fn set_ignore_wait(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<IgnoreWaitRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::IgnoreWait {
            ignore_wait: body.ignore_wait,
        },
    )
    .await
}

/// `POST /SyncPlay/NextItem` — advance to the next queue item.
async fn next_item(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<NextItemRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::NextItem {
            playlist_item_id: body.playlist_item_id,
        },
    )
    .await
}

/// `POST /SyncPlay/PreviousItem` — go back to the previous queue item.
async fn previous_item(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<PreviousItemRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::PreviousItem {
            playlist_item_id: body.playlist_item_id,
        },
    )
    .await
}

/// `POST /SyncPlay/SetRepeatMode` — set the group repeat mode.
async fn set_repeat_mode(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<SetRepeatModeRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::SetRepeatMode { mode: body.mode },
    )
    .await
}

/// `POST /SyncPlay/SetShuffleMode` — set the group shuffle mode.
async fn set_shuffle_mode(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<SetShuffleModeRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(
        &state,
        &auth,
        PlaybackRequest::SetShuffleMode { mode: body.mode },
    )
    .await
}

/// `POST /SyncPlay/Ping` — report the caller's measured ping.
async fn ping(
    RequireAuth(auth): RequireAuth,
    State(state): State<AppState>,
    Json(body): Json<PingRequestDto>,
) -> Result<StatusCode, ApiError> {
    dispatch(&state, &auth, PlaybackRequest::Ping { ping: body.ping }).await
}

/// Registers the `/SyncPlay/*` routes.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/SyncPlay/New", post(new_group))
        .route("/SyncPlay/Join", post(join_group))
        .route("/SyncPlay/Leave", post(leave_group))
        .route("/SyncPlay/List", get(list_groups))
        .route("/SyncPlay/{id}", get(get_group))
        .route("/SyncPlay/SetNewQueue", post(set_new_queue))
        .route("/SyncPlay/SetPlaylistItem", post(set_playlist_item))
        .route("/SyncPlay/RemoveFromPlaylist", post(remove_from_playlist))
        .route("/SyncPlay/MovePlaylistItem", post(move_playlist_item))
        .route("/SyncPlay/Queue", post(queue))
        .route("/SyncPlay/Unpause", post(unpause))
        .route("/SyncPlay/Pause", post(pause))
        .route("/SyncPlay/Stop", post(stop))
        .route("/SyncPlay/Seek", post(seek))
        .route("/SyncPlay/Buffering", post(buffering))
        .route("/SyncPlay/Ready", post(ready))
        .route("/SyncPlay/SetIgnoreWait", post(set_ignore_wait))
        .route("/SyncPlay/NextItem", post(next_item))
        .route("/SyncPlay/PreviousItem", post(previous_item))
        .route("/SyncPlay/SetRepeatMode", post(set_repeat_mode))
        .route("/SyncPlay/SetShuffleMode", post(set_shuffle_mode))
        .route("/SyncPlay/Ping", post(ping))
}
