//! Session-layer manager trait — client sessions, playback reporting, commands.
//!
//! Port of `MediaBrowser.Controller.Session.ISessionManager` (plus the
//! [`AuthenticationRequest`] parameter type it consumes).
//!
//! Port rules applied throughout:
//! - The C# domain `User` argument of `LogSessionActivity` becomes a
//!   [`UserEntity`] row; `Device` arguments become [`DeviceEntity`] rows.
//! - The in-memory `SessionInfo` domain object is **not** ported; session reads
//!   surface as the [`SessionInfoDto`] wire DTO. The `AuthenticateNewSession` /
//!   `AuthenticateDirect` methods return an [`AuthenticationResultData`] — the
//!   session DTO plus the minted access token — mirroring Jellyfin's
//!   `AuthenticationResult` carrying its `AccessToken`. The full
//!   `{ UserDto, SessionInfoDto, AccessToken, ServerId }` wire envelope is
//!   assembled at the API layer from this data. The `ToSessionInfoDto` /
//!   `OnSessionControllerConnected` mapping helpers that take the un-ported
//!   `SessionInfo` are dropped.
//! - .NET `event`s (`PlaybackStart`, `SessionStarted`, …) are dropped; event
//!   wiring lives in `hermit-core`.
//! - Generic overloads (`SendSyncPlayGroupUpdate<T>`,
//!   `SendMessageToUserSessions<T>`, `SendMessageToAdminSessions<T>`) collapse to
//!   a single method taking a pre-serialized JSON payload (a `&str`), which keeps
//!   the trait non-generic and object-safe.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` /
//!   `IProgress` are dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use hermit_db::entities::security::DeviceEntity;
use hermit_db::entities::users::UserEntity;
use hermit_model::dto::SessionInfoDto;
use hermit_model::secret::Secret;
use hermit_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayRequest, PlaybackProgressInfo,
    PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest, SessionMessageType, TranscodingInfo,
};
use uuid::Uuid;

use crate::error::ServiceError;

/// The outcome of authenticating and opening a session: the session DTO plus
/// the freshly minted access token the client must present on subsequent
/// requests.
///
/// Port shape of the token-carrying half of Jellyfin's `AuthenticationResult`
/// (`SessionManager.AuthenticateNewSession` returns the result *with* its
/// `AccessToken`). The full `{ UserDto, SessionInfoDto, AccessToken, ServerId }`
/// envelope is assembled at the API layer from this data.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthenticationResultData {
    /// The opened session.
    pub session: SessionInfoDto,

    /// The minted access token backing the session's `Device` row. Clients
    /// present this on every subsequent authenticated request.
    pub access_token: Secret,
}

/// A request to authenticate and open a new session.
///
/// Port of `MediaBrowser.Controller.Session.AuthenticationRequest`. The obsolete
/// `PasswordSha1` field is dropped (Jellyfin marks it `[Obsolete]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthenticationRequest {
    /// The username to authenticate, if authenticating by name.
    pub username: Option<String>,

    /// The user id to authenticate, if authenticating by id.
    pub user_id: Option<Uuid>,

    /// The plaintext password.
    pub password: Option<Secret>,

    /// The client application name.
    pub app: Option<String>,

    /// The client application version.
    pub app_version: Option<String>,

    /// The client-reported device id.
    pub device_id: Option<String>,

    /// The client-reported device name.
    pub device_name: Option<String>,

    /// The remote endpoint (client IP) the request came from.
    pub remote_endpoint: Option<String>,
}

/// Orchestrates client sessions: activity, playback reporting, and remote
/// commands.
///
/// Port of `ISessionManager` (the object-safe, domain-`SessionInfo`-free
/// subset). Reads return [`SessionInfoDto`]; the authenticate methods return an
/// [`AuthenticationResultData`] (session DTO + minted access token).
#[async_trait]
pub trait SessionManager: Send + Sync {
    /// Records session activity for a client, returning the session DTO.
    async fn log_session_activity(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: &UserEntity,
    ) -> Result<SessionInfoDto, ServiceError>;

    /// Updates the reported device name of a session.
    async fn update_device_name(
        &self,
        session_id: &str,
        reported_device_name: &str,
    ) -> Result<(), ServiceError>;

    /// Reports that playback started for an item.
    async fn on_playback_start(&self, info: &PlaybackStartInfo) -> Result<(), ServiceError>;

    /// Reports playback progress for an item.
    async fn on_playback_progress(
        &self,
        info: &PlaybackProgressInfo,
        is_automated: bool,
    ) -> Result<(), ServiceError>;

    /// Reports that playback stopped for an item.
    async fn on_playback_stopped(&self, info: &PlaybackStopInfo) -> Result<(), ServiceError>;

    /// Reports that a session has ended.
    async fn report_session_ended(&self, session_id: &str) -> Result<(), ServiceError>;

