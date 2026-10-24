//! Shared session-context resolution for the Playstate / Session handlers.
//!
//! Jellyfin's controllers call `RequestHelpers.GetSession` /
//! `RequestHelpers.GetSessionId`, which log the caller's activity via
//! `ISessionManager.LogSessionActivity` (client, version, device, remote IP,
//! resolved user) and return the resulting `SessionInfo`. The portable seam has
//! [`SessionManager::log_session_activity`](ferrofin_traits::session::SessionManager::log_session_activity),
//! so [`current_session`] reconstructs the same call from the authenticated
//! [`AuthorizationInfo`], and [`current_session_id`] returns just its id.

use ferrofin_model::dto::SessionInfoDto;
use ferrofin_traits::options::AuthorizationInfo;

use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::state::AppState;

/// Resolves (and logs activity for) the caller's current session.
///
/// Mirrors C# `RequestHelpers.GetSession`: resolve the caller's user when it
/// has one (`if (!userId.IsNullOrEmpty()) user = GetUserById(userId)`), then
/// `LogSessionActivity` with the client/device fields from the request's
/// [`AuthorizationInfo`] and that user, possibly none. So an API key, which
/// has no user, gets a user-less session and the request goes on. A session
/// the manager cannot open surfaces its `ServiceError`.
pub(crate) async fn current_session(
    state: &AppState,
    auth: &AuthorizationInfo,
) -> Result<SessionInfoDto, ApiError> {
    let user = resolve_user_opt(state, auth, None).await?;
    let session = state
        .sessions
        .log_session_activity(
            auth.client.as_deref().unwrap_or_default(),
            auth.version.as_deref().unwrap_or_default(),
            auth.device_id.as_deref().unwrap_or_default(),
            auth.device.as_deref().unwrap_or_default(),
            // C# passes `httpContext.GetNormalizedRemoteIP().ToString()` here, so
            // a session's `RemoteEndPoint` tracks a roaming client. The address
            // is resolved by the auth layer and rides on `AuthorizationInfo`;
            // passing "" would blank the value the login recorded, because
            // `upsert_session` writes this field unconditionally.
            auth.remote_endpoint.as_deref().unwrap_or_default(),
            user.as_ref(),
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
