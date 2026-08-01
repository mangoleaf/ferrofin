//! [`HermitSessionManager`] — the concrete [`SessionManager`] over injected
//! siblings + in-memory session state.
//!
//! Port of `Emby.Server.Implementations.Session.SessionManager` (the largest
//! implementation in the crate, ~81K of C#). It owns the **in-memory**
//! [`SessionInfo`] table (the C# `_activeConnections` `ConcurrentDictionary`),
//! reports playback through the injected [`UserDataManager`], and broadcasts
//! server → client messages to each session's live WebSocket controllers.
//!
//! ## Port rules applied
//! - **Sibling managers are injected** (`Arc<dyn _>`): [`UserManager`],
//!   [`DeviceManager`], [`UserDataManager`], [`LibraryManager`], [`DtoService`]
//!   and the [`EventManager`] publish seam. The `IMusicManager` /
//!   `IImageProcessor` / `IMediaSourceManager` used only for instant-mix
//!   translation and image-cache tags are **not** injected into this unit; the
//!   features that need them (instant-mix `SendPlayCommand`, user-image tags,
//!   live-stream media-source resolution) are documented deferrals.
//! - **The domain `SessionInfo` is modelled here**, not in `hermit-traits`
//!   (whose reads surface the [`SessionInfoDto`] wire type). It is a private
//!   in-memory struct; [`session_info_to_dto`] maps it to the DTO on read.
//! - **Message broadcast collapses to pre-serialized JSON** (the trait takes a
//!   `&str` payload). Each session holds its live
//!   `Arc<dyn `[`WebSocketConnection`]`>` controllers; a broadcast serializes a
//!   small `{ MessageType, Data }` envelope once and `send`s the bytes to every
//!   controller. The `SessionControllers`/`WebSocketController` indirection of
//!   the C# collapses into "the session's connection handles".
//! - **`AuthenticationResult`** now lands in `hermit-model`; the authenticate
//!   methods return an [`AuthenticationResultData`] (the [`SessionInfoDto`] plus
//!   the minted access token) from which the API layer assembles the wire
//!   envelope. The access token is minted via [`DeviceManager`].
//! - **Idle/inactive timers and `IAsyncDisposable`** are dropped — no real
//!   scheduler in this crate (that is Wave 8 / scheduled tasks). Automatic
//!   progress is tracked as a flag on the in-memory session.
//! - **Exceptions → `Result<_, ServiceError>`**: `SecurityException` /
//!   `AuthenticationException` → [`ServiceError::Unauthorized`]; a missing
//!   session / user → [`ServiceError::NotFound`]; bad arguments →
//!   [`ServiceError::InvalidInput`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{error, info};
use uuid::Uuid;

use hermit_db::Database;
use hermit_db::entities::security::DeviceEntity;
use hermit_db::entities::users::UserEntity;
use hermit_db::enums::PermissionKind;
use hermit_model::dto::SessionInfoDto;
use hermit_model::session::{
    ClientCapabilities, GeneralCommand, GeneralCommandType, MessageCommand, PlayRequest,
    PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest,
    SessionMessageType, SessionUserInfo, TranscodingInfo,
};

use hermit_traits::devices::{DeviceManager, DeviceQuery};
use hermit_traits::dto::DtoService;
use hermit_traits::error::ServiceError;
use hermit_traits::events::EventManager;
use hermit_traits::library::{LibraryManager, UserDataManager, UserManager};
use hermit_traits::net::WebSocketConnection;
use hermit_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use hermit_traits::session_bus::SessionMessageBus;

use crate::db_error::db_err;
use crate::user_entity_ext::has_permission;

/// The default device name applied when a client reports an empty one (C#
/// `CreateSessionInfo` → `"Network Device"`).
const DEFAULT_DEVICE_NAME: &str = "Network Device";

/// The in-memory session-state object the C# `SessionInfo` domain class becomes.
///
/// Deliberately **not** in `hermit-traits` (its reads surface the wire
/// [`SessionInfoDto`]). Holds the mutable per-session state plus the live
/// WebSocket controllers used to push messages. Cheaply cloneable snapshots
/// feed [`session_info_to_dto`].
#[derive(Clone)]
struct SessionInfo {
    /// The stable session id (MD5 of `app + deviceId`, hex, matching C#).
    id: String,
    user_id: Uuid,
    user_name: Option<String>,
    client: Option<String>,
    device_id: String,
    device_name: Option<String>,
    application_version: Option<String>,
    remote_end_point: Option<String>,
    server_id: Option<String>,
    has_custom_device_name: bool,
    last_activity_date: DateTime<Utc>,
    last_playback_check_in: DateTime<Utc>,
    last_paused_date: Option<DateTime<Utc>>,
    now_playing_item_id: Option<Uuid>,
    now_viewing_item_id: Option<Uuid>,
    additional_users: Vec<SessionUserInfo>,
    capabilities: ClientCapabilities,
    transcoding_info: Option<TranscodingInfo>,
    /// Whether automatic progress reporting was started for the current item
    /// (C# `StartAutomaticProgress`). No timer runs in this crate; the flag
    /// preserves the observable state.
    is_playing: bool,
    /// The live WebSocket connections attached to this session (the C#
    /// `SessionControllers`). Broadcasts push serialized bytes to each.
    connections: Vec<Arc<dyn WebSocketConnection>>,
}

