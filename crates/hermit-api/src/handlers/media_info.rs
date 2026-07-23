//! `MediaInfoController` — playback-info resolution.
//!
//! Ports `GET`/`POST /Items/{itemId}/PlaybackInfo`: resolves the item's playable
//! [`MediaSourceInfo`](hermit_model::dto::MediaSourceInfo)s for the requesting
//! user via the [`MediaSourceManager`](hermit_traits::library::MediaSourceManager)
//! and returns them in a [`PlaybackInfoResponse`]. The `POST` body (a device
//! profile + stream selections) is accepted and ignored for now; both verbs
//! share one handler, matching Jellyfin's two actions.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use hermit_model::media_info::PlaybackInfoResponse;
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
        play_session_id: None,
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

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/Items/{itemId}/PlaybackInfo",
        get(get_playback_info).post(post_playback_info),
    )
}
