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
//! - The .NET `event`s `SessionInfoWebSocketListener` subscribes to
//!   (`SessionStarted`, `PlaybackStart`, …) become one [`SessionEvent`]
//!   broadcast, [`SessionManager::subscribe_session_events`]. Their payloads
//!   are dropped: the listener only asks "send now, or throttled?", and the
//!   domain [`EventManager`](crate::events::EventManager) still carries the
//!   payloads for its own consumers.
//! - Generic overloads (`SendSyncPlayGroupUpdate<T>`,
//!   `SendMessageToUserSessions<T>`, `SendMessageToAdminSessions<T>`) collapse to
//!   a single method taking a pre-serialized JSON payload (a `&str`), which keeps
//!   the trait non-generic and object-safe.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` /
//!   `IProgress` are dropped for v1.
//!
//! The trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;
use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::SessionInfoDto;
use ferrofin_model::secret::Secret;
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, MessageCommand, PlayMethod, PlayRequest,
    PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest,
    SessionMessageType, TranscodingInfo,
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

/// A change the WebSocket `Sessions` stream reacts to: one of the
/// `ISessionManager` events `SessionInfoWebSocketListener` subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// A session was created (`SessionStarted`).
    SessionStarted,
    /// A session left the pool: ended, closed or logged out (`SessionEnded`).
    SessionEnded,
    /// A client reported the start of playback (`PlaybackStart`).
    PlaybackStart,
    /// A client reported the end of playback, or the idle check stopped it
    /// (`PlaybackStopped`).
    PlaybackStopped,
    /// Playback progress (`PlaybackProgress`); `is_automated` is upstream's
    /// `PlaybackProgressEventArgs.IsAutomated`: the server's own one-second
    /// estimate rather than a client check-in.
    PlaybackProgress {
        /// The server estimated this progress rather than a client reporting it.
        is_automated: bool,
    },
    /// A client reported its capabilities (`CapabilitiesChanged`).
    CapabilitiesChanged,
    /// A request arrived on a session that had been idle for a while, or
    /// created it (`SessionActivity`).
    SessionActivity,
}

/// One session, as a caller that only reads ids or counts from
/// `_sessionManager.Sessions` sees it (the `excludeActiveSessions` filter of
/// `GetResumeItems`, the library-change audience, the metrics gauges):
/// [`SessionManager::get_session_summaries`] lists these where
/// [`SessionManager::get_sessions`] copies out each full [`SessionInfoDto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session's user (`SessionInfo.UserId`), nil for a session with no
    /// user. Additional users are not listed.
    pub user_id: Uuid,
    /// The id of the item the session shows as playing (`NowPlayingItem.Id`),
    /// `None` while its `NowPlayingItem` is null.
    pub now_playing_item_id: Option<Uuid>,
    /// The session's `PlayState.PlayMethod`, as the session's DTO reports it.
    pub play_method: Option<PlayMethod>,
    /// Whether the session carries `TranscodingInfo`.
    pub is_transcoding: bool,
}

impl From<&SessionInfoDto> for SessionSummary {
    fn from(session: &SessionInfoDto) -> Self {
        Self {
            user_id: session.user_id,
            now_playing_item_id: session.now_playing_item.as_ref().map(|item| item.id),
            play_method: session
                .play_state
                .as_ref()
                .and_then(|state| state.play_method),
            is_transcoding: session.transcoding_info.is_some(),
        }
    }
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
    ///
    /// `user` is `None` for an API-key caller, which upstream's
    /// `RequestHelpers.GetSession` logs as a session with no user
    /// (`LogSessionActivity(..., user: null)`).
    async fn log_session_activity(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: Option<&UserEntity>,
    ) -> Result<SessionInfoDto, ServiceError>;

    /// Records session activity exactly as
    /// [`log_session_activity`](Self::log_session_activity) does, returning
    /// only the session id (C# `RequestHelpers.GetSessionId`): what the
    /// playstate and session handlers want on every report, without building
    /// the session DTO. `None` when the session has no id.
    ///
    /// The default reads the id off `log_session_activity`'s DTO; the concrete
    /// session manager overrides it to skip that DTO.
    async fn log_session_activity_id(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: Option<&UserEntity>,
    ) -> Result<Option<String>, ServiceError> {
        Ok(self
            .log_session_activity(
                app_name,
                app_version,
                device_id,
                device_name,
                remote_endpoint,
                user,
            )
            .await?
            .id)
    }

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