impl SessionInfo {
    /// Whether the session references `user_id` as its primary or an additional
    /// user (C# `SessionInfo.ContainsUser`).
    fn contains_user(&self, user_id: Uuid) -> bool {
        self.user_id == user_id || self.additional_users.iter().any(|u| u.user_id == user_id)
    }

    /// Whether the session has a live (open) connection — a directly attached controller.
    fn is_active(&self) -> bool {
        self.connections.iter().any(|c| c.is_open())
    }

    /// Whether the session has any connection object at all (open or not).
    fn has_connections(&self) -> bool {
        !self.connections.is_empty()
    }

    /// Whether the session can be remote-controlled — it reports media control
    /// and has a live controller (C# `SupportsRemoteControl`).
    fn supports_remote_control(&self) -> bool {
        self.capabilities.supports_media_control && self.is_active()
    }
}

/// The server → client message envelope pushed over a WebSocket.
///
/// The C# `WebSocketMessage<T>` carries `{ MessageType, MessageId, Data }`; the
/// object-safe trait already reduced `Data` to a pre-serialized JSON string, so
/// this envelope embeds it verbatim as a raw JSON value.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OutboundMessage<'a> {
    message_type: SessionMessageType,
    message_id: String,
    /// The already-serialized payload, embedded as raw JSON.
    data: serde_json::Value,
    #[serde(skip)]
    _marker: std::marker::PhantomData<&'a ()>,
}

/// The concrete session manager.
///
/// Holds injected siblings and the in-memory session table. The table is an
/// async [`Mutex`] because a broadcast holds it while `.await`ing sends to the
/// sessions' WebSocket connections.
#[derive(Clone)]
pub struct HermitSessionManager {
    user_manager: Arc<dyn UserManager>,
    device_manager: Arc<dyn DeviceManager>,
    user_data_manager: Arc<dyn UserDataManager>,
    library_manager: Arc<dyn LibraryManager>,
    /// Injected DTO-assembly seam. Held for the deferred now-playing-item DTO
    /// enrichment (C# `UpdateNowPlayingItem` builds a `BaseItemDto`); not yet
    /// read because this unit stores only the now-playing item **id**.
    #[allow(dead_code)]
    dto_service: Arc<dyn DtoService>,
    event_manager: Arc<dyn EventManager>,
    /// The database handle, used for the permission checks the injected traits
    /// do not surface (the same escape hatch `HermitDtoService` uses). Reads
    /// only the `Permissions` table via [`has_permission`].
    db: Database,
    /// This server's stable id, stamped onto each session (C# `SystemId`).
    server_id: String,
    /// The pool of active sessions keyed by session key (`app + deviceId`),
    /// matching the C# `_activeConnections` keying.
    sessions: Arc<Mutex<HashMap<String, SessionInfo>>>,
    /// The session message bus the HTTP WebSocket handler registers client
    /// sinks on. A bus-registered socket counts as a live controller (it drives
    /// `SupportsRemoteControl`) and is the delivery path for remote-control
    /// pushes when no [`WebSocketConnection`] is attached directly.
    bus: Option<Arc<dyn SessionMessageBus>>,
}