    /// Sends a general command to a controlled session.
    async fn send_general_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &GeneralCommand,
    ) -> Result<(), ServiceError>;

    /// Sends a display-message command to a controlled session.
    async fn send_message_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &MessageCommand,
    ) -> Result<(), ServiceError>;

    /// Sends a play command to a controlled session.
    async fn send_play_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &PlayRequest,
    ) -> Result<(), ServiceError>;

    /// Sends a playstate command to a controlled session.
    async fn send_playstate_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &PlaystateRequest,
    ) -> Result<(), ServiceError>;

    /// Sends a pre-serialized message to all admin sessions.
    ///
    /// Collapses the C# generic `SendMessageToAdminSessions<T>`; `data` is the
    /// payload already serialized to a JSON string upstream, keeping the trait
    /// non-generic and object-safe.
    async fn send_message_to_admin_sessions(
        &self,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError>;

    /// Sends a pre-serialized JSON message to all sessions of the given users.
    async fn send_message_to_user_sessions(
        &self,
        user_ids: &[Uuid],
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError>;

    /// Sends a pre-serialized JSON message to every session with a signed-in
    /// user (the delivery target of Jellyfin's library/server notifiers, e.g.
    /// `LibraryChanged`). Defaulted to a no-op so lightweight test doubles need
    /// not implement delivery; the concrete session manager overrides it.
    async fn send_message_to_all_sessions(
        &self,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError> {
        let _ = (message_type, data);
        Ok(())
    }

    /// Sends a pre-serialized JSON message to all sessions of a specific device.
    async fn send_message_to_user_device_sessions(
        &self,
        device_id: &str,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError>;

    /// Broadcasts a "restart required" notification to all sessions.
    async fn send_restart_required_notification(&self) -> Result<(), ServiceError>;

    /// Adds an additional (guest) user to a session.
    async fn add_additional_user(
        &self,
        session_id: &str,
        user_id: Uuid,
    ) -> Result<(), ServiceError>;

    /// Removes an additional (guest) user from a session.
    async fn remove_additional_user(
        &self,
        session_id: &str,
        user_id: Uuid,
    ) -> Result<(), ServiceError>;

    /// Reports the item a session is now viewing.
    async fn report_now_viewing_item(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Result<(), ServiceError>;

    /// Authenticates a request and opens a new session.
    ///
    /// Returns the opened session together with the minted access token (see
    /// [`AuthenticationResultData`]) so the API layer can echo the token in the
    /// `AuthenticationResult` body.
    async fn authenticate_new_session(
        &self,
        request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError>;

    /// Authenticates directly (bypassing the interactive flow), opening a
    /// session.
    ///
    /// Returns the opened session together with the minted access token (see
    /// [`AuthenticationResultData`]).
    async fn authenticate_direct(
        &self,
        request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError>;

    /// Records the reported capabilities of a session.
    async fn report_capabilities(
        &self,
        session_id: &str,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError>;

    /// Records live transcoding information for a device.
    async fn report_transcoding_info(
        &self,
        device_id: &str,
        info: &TranscodingInfo,
    ) -> Result<(), ServiceError>;

    /// Clears any recorded transcoding information for a device.
    async fn clear_transcoding_info(&self, device_id: &str) -> Result<(), ServiceError>;

    /// Gets the sessions visible to a user, filtered by device/activity.
    async fn get_sessions(
        &self,
        user_id: Uuid,
        device_id: Option<&str>,
        active_within_seconds: Option<i32>,
        controllable_user_to_check: Option<Uuid>,
        is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError>;

    /// Resolves a session from its access token, returning the session DTO.
    async fn get_session_by_authentication_token(
        &self,
        token: &str,
        device_id: &str,
        remote_endpoint: &str,
    ) -> Result<SessionInfoDto, ServiceError>;

    /// Logs out the session identified by an access token.
    async fn logout(&self, access_token: &str) -> Result<(), ServiceError>;

    /// Logs out the session bound to a specific device row.
    async fn logout_device(&self, device: &DeviceEntity) -> Result<(), ServiceError>;

    /// Revokes all of a user's tokens except the current one.
    async fn revoke_user_tokens(
        &self,
        user_id: Uuid,
        current_access_token: &str,
    ) -> Result<(), ServiceError>;

    /// Closes the given live stream if no session still needs it.
    async fn close_live_stream_if_needed(
        &self,
        live_stream_id: &str,
        session_or_play_session_id: &str,
    ) -> Result<(), ServiceError>;

    /// Whether any session is currently playing something (`NowPlayingItem`
    /// set). Maintenance that would disrupt live playback — the database
    /// `VACUUM`/checkpoint, the transcode-directory sweep — gates on this and
    /// skips its run while it is `true` (a real black-screen incident traced
    /// to exclusive locks / segment deletion during playback).
    ///
    /// The default is `false` so the many test fakes need no change; the
    /// concrete session manager MUST override it — a real implementation
    /// leaning on this default silently disables the playback guard.
    async fn has_active_playback(&self) -> Result<bool, ServiceError> {
        Ok(false)
    }
}

fn _assert_object_safe_session_manager(_: &dyn SessionManager) {}

#[cfg(test)]
mod tests {
    use super::AuthenticationRequest;
    use hermit_model::secret::Secret;
    use uuid::Uuid;

    #[test]
    fn authentication_request_default_is_empty() {
        let req = AuthenticationRequest::default();
        assert_eq!(req.username, None);
        assert_eq!(req.user_id, None);
        assert_eq!(req.password, None);
    }

    #[test]
    fn authentication_request_carries_fields() {
        let id = Uuid::from_u128(0x42);
        let req = AuthenticationRequest {
            username: Some("alice".to_owned()),
            user_id: Some(id),
            password: Some(Secret::new("hunter2")),
            ..Default::default()
        };
        assert_eq!(req.username.as_deref(), Some("alice"));
        assert_eq!(req.user_id, Some(id));
    }
}
