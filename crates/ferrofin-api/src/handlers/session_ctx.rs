//! Shared session-context resolution for the Playstate / Session handlers.
//!
//! Jellyfin's controllers call `RequestHelpers.GetSession` /
//! `RequestHelpers.GetSessionId`, which log the caller's activity via
//! `ISessionManager.LogSessionActivity` (client, version, device, remote IP,
//! resolved user) and return the resulting `SessionInfo`. The portable seam has
//! [`SessionManager::log_session_activity`](ferrofin_traits::session::SessionManager::log_session_activity),
//! so [`current_session`] reconstructs the same call from the authenticated
//! [`AuthorizationInfo`]. [`current_session_id`] makes it through
//! [`SessionManager::log_session_activity_id`](ferrofin_traits::session::SessionManager::log_session_activity_id),
//! which returns just the id.

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
    let caller = Caller::of(auth);
    let session = state
        .sessions
        .log_session_activity(
            caller.client,
            caller.version,
            caller.device_id,
            caller.device,
            caller.remote_endpoint,
            user.as_ref(),
        )
        .await?;
    Ok(session)
}

/// Resolves (and logs activity for) the caller's current session id (C#
/// `GetSessionId`): the same activity as [`current_session`], without building
/// the session DTO.
pub(crate) async fn current_session_id(
    state: &AppState,
    auth: &AuthorizationInfo,
) -> Result<String, ApiError> {
    let user = resolve_user_opt(state, auth, None).await?;
    let caller = Caller::of(auth);
    state
        .sessions
        .log_session_activity_id(
            caller.client,
            caller.version,
            caller.device_id,
            caller.device,
            caller.remote_endpoint,
            user.as_ref(),
        )
        .await?
        .ok_or_else(|| ApiError::NotFound("Session not found.".to_owned()))
}

/// The client and device fields `LogSessionActivity` is called with, from the
/// request's [`AuthorizationInfo`].
struct Caller<'a> {
    client: &'a str,
    version: &'a str,
    device_id: &'a str,
    device: &'a str,
    /// C# passes `httpContext.GetNormalizedRemoteIP().ToString()` here, so a
    /// session's `RemoteEndPoint` tracks a roaming client. The address is
    /// resolved by the auth layer and rides on `AuthorizationInfo`; passing ""
    /// would blank the value the login recorded, because `upsert_session`
    /// writes this field unconditionally.
    remote_endpoint: &'a str,
}

impl<'a> Caller<'a> {
    fn of(auth: &'a AuthorizationInfo) -> Self {
        Self {
            client: auth.client.as_deref().unwrap_or_default(),
            version: auth.version.as_deref().unwrap_or_default(),
            device_id: auth.device_id.as_deref().unwrap_or_default(),
            device: auth.device.as_deref().unwrap_or_default(),
            remote_endpoint: auth.remote_endpoint.as_deref().unwrap_or_default(),
        }
    }
}