impl std::fmt::Debug for HermitSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitSessionManager")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl HermitSessionManager {
    /// Creates a session manager over the injected siblings.
    ///
    /// `server_id` is this server's stable id (the composition root's
    /// `SystemId`); it is stamped onto every session and the
    /// `AuthenticationResult`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_manager: Arc<dyn UserManager>,
        device_manager: Arc<dyn DeviceManager>,
        user_data_manager: Arc<dyn UserDataManager>,
        library_manager: Arc<dyn LibraryManager>,
        dto_service: Arc<dyn DtoService>,
        event_manager: Arc<dyn EventManager>,
        db: Database,
        server_id: impl Into<String>,
    ) -> Self {
        Self {
            user_manager,
            device_manager,
            user_data_manager,
            library_manager,
            dto_service,
            event_manager,
            db,
            server_id: server_id.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            bus: None,
        }
    }

    /// Attaches the session message bus, so sessions whose WebSocket registered
    /// a sink there count as live (remote-controllable) and receive pushes.
    #[must_use]
    pub fn with_session_bus(mut self, bus: Arc<dyn SessionMessageBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Whether the session has any live controller — a directly attached
    /// [`WebSocketConnection`] or a sink registered on the session bus.
    fn session_is_active(&self, session: &SessionInfo) -> bool {
        session.is_active()
            || self
                .bus
                .as_ref()
                .is_some_and(|bus| bus.is_connected(&session.id))
    }

    /// C# `SessionInfo.IsActive` for the DTO: a session with NO controllers at all is still
    /// active (e.g. an HTTP-only session that never opened a websocket); with controllers, one
    /// must be live. Distinct from remote-controllability, which always needs a live controller.
    fn session_is_active_dto(&self, session: &SessionInfo) -> bool {
        !session.has_connections() || self.session_is_active(session)
    }

    /// Bus-aware `SupportsRemoteControl` (C# `SessionInfo.SupportsRemoteControl`).
    fn session_supports_remote_control(&self, session: &SessionInfo) -> bool {
        session.capabilities.supports_media_control && self.session_is_active(session)
    }

    /// Maps a session to its wire DTO with the liveness fields computed against
    /// the bus (the free [`session_info_to_dto`] only sees direct connections).
    fn to_dto(&self, session: &SessionInfo) -> SessionInfoDto {
        let mut dto = session_info_to_dto(session);
        dto.is_active = self.session_is_active_dto(session);
        dto.supports_remote_control = self.session_supports_remote_control(session);
        dto
    }

    /// Attaches a live WebSocket connection to the session identified by
    /// `session_id`, so subsequent broadcasts reach that client.
    ///
    /// This is the seam the [`WebSocketListener`](hermit_traits::net::WebSocketListener)
    /// (`SessionWebSocketListener`) calls on connect — the C#
    /// `EnsureController` + `OnSessionControllerConnected`. Beyond the trait
    /// surface because the trait models no connection type.
    ///
    /// # Errors
    /// Returns [`ServiceError::NotFound`] when no session has that id.
    pub async fn add_web_socket(
        &self,
        session_id: &str,
        connection: Arc<dyn WebSocketConnection>,
    ) -> Result<(), ServiceError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .values_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))?;
        session.connections.push(connection);
        Ok(())
    }

    /// Computes the session key the pool is indexed by (C# `GetSessionKey`).
    fn session_key(app_name: &str, device_id: &str) -> String {
        format!("{app_name}{device_id}")
    }

    /// The stable session id derived from the session key (C#
    /// `key.GetMD5().ToString("N")`) — the shared
    /// [`get_md5`](hermit_common::extensions::get_md5) reproduces the .NET `Guid`;
    /// `simple()` renders it as 32 lowercase hex digits without dashes.
    fn session_id_for_key(key: &str) -> String {
        hermit_common::extensions::get_md5(key).simple().to_string()
    }

    /// Records session activity, creating the session on first contact
    /// (C# `LogSessionActivity` → `GetSessionInfo`/`CreateSessionInfo`).
    async fn upsert_session(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: Option<&UserEntity>,
    ) -> Result<SessionInfo, ServiceError> {
        if device_id.is_empty() {
            return Err(ServiceError::invalid_input("deviceId is required"));
        }

        let key = Self::session_key(app_name, device_id);
        let now = Utc::now();
        let user_id = user.map_or(Uuid::nil(), |u| parse_user_id(&u.id));
        let user_name = user.map(|u| u.username.clone());

        // The custom device name, if the device has one persisted.
        let custom_name = self
            .device_manager
            .get_device_options(device_id)
            .await?
            .and_then(|o| o.custom_name);

        let mut sessions = self.sessions.lock().await;
        let is_new = !sessions.contains_key(&key);
        let session = sessions.entry(key.clone()).or_insert_with(|| {
            let (device_name, has_custom) = match &custom_name {
                Some(name) if !name.is_empty() => (name.clone(), true),
                _ => (
                    if device_name.is_empty() {
                        DEFAULT_DEVICE_NAME.to_owned()
                    } else {
                        device_name.to_owned()
                    },
                    false,
                ),
            };
            SessionInfo {
                id: Self::session_id_for_key(&key),
                user_id,
                user_name: user_name.clone(),
                client: Some(app_name.to_owned()),
                device_id: device_id.to_owned(),
                device_name: Some(device_name),
                application_version: Some(app_version.to_owned()),
                remote_end_point: Some(remote_endpoint.to_owned()),
                server_id: Some(self.server_id.clone()),
                has_custom_device_name: has_custom,
                last_activity_date: now,
                last_playback_check_in: now,
                last_paused_date: None,
                now_playing_item_id: None,
                now_viewing_item_id: None,
                additional_users: Vec::new(),
                capabilities: ClientCapabilities::default(),
                transcoding_info: None,
                is_playing: false,
                connections: Vec::new(),
            }
        });

        // Refresh the mutable per-activity fields (C# `GetSessionInfo` tail).
        session.user_id = user_id;
        session.user_name = user_name;
        session.remote_end_point = Some(remote_endpoint.to_owned());
        session.client = Some(app_name.to_owned());
        session.application_version = Some(app_version.to_owned());
        session.last_activity_date = now;
        if !session.has_custom_device_name || session.device_name.is_none() {
            session.device_name = Some(if device_name.is_empty() {
                DEFAULT_DEVICE_NAME.to_owned()
            } else {
                device_name.to_owned()
            });
        }
        if user.is_none() {
            session.additional_users.clear();
        }

        let snapshot = session.clone();
        drop(sessions);

        if is_new {
            // C# `OnSessionStarted` publishes `SessionStarted`; consumers are
            // registered on the injected event manager.
            let _ = self
                .event_manager
                .publish("SessionStarted", &session_started_payload(&snapshot))
                .await;
        }

        Ok(snapshot)
    }

    /// Looks up a live session by its **session id** (not key), cloning a
    /// snapshot. C# `GetSession(sessionId)`.
    async fn get_session_snapshot(&self, session_id: &str) -> Result<SessionInfo, ServiceError> {
        let sessions = self.sessions.lock().await;
        sessions
            .values()
            .find(|s| s.id == session_id)
            .cloned()
            .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))
    }

    /// Resolves the primary + additional users of a session (C# `GetUsers`).
    async fn users_for(&self, session: &SessionInfo) -> Result<Vec<UserEntity>, ServiceError> {
        let mut users = Vec::new();
        if session.user_id.is_nil() {
            return Ok(users);
        }
        let primary = self
            .user_manager
            .get_user_by_id(session.user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found("user not found"))?;
        users.push(primary);
        for extra in &session.additional_users {
            if let Some(u) = self.user_manager.get_user_by_id(extra.user_id).await? {
                users.push(u);
            }
        }
        Ok(users)
    }

    /// Sends a pre-serialized message to a set of sessions, matching them with a
    /// predicate over a snapshot. Holds the session lock across the sends.
    async fn broadcast<P>(
        &self,
        message_type: SessionMessageType,
        data: &str,
        mut predicate: P,
    ) -> Result<(), ServiceError>
    where
        P: FnMut(&SessionInfo) -> bool,
    {
        let payload = envelope_bytes(message_type, data)?;
        let sessions = self.sessions.lock().await;
        for session in sessions.values().filter(|s| predicate(s)) {
            let mut delivered = false;
            for connection in &session.connections {
                if !connection.is_open() {
                    continue;
                }
                if let Err(err) = connection.send(&payload).await {
                    error!(session_id = %session.id, %err, "failed to push message to session");
                }
                delivered = true;
            }
            // No direct connection — deliver over the session bus (the HTTP
            // WebSocket handler's sink), so remote-control pushes reach clients
            // connected through `/socket`.
            if !delivered
                && let Some(bus) = &self.bus
                && let Ok(text) = std::str::from_utf8(&payload)
            {
                bus.send(&session.id, text.to_owned());
            }
        }
        Ok(())
    }

    /// Sends a message to one controllable session, enforcing the controller's
    /// permission when a controlling session is named (C# `AssertCanControl`).
    async fn send_to_controllable(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError> {
        // Ensure the target exists and supports remote control.
        let target = self.get_session_snapshot(session_id).await?;
        if !self.session_supports_remote_control(&target) {
            return Err(ServiceError::invalid_input(
                "session does not support remote control",
            ));
        }
        if !controlling_session_id.is_empty() {
            // The controlling session must exist; full permission arbitration
            // (own-vs-others) is deferred with the note below.
            let _controller = self.get_session_snapshot(controlling_session_id).await?;
        }
        self.broadcast(message_type, data, |s| s.id == session_id)
            .await
    }
}

