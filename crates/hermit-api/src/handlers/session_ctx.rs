//! Shared session-context resolution for the Playstate / Session handlers.
//!
//! Jellyfin's controllers call `RequestHelpers.GetSession` /
//! `RequestHelpers.GetSessionId`, which log the caller's activity via
//! `ISessionManager.LogSessionActivity` (client, version, device, remote IP,
//! resolved user) and return the resulting `SessionInfo`. The portable seam has
//! [`SessionManager::log_session_activity`](hermit_traits::session::SessionManager::log_session_activity),
//! so [`current_session`] reconstructs the same call from the authenticated
//! [`AuthorizationInfo`], and [`current_session_id`] returns just its id.

use hermit_model::dto::SessionInfoDto;
use hermit_traits::options::AuthorizationInfo;

use crate::error::ApiError;
use crate::handlers::items::resolve_user;
use crate::state::AppState;

/// Resolves (and logs activity for) the caller's current session.
///
/// Mirrors C# `RequestHelpers.GetSession`: resolve the authenticated user, then
/// `LogSessionActivity` with the client/device fields from the request's
/// [`AuthorizationInfo`]. A caller with no resolvable user is a `400` (via
/// [`resolve_user`]); a session the manager cannot open surfaces its
/// `ServiceError`.
pub(crate) async fn current_session(
    state: &AppState,
    auth: &AuthorizationInfo,
) -> Result<SessionInfoDto, ApiError> {
    let user = resolve_user(state, auth, None).await?;
    let session = state
        .sessions
        .log_session_activity(
            auth.client.as_deref().unwrap_or_default(),
            auth.version.as_deref().unwrap_or_default(),
            auth.device_id.as_deref().unwrap_or_default(),
            auth.device.as_deref().unwrap_or_default(),
            // The normalized remote IP is wired at the composition root's remote-
            // address layer, which is not part of the `AuthorizationInfo` seam;
            // the session manager only records it, so an empty value is safe here.
            "",
            &user,
        )
        .await?;
    Ok(session)
}

/// Resolves the caller's current session id (C# `GetSessionId`).
pub(crate) async fn current_session_id(
    state: &AppState,
    auth: &AuthorizationInfo,
) -> Result<String, ApiError> {
    let session = current_session(state, auth).await?;
    session
        .id
        .ok_or_else(|| ApiError::NotFound("Session not found.".to_owned()))
}