    /// Ends a session whose last WebSocket closed (C#
    /// `SessionManager.CloseIfNeededAsync`): the live stream it was playing is
    /// released first, which [`report_session_ended`](Self::report_session_ended)
    /// (a logout) does not do. The default only ends the session.
    async fn close_if_needed(&self, session_id: &str) -> Result<(), ServiceError> {
        self.report_session_ended(session_id).await
    }

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

    /// Whether any session of `user_id` could receive a pushed message, so a
    /// sender can skip building one nobody would hear (upstream's
    /// `SendMessageToUserSessions` builds its data only when the user has a
    /// session). The default answers `true`.
    async fn has_listener_for_user(&self, _user_id: Uuid) -> bool {
        true
    }

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

    /// Checks Jellyfin's `AssertCanControl` before mutating a named session.
    /// An empty controlling id denotes a trusted internal/API-key operation.
    /// Implementations without session authorization fail closed.
    async fn assert_can_control(
        &self,
        _controlling_session_id: &str,
        _session_id: &str,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::Forbidden(
            "session control is not available".to_owned(),
        ))
    }

    /// Adds an additional (guest) user to a session. Request handlers must
    /// authorize session control and attachment before calling this operation.
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

    /// Every session, unfiltered (C# `ISessionManager.Sessions`), as a
    /// [`SessionSummary`], in no particular order. It is for callers that
    /// read only ids or counts, and it copies no session DTO or
    /// `NowPlayingItem`.
    ///
    /// The default reduces the elevated [`Self::get_sessions`] view (a nil
    /// user with the API-key flag) to these summaries, so test fakes need no
    /// change. The concrete manager overrides it to read its session table
    /// directly.
    async fn get_session_summaries(&self) -> Result<Vec<SessionSummary>, ServiceError> {
        let sessions = self
            .get_sessions(Uuid::nil(), None, None, None, true)
            .await?;
        Ok(sessions.iter().map(SessionSummary::from).collect())
    }

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

    /// Releases a viewer's hold on a live stream and closes the stream when
    /// that release calls for it (C# `CloseLiveStreamIfNeededAsync`).
    ///
    /// `session_or_play_session_id` names the viewer, by either of its ids.
    /// Which streams count as held comes from play-state reports that name a
    /// `LiveStreamId`. Three cases:
    /// - the id holds the stream: the viewer's hold (under both ids) is dropped
    ///   and the stream is closed;
    /// - the stream has holders but not this id: nothing is closed;
    /// - the stream has no holder at all (no report named it, or its last
    ///   holder already released): the stream is closed.
    ///
    /// Closing is one `MediaSourceManager::close_live_stream`, which lowers the
    /// stream's consumer count; the tuner goes when the count reaches zero.
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

    /// Subscribes to the session events the WebSocket `Sessions` stream sends
    /// on (the C# `event` fields `SessionInfoWebSocketListener` attaches to).
    /// Only events raised after the call are received.
    ///
    /// `None`, the default kept for test fakes, means this manager raises no
    /// events, so a `Sessions` subscriber is sent nothing. It is not a polling
    /// fallback.
    fn subscribe_session_events(&self) -> Option<tokio::sync::broadcast::Receiver<SessionEvent>> {
        None
    }
}

fn _assert_object_safe_session_manager(_: &dyn SessionManager) {}

#[cfg(test)]
mod tests {
    use super::AuthenticationRequest;
    use ferrofin_model::secret::Secret;
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

    /// A summary keeps the user, the shown item's id, the play method and
    /// whether the session transcodes, and nothing else.
    #[test]
    fn a_session_summary_reduces_the_session_dto() {
        use super::SessionSummary;
        use ferrofin_model::dto::{BaseItemDto, SessionInfoDto};
        use ferrofin_model::session::{PlayMethod, PlayerStateInfo, TranscodingInfo};

        let user_id = Uuid::from_u128(1);
        let item_id = Uuid::from_u128(2);
        let playing = SessionInfoDto {
            user_id,
            now_playing_item: Some(BaseItemDto {
                id: item_id,
                ..BaseItemDto::default()
            }),
            play_state: Some(PlayerStateInfo {
                play_method: Some(PlayMethod::DirectStream),
                ..PlayerStateInfo::default()
            }),
            transcoding_info: Some(TranscodingInfo::default()),
            ..SessionInfoDto::default()
        };
        assert_eq!(
            SessionSummary::from(&playing),
            SessionSummary {
                user_id,
                now_playing_item_id: Some(item_id),
                play_method: Some(PlayMethod::DirectStream),
                is_transcoding: true,
            }
        );
        assert_eq!(
            SessionSummary::from(&SessionInfoDto::default()),
            SessionSummary {
                user_id: Uuid::nil(),
                now_playing_item_id: None,
                play_method: None,
                is_transcoding: false,
            }
        );
    }
}