#[async_trait]
impl SessionManager for HermitSessionManager {
    async fn log_session_activity(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: &UserEntity,
    ) -> Result<SessionInfoDto, ServiceError> {
        let session = self
            .upsert_session(
                app_name,
                app_version,
                device_id,
                device_name,
                remote_endpoint,
                Some(user),
            )
            .await?;
        Ok(self.to_dto(&session))
    }

    async fn update_device_name(
        &self,
        session_id: &str,
        reported_device_name: &str,
    ) -> Result<(), ServiceError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .values_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))?;
        session.device_name = Some(reported_device_name.to_owned());
        Ok(())
    }

    async fn on_playback_start(&self, info: &PlaybackStartInfo) -> Result<(), ServiceError> {
        let session_id = info
            .session_id
            .as_deref()
            .ok_or_else(|| ServiceError::invalid_input("sessionId is required"))?;
        let session = self.get_session_snapshot(session_id).await?;

        // Record the now-playing item + mark automatic progress started.
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.values_mut().find(|s| s.id == session_id) {
                s.now_playing_item_id = (!info.item_id.is_nil()).then_some(info.item_id);
                s.is_playing = true;
                s.last_playback_check_in = Utc::now();
            }
        }

        // Resolve the library item (best-effort) for the event payload.
        let item = if info.item_id.is_nil() {
            None
        } else {
            self.library_manager.get_item_by_id(info.item_id).await?
        };
        if item.is_some() {
            info!(
                user = ?session.user_name,
                client = ?session.client,
                "user started playback",
            );
        }

        let payload = playback_event_payload(&session, info.item_id, info.position_ticks);
        let _ = self.event_manager.publish("PlaybackStart", &payload).await;
        Ok(())
    }

    async fn on_playback_progress(
        &self,
        info: &PlaybackProgressInfo,
        _is_automated: bool,
    ) -> Result<(), ServiceError> {
        let session_id = info
            .session_id
            .as_deref()
            .ok_or_else(|| ServiceError::invalid_input("sessionId is required"))?;
        let session = self.get_session_snapshot(session_id).await?;

        {
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.values_mut().find(|s| s.id == session_id) {
                s.last_activity_date = Utc::now();
                s.last_playback_check_in = Utc::now();
                if info.is_paused {
                    s.last_paused_date = Some(Utc::now());
                } else {
                    s.last_paused_date = None;
                }
            }
        }

        // Persist play-state for every user on the session (C#
        // `OnPlaybackProgress(user, item, info)` → `UpdatePlayState`).
        if !info.item_id.is_nil() {
            for user in self.users_for(&session).await? {
                let user_id = parse_user_id(&user.id);
                self.user_data_manager
                    .update_play_state(user_id, info.item_id, info.position_ticks)
                    .await?;
            }
        }

        let payload = playback_event_payload(&session, info.item_id, info.position_ticks);
        let _ = self
            .event_manager
            .publish("PlaybackProgress", &payload)
            .await;
        Ok(())
    }

    async fn on_playback_stopped(&self, info: &PlaybackStopInfo) -> Result<(), ServiceError> {
        let session_id = info
            .session_id
            .as_deref()
            .ok_or_else(|| ServiceError::invalid_input("sessionId is required"))?;
        let session = self.get_session_snapshot(session_id).await?;

        {
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.values_mut().find(|s| s.id == session_id) {
                s.now_playing_item_id = None;
                s.is_playing = false;
                s.last_activity_date = Utc::now();
            }
        }

        // Final play-state update per user unless playback failed (C#
        // `OnPlaybackStopped(user, item, positionTicks, playbackFailed)`).
        if !info.item_id.is_nil() && !info.failed {
            for user in self.users_for(&session).await? {
                let user_id = parse_user_id(&user.id);
                self.user_data_manager
                    .update_play_state(user_id, info.item_id, info.position_ticks)
                    .await?;
            }
        }

        let payload = playback_event_payload(&session, info.item_id, info.position_ticks);
        let _ = self
            .event_manager
            .publish("PlaybackStopped", &payload)
            .await;
        Ok(())
    }

    async fn report_session_ended(&self, session_id: &str) -> Result<(), ServiceError> {
        let removed = {
            let mut sessions = self.sessions.lock().await;
            let key = sessions
                .iter()
                .find(|(_, s)| s.id == session_id)
                .map(|(k, _)| k.clone());
            key.and_then(|k| sessions.remove(&k))
        };
        if let Some(session) = removed {
            let _ = self
                .event_manager
                .publish("SessionEnded", &session_started_payload(&session))
                .await;
        }
        Ok(())
    }

    async fn send_general_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &GeneralCommand,
    ) -> Result<(), ServiceError> {
        let data = serde_json::to_string(command)
            .map_err(|e| ServiceError::backend(format!("serialize command: {e}")))?;
        self.send_to_controllable(
            controlling_session_id,
            session_id,
            SessionMessageType::GeneralCommand,
            &data,
        )
        .await
    }

    async fn send_message_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &MessageCommand,
    ) -> Result<(), ServiceError> {
        // C# builds a DisplayMessage GeneralCommand carrying the header/text.
        let mut arguments = std::collections::HashMap::new();
        if let Some(header) = &command.header {
            arguments.insert("Header".to_owned(), header.clone());
        }
        arguments.insert("Text".to_owned(), command.text.clone());
        if let Some(timeout) = command.timeout_ms {
            arguments.insert("TimeoutMs".to_owned(), timeout.to_string());
        }
        let general = GeneralCommand {
            name: GeneralCommandType::DisplayMessage,
            controlling_user_id: Uuid::nil(),
            arguments,
        };
        self.send_general_command(controlling_session_id, session_id, &general)
            .await
    }

    async fn send_play_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &PlayRequest,
    ) -> Result<(), ServiceError> {
        // Instant-mix expansion (C# `TranslateItemForInstantMix`) needs the
        // injected `IMusicManager`, which is not part of this unit — deferred.
        // The play request is forwarded verbatim as the pushed payload.
        let data = serde_json::to_string(command)
            .map_err(|e| ServiceError::backend(format!("serialize command: {e}")))?;
        self.send_to_controllable(
            controlling_session_id,
            session_id,
            SessionMessageType::Play,
            &data,
        )
        .await
    }

    async fn send_playstate_command(
        &self,
        controlling_session_id: &str,
        session_id: &str,
        command: &PlaystateRequest,
    ) -> Result<(), ServiceError> {
        let data = serde_json::to_string(command)
            .map_err(|e| ServiceError::backend(format!("serialize command: {e}")))?;
        self.send_to_controllable(
            controlling_session_id,
            session_id,
            SessionMessageType::Playstate,
            &data,
        )
        .await
    }

    async fn send_message_to_admin_sessions(
        &self,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError> {
        // Resolve the admin user ids, then broadcast to their sessions.
        let mut admin_ids = Vec::new();
        for user in self.user_manager.get_users().await? {
            if has_permission(self.db.pool(), &user.id, PermissionKind::IsAdministrator).await? {
                admin_ids.push(parse_user_id(&user.id));
            }
        }
        self.broadcast(message_type, data, |s| {
            admin_ids.iter().any(|id| s.contains_user(*id))
        })
        .await
    }

    async fn send_message_to_user_sessions(
        &self,
        user_ids: &[Uuid],
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError> {
        self.broadcast(message_type, data, |s| {
            user_ids.iter().any(|id| s.contains_user(*id))
        })
        .await
    }

    async fn send_message_to_user_device_sessions(
        &self,
        device_id: &str,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError> {
        self.broadcast(message_type, data, |s| {
            s.device_id.eq_ignore_ascii_case(device_id)
        })
        .await
    }

    async fn send_restart_required_notification(&self) -> Result<(), ServiceError> {
        self.broadcast(SessionMessageType::RestartRequired, "", |_| true)
            .await
    }

    async fn add_additional_user(
        &self,
        session_id: &str,
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        let user_name = self
            .user_manager
            .get_user_by_id(user_id)
            .await?
            .map(|u| u.username);
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .values_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))?;
        if session.user_id == user_id {
            return Err(ServiceError::invalid_input(
                "the requested user is already the primary user of the session",
            ));
        }
        if session
            .additional_users
            .iter()
            .all(|u| u.user_id != user_id)
        {
            session
                .additional_users
                .push(SessionUserInfo { user_id, user_name });
        }
        Ok(())
    }

    async fn remove_additional_user(
        &self,
        session_id: &str,
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .values_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))?;
        if session.user_id == user_id {
            return Err(ServiceError::invalid_input(
                "the requested user is already the primary user of the session",
            ));
        }
        session.additional_users.retain(|u| u.user_id != user_id);
        Ok(())
    }

    async fn report_now_viewing_item(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Result<(), ServiceError> {
        let parsed = Uuid::parse_str(item_id)
            .map_err(|_| ServiceError::invalid_input(format!("invalid item id {item_id}")))?;
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .values_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))?;
        session.now_viewing_item_id = Some(parsed);
        Ok(())
    }

    async fn authenticate_new_session(
        &self,
        request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        self.authenticate_internal(request, true).await
    }

    async fn authenticate_direct(
        &self,
        request: &AuthenticationRequest,
    ) -> Result<AuthenticationResultData, ServiceError> {
        self.authenticate_internal(request, false).await
    }

    async fn report_capabilities(
        &self,
        session_id: &str,
        capabilities: &ClientCapabilities,
    ) -> Result<(), ServiceError> {
        let device_id = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .values_mut()
                .find(|s| s.id == session_id)
                .ok_or_else(|| ServiceError::not_found(format!("session {session_id}")))?;
            session.capabilities = capabilities.clone();
            session.device_id.clone()
        };
        // Persist through the device manager (C# `_deviceManager.SaveCapabilities`).
        self.device_manager
            .save_capabilities(&device_id, capabilities)
            .await
    }

    async fn report_transcoding_info(
        &self,
        device_id: &str,
        info: &TranscodingInfo,
    ) -> Result<(), ServiceError> {
        let mut sessions = self.sessions.lock().await;
        for session in sessions
            .values_mut()
            .filter(|s| s.device_id.eq_ignore_ascii_case(device_id))
        {
            session.transcoding_info = Some(info.clone());
        }
        Ok(())
    }

    async fn clear_transcoding_info(&self, device_id: &str) -> Result<(), ServiceError> {
        let mut sessions = self.sessions.lock().await;
        for session in sessions
            .values_mut()
            .filter(|s| s.device_id.eq_ignore_ascii_case(device_id))
        {
            session.transcoding_info = None;
        }
        Ok(())
    }

    async fn get_sessions(
        &self,
        user_id: Uuid,
        device_id: Option<&str>,
        active_within_seconds: Option<i32>,
        controllable_user_to_check: Option<Uuid>,
        is_api_key: bool,
    ) -> Result<Vec<SessionInfoDto>, ServiceError> {
        // Resolve the caller's control permissions (C# `GetSessions` head).
        let (user_can_control_others, user_is_admin) = if is_api_key {
            (true, true)
        } else if user_id.is_nil() {
            (false, false)
        } else {
            match self.user_manager.get_user_by_id(user_id).await? {
                Some(user) => (
                    has_permission(
                        self.db.pool(),
                        &user.id,
                        PermissionKind::EnableRemoteControlOfOtherUsers,
                    )
                    .await?,
                    has_permission(self.db.pool(), &user.id, PermissionKind::IsAdministrator)
                        .await?,
                ),
                None => return Ok(Vec::new()),
            }
        };

        let min_active = active_within_seconds
            .filter(|s| *s > 0)
            .map(|s| Utc::now() - chrono::Duration::seconds(i64::from(s)));

        let sessions = self.sessions.lock().await;
        let mut result = Vec::new();
        for session in sessions.values() {
            if let Some(want) = device_id
                && !session.device_id.eq_ignore_ascii_case(want)
            {
                continue;
            }

            if controllable_user_to_check.is_some() {
                if !self.session_supports_remote_control(session) {
                    continue;
                }
                if !user_can_control_others
                    && !session.user_id.is_nil()
                    && !session.contains_user(user_id)
                {
                    continue;
                }
            } else if !user_is_admin && !session.user_id.is_nil() && !session.contains_user(user_id)
            {
                // Non-admin: limit to own sessions.
                continue;
            }

            if let Some(min) = min_active
                && session.last_activity_date < min
            {
                continue;
            }

            let mut dto = self.to_dto(session);
            if !user_is_admin {
                // Don't report hardware-acceleration detail to non-admins.
                dto.transcoding_info = None;
            }
            result.push(dto);
        }

        // Newest activity first (C# `OrderByDescending(LastActivityDate)`).
        result.sort_by_key(|s| std::cmp::Reverse(s.last_activity_date));
        Ok(result)
    }

    async fn get_session_by_authentication_token(
        &self,
        token: &str,
        device_id: &str,
        remote_endpoint: &str,
    ) -> Result<SessionInfoDto, ServiceError> {
        let device = self
            .device_by_access_token(token)
            .await?
            .ok_or_else(|| ServiceError::unauthorized("invalid access token"))?;

        let user = if device.user_id.is_empty() {
            None
        } else {
            self.user_manager
                .get_user_by_id(parse_user_id(&device.user_id))
                .await?
        };

        let effective_device_id = if device_id.is_empty() {
            device.device_id.as_str()
        } else {
            device_id
        };
        let app_version = if device.app_version.is_empty() {
            "1"
        } else {
            device.app_version.as_str()
        };

        let session = self
            .upsert_session(
                &device.app_name,
                app_version,
                effective_device_id,
                &device.device_name,
                remote_endpoint,
                user.as_ref(),
            )
            .await?;
        Ok(self.to_dto(&session))
    }

    async fn logout(&self, access_token: &str) -> Result<(), ServiceError> {
        if access_token.is_empty() {
            return Err(ServiceError::invalid_input("accessToken is required"));
        }
        if let Some(device) = self.device_by_access_token(access_token).await? {
            self.logout_device(&device).await?;
        }
        Ok(())
    }

    async fn logout_device(&self, device: &DeviceEntity) -> Result<(), ServiceError> {
        self.device_manager.delete_device(device).await?;
        // End every session bound to the device (C# `Logout(Device)`).
        let ended: Vec<String> = {
            let sessions = self.sessions.lock().await;
            sessions
                .values()
                .filter(|s| s.device_id.eq_ignore_ascii_case(&device.device_id))
                .map(|s| s.id.clone())
                .collect()
        };
        for id in ended {
            if let Err(err) = self.report_session_ended(&id).await {
                error!(session_id = %id, %err, "error reporting session ended");
            }
        }
        Ok(())
    }

    async fn revoke_user_tokens(
        &self,
        user_id: Uuid,
        current_access_token: &str,
    ) -> Result<(), ServiceError> {
        let devices = self
            .device_manager
            .get_devices(&DeviceQuery {
                user_id: Some(user_id),
                ..DeviceQuery::default()
            })
            .await?;
        for device in devices.items {
            if !device
                .access_token
                .eq_ignore_ascii_case(current_access_token)
            {
                self.logout_device(&device).await?;
            }
        }
        Ok(())
    }

    async fn close_live_stream_if_needed(
        &self,
        _live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        // Live-stream reference-counting needs the injected `IMediaSourceManager`
        // (not part of this unit). Deferred: with no live streams opened here,
        // there is nothing to close. See the module docs.
        Ok(())
    }
}

impl HermitSessionManager {
    /// Shared authenticate path (C# `AuthenticateNewSessionInternal`).
    async fn authenticate_internal(
        &self,
        request: &AuthenticationRequest,
        enforce_password: bool,
    ) -> Result<AuthenticationResultData, ServiceError> {
        let app = require(request.app.as_deref(), "app")?;
        let device_id = require(request.device_id.as_deref(), "deviceId")?;
        let device_name = require(request.device_name.as_deref(), "deviceName")?;
        let app_version = require(request.app_version.as_deref(), "appVersion")?;
        let remote_endpoint = request.remote_endpoint.as_deref().unwrap_or("");

        // Resolve the user by id, then by name, then (when enforcing) by password.
        let mut user = if let Some(id) = request.user_id {
            self.user_manager.get_user_by_id(id).await?
        } else {
            None
        };
        if let (None, Some(name)) = (&user, request.username.as_deref()) {
            user = self.user_manager.get_user_by_name(name).await?;
        }
        if enforce_password {
            user = self
                .user_manager
                .authenticate_user(
                    request.username.as_deref().unwrap_or(""),
                    request.password.as_deref().unwrap_or(""),
                    remote_endpoint,
                    true,
                )
                .await?;
        }
        let user =
            user.ok_or_else(|| ServiceError::unauthorized("invalid username or password entered"))?;

        if !self
            .device_manager
            .can_access_device(&user, device_id)
            .await?
        {
            return Err(ServiceError::unauthorized(
                "user is not allowed access from this device",
            ));
        }

        // Enforce the per-user max active sessions (C# check).
        let user_uuid = parse_user_id(&user.id);
        if user.max_active_sessions >= 1 {
            let active = {
                let sessions = self.sessions.lock().await;
                sessions.values().filter(|s| s.user_id == user_uuid).count()
            };
            if i64::try_from(active).unwrap_or(i64::MAX) >= user.max_active_sessions {
                return Err(ServiceError::unauthorized(
                    "user is at their maximum number of sessions",
                ));
            }
        }

        // Mint a fresh access token: log out existing device rows for this
        // user/device, then create a new device (C# `GetAuthorizationToken`).
        let existing = self
            .device_manager
            .get_devices(&DeviceQuery {
                device_id: Some(device_id.to_owned()),
                user_id: Some(user_uuid),
                ..DeviceQuery::default()
            })
            .await?;
        for device in existing.items {
            if let Err(err) = self.logout_device(&device).await {
                error!(%err, "error logging out existing session");
            }
        }
        let new_device = DeviceEntity {
            id: 0,
            access_token: Uuid::new_v4().simple().to_string(),
            app_name: app.to_owned(),
            app_version: app_version.to_owned(),
            date_created: Utc::now(),
            date_last_activity: Utc::now(),
            date_modified: Utc::now(),
            device_id: device_id.to_owned(),
            device_name: device_name.to_owned(),
            is_active: true,
            user_id: user.id.clone(),
        };
        let created = self.device_manager.create_device(&new_device).await?;

        let session = self
            .upsert_session(
                app,
                app_version,
                device_id,
                device_name,
                remote_endpoint,
                Some(&user),
            )
            .await?;

        let mut dto = self.to_dto(&session);
        dto.server_id = Some(self.server_id.clone());
        // The full `AuthenticationResult` envelope (UserDto + ServerId) is
        // assembled by the caller from this data; the freshly minted token
        // (persisted on the created `Device` row) is returned so the caller can
        // echo it back to the client.
        let access_token = created.access_token;

        let _ = self
            .event_manager
            .publish("AuthenticationSucceeded", &session.id)
            .await;
        Ok(AuthenticationResultData {
            session: dto,
            access_token,
        })
    }

    /// Resolves the device row bearing a session access token.
    ///
    /// The injected [`DeviceManager`] trait's [`DeviceQuery`] carries no token
    /// filter (it exposes only device-id/user-id), so this reads the `Devices`
    /// table directly through the `db` handle — the same escape hatch the rest
    /// of the crate uses for reads the trait surface omits. Returns `None` when
    /// no device matches.
    async fn device_by_access_token(
        &self,
        token: &str,
    ) -> Result<Option<DeviceEntity>, ServiceError> {
        sqlx::query_as::<_, DeviceEntity>(
            r#"SELECT * FROM "Devices" WHERE "AccessToken" = ?1 ORDER BY "Id" LIMIT 1"#,
        )
        .bind(token)
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)
    }
}

/// Requires an optional string argument to be present and non-empty.
fn require<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, ServiceError> {
    match value {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(ServiceError::invalid_input(format!("{name} is required"))),
    }
}

/// Parses the hyphenated `UserEntity::id` string into a [`Uuid`], defaulting to
/// the nil uuid on a malformed value (matching the C# `Guid.Empty` fallback).
fn parse_user_id(id: &str) -> Uuid {
    Uuid::parse_str(id).unwrap_or_else(|_| Uuid::nil())
}

/// Maps the in-memory session state to the wire [`SessionInfoDto`]
/// (C# `ToSessionInfoDto`).
fn session_info_to_dto(session: &SessionInfo) -> SessionInfoDto {
    SessionInfoDto {
        additional_users: Some(session.additional_users.clone()),
        remote_end_point: session.remote_end_point.clone(),
        playable_media_types: session.capabilities.playable_media_types.clone(),
        id: Some(session.id.clone()),
        user_id: session.user_id,
        user_name: session.user_name.clone(),
        client: session.client.clone(),
        last_activity_date: session.last_activity_date,
        last_playback_check_in: session.last_playback_check_in,
        last_paused_date: session.last_paused_date,
        device_name: session.device_name.clone(),
        device_id: Some(session.device_id.clone()),
        application_version: session.application_version.clone(),
        transcoding_info: session.transcoding_info.clone(),
        is_active: session.is_active(),
        supports_media_control: session.capabilities.supports_media_control,
        supports_remote_control: session.supports_remote_control(),
        has_custom_device_name: session.has_custom_device_name,
        server_id: session.server_id.clone(),
        supported_commands: session.capabilities.supported_commands.clone(),
        // Jellyfin always emits these: a Capabilities object, a default PlayState, and the two
        // (empty) now-playing queues.
        capabilities: Some(crate::device_manager::client_capabilities_to_dto(
            &session.capabilities,
        )),
        play_state: Some(hermit_model::session::PlayerStateInfo::default()),
        now_playing_queue: Some(Vec::new()),
        now_playing_queue_full_items: Some(Vec::new()),
        ..SessionInfoDto::default()
    }
}

/// Serializes the `{ MessageType, MessageId, Data }` envelope to bytes for a
/// WebSocket push. `data` is embedded as raw JSON when it parses as such, else
/// as a JSON string (so an empty `data` becomes `""`).
fn envelope_bytes(message_type: SessionMessageType, data: &str) -> Result<Vec<u8>, ServiceError> {
    let data_value = if data.is_empty() {
        serde_json::Value::String(String::new())
    } else {
        serde_json::from_str(data).unwrap_or_else(|_| serde_json::Value::String(data.to_owned()))
    };
    let envelope = OutboundMessage {
        message_type,
        message_id: Uuid::new_v4().simple().to_string(),
        data: data_value,
        _marker: std::marker::PhantomData,
    };
    serde_json::to_vec(&envelope)
        .map_err(|e| ServiceError::backend(format!("serialize message: {e}")))
}

/// The JSON payload for a session-lifecycle event (a minimal snapshot).
fn session_started_payload(session: &SessionInfo) -> String {
    serde_json::to_string(&session_info_to_dto(session)).unwrap_or_else(|_| "{}".to_owned())
}

/// The JSON payload for a playback-progress event.
fn playback_event_payload(
    session: &SessionInfo,
    item_id: Uuid,
    position_ticks: Option<i64>,
) -> String {
    serde_json::json!({
        "SessionId": session.id,
        "UserId": session.user_id,
        "ItemId": item_id,
        "PositionTicks": position_ticks,
    })
    .to_string()
}

#[cfg(test)]
mod tests;
