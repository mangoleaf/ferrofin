//! [`FerrofinSessionManager`] — the concrete [`SessionManager`] over injected
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
//! - **The domain `SessionInfo` is modelled here**, not in `ferrofin-traits`
//!   (whose reads surface the [`SessionInfoDto`] wire type). It is a private
//!   in-memory struct; [`session_info_to_dto`] maps it to the DTO on read.
//! - **Message broadcast collapses to pre-serialized JSON** (the trait takes a
//!   `&str` payload). Each session holds its live
//!   `Arc<dyn `[`WebSocketConnection`]`>` controllers; a broadcast serializes a
//!   small `{ MessageType, Data }` envelope once and `send`s the bytes to every
//!   controller. The `SessionControllers`/`WebSocketController` indirection of
//!   the C# collapses into "the session's connection handles".
//! - **`AuthenticationResult`** now lands in `ferrofin-model`; the authenticate
//!   methods return an [`AuthenticationResultData`] (the [`SessionInfoDto`] plus
//!   the minted access token) from which the API layer assembles the wire
//!   envelope. The access token is minted via [`DeviceManager`].
//! - **Idle/inactive timers and `IAsyncDisposable`** are dropped — no real
//!   scheduler in this crate (that is Wave 8 / scheduled tasks). Automatic
//!   progress is tracked as a flag on the in-memory session.
//! - **The session pool is evicted exactly where upstream evicts it**, never on
//!   a timer: [`SessionManager::report_session_ended`] (`/Sessions/Logout`, and
//!   the C# `WebSocketController.OnConnectionClosed` -> `CloseIfNeededAsync`
//!   path the API layer's `/socket` handler calls when a session's last socket
//!   closes) and [`SessionManager::logout`]. A client that only ever speaks
//!   HTTP therefore keeps its entry until the server restarts — same as
//!   upstream, where `/Sessions` filters the stale ones by `activeWithinSeconds`
//!   rather than reaping them.
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

use ferrofin_db::Database;
use ferrofin_db::entities::security::DeviceEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::enums::PermissionKind;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{SessionInfoDto, SortOrder};
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::secret::Secret;
use ferrofin_model::session::{
    ClientCapabilities, GeneralCommand, GeneralCommandType, MessageCommand, PlayCommand,
    PlayRequest, PlaybackProgressInfo, PlaybackStartInfo, PlaybackStopInfo, PlaystateRequest,
    SessionMessageType, SessionUserInfo, TranscodingInfo, UserDataChangeInfo,
};
use ferrofin_util::shuffle_extensions::shuffle;

use ferrofin_traits::devices::{DeviceManager, DeviceQuery};
use ferrofin_traits::dto::DtoService;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::EventManager;
use ferrofin_traits::library::{
    LibraryManager, MediaSourceManager, MusicManager, UserDataManager, UserManager,
};
use ferrofin_traits::net::WebSocketConnection;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData, SessionManager};
use ferrofin_traits::session_bus::SessionMessageBus;

use crate::db_error::db_err;
use crate::user_entity_ext::has_permission;

/// The default device name applied when a client reports an empty one (C#
/// `CreateSessionInfo` → `"Network Device"`).
const DEFAULT_DEVICE_NAME: &str = "Network Device";

/// The in-memory session-state object the C# `SessionInfo` domain class becomes.
///
/// Deliberately **not** in `ferrofin-traits` (its reads surface the wire
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
    /// The current playback position + paused flag reported for the now-playing item,
    /// surfaced as the session's `PlayState` (C# `Session.PlayState`). Cleared on stop.
    now_playing_position_ticks: Option<i64>,
    now_playing_is_paused: bool,
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
/// async [`Mutex`] so the managers' `async fn`s can take it without blocking a
/// runtime worker; it is **never** held across an `.await` on a client socket —
/// [`FerrofinSessionManager::broadcast`] snapshots its targets and releases the
/// guard before sending, so one stalled WebSocket client cannot stall session
/// state for everyone else.
#[derive(Clone)]
pub struct FerrofinSessionManager {
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
    /// do not surface (the same escape hatch `FerrofinDtoService` uses). Reads
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
    /// The media-source manager, when wired — the owner of the open-live-stream
    /// table. `OnPlaybackStopped` closes the reported live stream through it
    /// (C# `CloseLiveStreamIfNeededAsync`); without it that close is a no-op and
    /// an abandoned live stream stays open until the client asks explicitly.
    media_sources: Option<Arc<dyn MediaSourceManager>>,
    /// The music manager, when wired — used only by `SendPlayCommand`'s
    /// `PlayInstantMix` translation (C# `TranslateItemForInstantMix`). Without
    /// it a cast instant-mix falls back to playing the seed item itself.
    music_manager: Option<Arc<dyn MusicManager>>,
}

impl std::fmt::Debug for FerrofinSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSessionManager")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl FerrofinSessionManager {
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
            media_sources: None,
            music_manager: None,
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

    /// Wires the media-source manager (composition root only), so a
    /// playback-stopped report closes the live stream it names — the C#
    /// `OnPlaybackStopped` -> `CloseLiveStreamIfNeededAsync` path. Without it
    /// live streams are only ever closed by an explicit `/LiveStreams/Close`.
    #[must_use]
    pub fn with_media_sources(mut self, media_sources: Arc<dyn MediaSourceManager>) -> Self {
        self.media_sources = Some(media_sources);
        self
    }

    /// Wires the music manager (composition root only), so casting an instant
    /// mix to a device expands the seed item into the mix — the C#
    /// `SendPlayCommand` -> `TranslateItemForInstantMix` path. Without it a
    /// `PlayInstantMix` cast degrades to playing the seed item alone.
    #[must_use]
    pub fn with_music_manager(mut self, music_manager: Arc<dyn MusicManager>) -> Self {
        self.music_manager = Some(music_manager);
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
    /// This is the seam the [`WebSocketListener`](ferrofin_traits::net::WebSocketListener)
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
    /// [`get_md5`](ferrofin_common::extensions::get_md5) reproduces the .NET `Guid`;
    /// `simple()` renders it as 32 lowercase hex digits without dashes.
    fn session_id_for_key(key: &str) -> String {
        ferrofin_common::extensions::get_md5(key)
            .simple()
            .to_string()
    }

    /// Records session activity, creating the session on first contact
    /// (C# `LogSessionActivity` → `GetSessionInfo`/`CreateSessionInfo`).
    ///
    /// Returns the session's wire [`SessionInfoDto`] rather than a clone of the
    /// in-memory [`SessionInfo`]: this runs on every authenticated request and
    /// every websocket token resolve, and the DTO (plus, on first contact, the
    /// `SessionStarted` payload) is all any caller wants — cloning the whole
    /// session (additional users, capabilities, connection handles, ~8 options)
    /// only to project it was pure per-request garbage.
    async fn upsert_session(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: Option<&UserEntity>,
    ) -> Result<SessionInfoDto, ServiceError> {
        if device_id.is_empty() {
            return Err(ServiceError::invalid_input("deviceId is required"));
        }

        let key = Self::session_key(app_name, device_id);
        let now = Utc::now();
        let user_id = match user {
            // C# `SessionInfo.UserId` is `Guid.Empty` when no user is signed in.
            None => Uuid::nil(),
            Some(u) => parse_user_id(&u.id)?,
        };
        let user_name = user.map(|u| u.username.clone());

        // The custom device name, if the device has one persisted.
        let custom_name = self
            .device_manager
            .get_device_options(device_id)
            .await?
            .and_then(|o| o.custom_name);

        // A newly created session inherits the device's last reported
        // capabilities (C# `OnSessionStarted` -> `ReportCapabilities(info, caps,
        // saveCapabilities: false)`). That is what makes the device manager's
        // never-evicted capabilities map load-bearing: a session recreated after
        // its socket closed still advertises `SupportsRemoteControl`, so the
        // client does not vanish from the cast menu until it re-posts
        // `/Sessions/Capabilities/Full`.
        let device_capabilities = self
            .device_manager
            .get_capabilities(Some(device_id))
            .await?;

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
                now_playing_position_ticks: None,
                now_playing_is_paused: false,
                now_viewing_item_id: None,
                additional_users: Vec::new(),
                capabilities: device_capabilities,
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

        // Project everything the callers need while the guard is still held, so
        // no clone of the session itself escapes.
        let dto = self.to_dto(session);
        let started_payload = is_new.then(|| session_started_payload(session));
        drop(sessions);

        if let Some(payload) = started_payload {
            // C# `OnSessionStarted` publishes `SessionStarted`; consumers are
            // registered on the injected event manager.
            let _ = self.event_manager.publish("SessionStarted", &payload).await;
        }

        Ok(dto)
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
    /// predicate over a snapshot.
    ///
    /// The session lock is **never held across a send**: the matching sessions'
    /// ids and open connection handles are snapshotted under the lock, the guard
    /// is dropped, and only then are the frames pushed. Holding it across the
    /// `.await` used to serialize every session operation (playback reports,
    /// `upsert_session`, capability reports, websocket attach) behind the
    /// slowest WebSocket client — one stalled socket blocked all session state.
    ///
    /// Delivery semantics are unchanged: the same sessions receive, in the same
    /// order, and a session with no open direct connection still falls back to
    /// the session bus.
    async fn broadcast<P>(
        &self,
        message_type: SessionMessageType,
        data: &str,
        mut predicate: P,
    ) -> Result<(), ServiceError>
    where
        P: FnMut(&SessionInfo) -> bool,
    {
        // (session id, its open connections) for every matching session that can
        // actually receive, in the pool's iteration order — captured under the
        // lock, sent without it. A matching session with neither an open direct
        // connection nor a registered bus sink is dropped here: `bus.send` would
        // miss the map and return `false`, so skipping it delivers the same
        // messages to the same clients while costing nothing. That matters
        // because the session pool holds every device that ever authenticated,
        // live or not.
        let targets: Vec<(String, Vec<Arc<dyn WebSocketConnection>>)> = {
            let sessions = self.sessions.lock().await;
            sessions
                .values()
                .filter(|s| predicate(s))
                .filter_map(|s| {
                    let open: Vec<Arc<dyn WebSocketConnection>> = s
                        .connections
                        .iter()
                        .filter(|c| c.is_open())
                        .map(Arc::clone)
                        .collect();
                    if !open.is_empty() {
                        return Some((s.id.clone(), open));
                    }
                    self.bus
                        .as_ref()
                        .is_some_and(|bus| bus.is_connected(&s.id))
                        .then(|| (s.id.clone(), Vec::new()))
                })
                .collect()
        };
        if targets.is_empty() {
            // Nobody can receive it — don't pay to build a message that would be
            // dropped (Jellyfin's `SendMessageToUserSessions(…, Func<T> dataFn,
            // …)` overload exists for exactly this).
            return Ok(());
        }

        let payload = envelope_bytes(message_type, data)?;
        // Validate once, not once per recipient: the envelope is the same bytes
        // for everyone.
        let text = std::str::from_utf8(&payload).ok();

        for (session_id, connections) in targets {
            if connections.is_empty() {
                // No direct connection — deliver over the session bus (the HTTP
                // WebSocket handler's sink), so remote-control pushes reach
                // clients connected through `/socket`.
                if let (Some(bus), Some(text)) = (&self.bus, text) {
                    bus.send(&session_id, text.to_owned());
                }
                continue;
            }
            for connection in &connections {
                if let Err(err) = connection.send(&payload).await {
                    error!(session_id = %session_id, %err, "failed to push message to session");
                }
            }
        }
        Ok(())
    }

    /// Whether any of `user_id`'s sessions could actually receive a pushed
    /// message: an open WebSocket connection on the session, or a sink
    /// registered on the session bus for its id (the `/socket` handler's path).
    /// Exactly the two delivery routes [`Self::broadcast`] uses, so a `false`
    /// here means the broadcast would reach nobody.
    async fn has_listener_for_user(&self, user_id: Uuid) -> bool {
        let bus_candidates: Vec<String> = {
            let sessions = self.sessions.lock().await;
            let mut ids = Vec::new();
            for session in sessions.values().filter(|s| s.contains_user(user_id)) {
                if session.connections.iter().any(|c| c.is_open()) {
                    return true;
                }
                ids.push(session.id.clone());
            }
            ids
        };
        self.bus.as_ref().is_some_and(|bus| {
            bus_candidates
                .iter()
                .any(|id| bus.is_connected(id.as_str()))
        })
    }

    /// Pushes the user's refreshed play-state for `item_id` to every session
    /// belonging to that user (`UserDataChanged`), so their other signed-in
    /// devices update resume position / played flags live instead of showing
    /// stale progress until re-login. Port of Jellyfin's
    /// `UserDataChangedNotifier` (which fires on every user-data save).
    /// Best-effort: a delivery failure must not fail the playback report.
    async fn push_user_data_changed(&self, user_id: Uuid, item_id: Uuid) {
        // Building the payload costs a `UserData` read, and every play-state
        // report triggers one. When the user has no session that could receive
        // the push — no open WebSocket and no registered bus sink — `broadcast`
        // would drop it, so skip the read instead of paying for a message
        // nobody is listening for. (A session with either delivery path still
        // gets the push, unchanged.)
        if !self.has_listener_for_user(user_id).await {
            return;
        }
        let Ok(Some(dto)) = self
            .user_data_manager
            .get_user_data_dto(item_id, user_id)
            .await
        else {
            return;
        };
        let info = UserDataChangeInfo {
            user_id,
            user_data_list: vec![dto],
        };
        let Ok(data) = serde_json::to_string(&info) else {
            return;
        };
        let _ = self
            .broadcast(SessionMessageType::UserDataChanged, &data, |s| {
                s.contains_user(user_id)
            })
            .await;
    }

    /// Expands one cast item id into the ids a client can actually play — port
    /// of C# `TranslateItemForPlayback`.
    ///
    /// A folder (series, season, album, box set, playlist, …) becomes its
    /// recursive non-folder, non-virtual children; an "item by name" (genre,
    /// studio, person, year, artist) becomes the items tagged with it. Anything
    /// else is itself. Both expansions sort by `SortName`, matching upstream, so
    /// the receiving client gets the queue in playing order.
    ///
    /// An id that resolves to nothing contributes nothing (C# logs and returns
    /// an empty array rather than failing the whole command).
    async fn translate_item_for_playback(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(id).await? else {
            error!(item_id = %id, "nonexistent item id passed to play translation");
            return Ok(Vec::new());
        };
        let kind = crate::item_type_lookup::kind_from_type_name(&item.type_);

        // The persisted `IsFolder` column is authoritative for a specific row;
        // the kind is the class-level default (see `kinds::is_folder`).
        let by_name = kind.is_some_and(crate::kinds::is_item_by_name);
        if !item.is_folder && !by_name {
            return Ok(vec![id]);
        }

        let mut query = InternalItemsQuery {
            recursive: true,
            is_folder: Some(false),
            is_virtual_item: Some(false),
            user: user.cloned(),
            order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
            ..InternalItemsQuery::default()
        };
        if by_name {
            // `IItemByName.GetTaggedItems` — the filter field differs per kind.
            match kind {
                Some(BaseItemKind::Genre | BaseItemKind::MusicGenre) => query.genre_ids = vec![id],
                Some(BaseItemKind::Studio) => query.studio_ids = vec![id],
                Some(BaseItemKind::Person) => query.person_ids = vec![id],
                Some(BaseItemKind::MusicArtist) => query.artist_ids = vec![id],
                Some(BaseItemKind::Year) => {
                    let Some(year) = item.production_year.and_then(|y| i32::try_from(y).ok())
                    else {
                        return Ok(Vec::new());
                    };
                    query.years = vec![year];
                }
                // Not a by-name kind after all — fall back to the folder path.
                _ => query.ancestor_ids = vec![id],
            }
        } else {
            query.ancestor_ids = vec![id];
        }
        self.library_manager.get_item_ids(&query).await
    }

    /// Expands a cast instant-mix seed into the mix — port of C#
    /// `TranslateItemForInstantMix`. Without a wired [`MusicManager`] the seed
    /// item is played on its own rather than the command failing.
    async fn translate_item_for_instant_mix(
        &self,
        id: Uuid,
        user_id: Option<Uuid>,
    ) -> Result<Vec<Uuid>, ServiceError> {
        let Some(music) = self.music_manager.as_ref() else {
            error!(item_id = %id, "no music manager wired — instant-mix cast plays the seed item");
            return Ok(vec![id]);
        };
        let mix = music
            .get_instant_mix_from_item(id, user_id, &DtoOptions::default())
            .await?;
        Ok(mix
            .iter()
            .filter_map(|item| Uuid::parse_str(&item.id).ok())
            .collect())
    }

    /// Rewrites a cast [`PlayRequest`] the way C# `SendPlayCommand` does before
    /// it reaches the wire: expand the item ids, resolve `PlayInstantMix` /
    /// `PlayShuffle` into a concrete `PlayNow` list, extend a lone episode into
    /// the rest of its series when the target user auto-plays next episodes,
    /// and stamp the controlling user.
    async fn translate_play_request(
        &self,
        controlling_session_id: &str,
        target: &SessionInfo,
        command: &PlayRequest,
    ) -> Result<PlayRequest, ServiceError> {
        let user = if target.user_id.is_nil() {
            None
        } else {
            self.user_manager.get_user_by_id(target.user_id).await?
        };
        let user_id = user.as_ref().map(|_| target.user_id);

        let mut translated = command.clone();
        let mut items: Vec<Uuid> = Vec::new();
        if command.play_command == PlayCommand::PlayInstantMix {
            for id in &command.item_ids {
                items.extend(self.translate_item_for_instant_mix(*id, user_id).await?);
            }
            translated.play_command = PlayCommand::PlayNow;
        } else {
            for id in &command.item_ids {
                items.extend(self.translate_item_for_playback(*id, user.as_ref()).await?);
            }
        }

        if command.play_command == PlayCommand::PlayShuffle {
            shuffle(&mut items);
            translated.play_command = PlayCommand::PlayNow;
        }
        translated.item_ids = items;

        // C# `GetPlayAccess` is the `EnableMediaPlayback` permission and nothing
        // item-specific, so one check covers the whole list.
        if let Some(user) = user.as_ref()
            && !translated.item_ids.is_empty()
            && !has_permission(
                self.db.pool(),
                &user.id,
                PermissionKind::EnableMediaPlayback,
            )
            .await?
        {
            return Err(ServiceError::invalid_input(format!(
                "{} is not allowed to play media.",
                user.username
            )));
        }

        if let Some(user) = user.as_ref()
            && user.enable_next_episode_auto_play
            && translated.item_ids.len() == 1
            && let Some(rest) = self.episodes_from(translated.item_ids[0], user).await?
        {
            translated.item_ids = rest;
        }

        if !controlling_session_id.is_empty()
            && let Ok(controller) = self.get_session_snapshot(controlling_session_id).await
            && !controller.user_id.is_nil()
        {
            translated.controlling_user_id = controller.user_id;
        }
        Ok(translated)
    }

    /// The episodes of `episode_id`'s series from that episode onward, or `None`
    /// when the id is not an episode with a series (C# `SendPlayCommand`'s
    /// `EnableNextEpisodeAutoPlay` branch, which skips to the requested episode
    /// and takes the remainder).
    async fn episodes_from(
        &self,
        episode_id: Uuid,
        user: &UserEntity,
    ) -> Result<Option<Vec<Uuid>>, ServiceError> {
        let Some(item) = self.library_manager.get_item_by_id(episode_id).await? else {
            return Ok(None);
        };
        if crate::item_type_lookup::kind_from_type_name(&item.type_) != Some(BaseItemKind::Episode)
        {
            return Ok(None);
        }
        let Some(series_id) = item
            .series_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            return Ok(None);
        };
        let episodes = self
            .library_manager
            .get_item_ids(&InternalItemsQuery {
                recursive: true,
                is_folder: Some(false),
                is_virtual_item: Some(false),
                user: Some(user.clone()),
                ancestor_ids: vec![series_id],
                include_item_types: vec![BaseItemKind::Episode],
                order_by: vec![(ItemSortBy::SortName, SortOrder::Ascending)],
                ..InternalItemsQuery::default()
            })
            .await?;
        let Some(start) = episodes.iter().position(|id| *id == episode_id) else {
            return Ok(None);
        };
        let rest = episodes[start..].to_vec();
        Ok((rest.len() > 1).then_some(rest))
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
        // Ensure the target session exists (C# `GetSessionToRemoteControl` throws
        // `ResourceNotFoundException` → 404 when it doesn't). Jellyfin does NOT gate
        // command *delivery* on remote-control support — it hands the message to
        // whatever controllers the session has (none → a no-op) and returns 204 — so
        // neither do we; the guard here previously rejected controller-less sessions
        // that Jellyfin accepts.
        let _target = self.get_session_snapshot(session_id).await?;
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
impl SessionManager for FerrofinSessionManager {
    async fn log_session_activity(
        &self,
        app_name: &str,
        app_version: &str,
        device_id: &str,
        device_name: &str,
        remote_endpoint: &str,
        user: &UserEntity,
    ) -> Result<SessionInfoDto, ServiceError> {
        self.upsert_session(
            app_name,
            app_version,
            device_id,
            device_name,
            remote_endpoint,
            Some(user),
        )
        .await
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
                s.now_playing_position_ticks = info.position_ticks;
                s.now_playing_is_paused = info.is_paused;
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

        // Record the start on every session user's data (C# OnPlaybackStart:
        // PlayCount++ and the LastPlayedDate stamp Next Up filters on), then
        // push the change to the user's other devices.
        if !info.item_id.is_nil() {
            for user in self.users_for(&session).await? {
                let user_id = parse_user_id(&user.id)?;
                self.user_data_manager
                    .record_playback_start(user_id, info.item_id)
                    .await?;
                self.push_user_data_changed(user_id, info.item_id).await;
            }
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
                s.now_playing_position_ticks = info.position_ticks;
                s.now_playing_is_paused = info.is_paused;
                if info.is_paused {
                    s.last_paused_date = Some(Utc::now());
                } else {
                    s.last_paused_date = None;
                }
            }
        }

        // Persist play-state for every user on the session (C#
        // `OnPlaybackProgress(user, item, info)` → `UpdatePlayState`), then
        // push the change to the user's other devices.
        if !info.item_id.is_nil() {
            for user in self.users_for(&session).await? {
                let user_id = parse_user_id(&user.id)?;
                self.user_data_manager
                    .update_play_state(user_id, info.item_id, info.position_ticks)
                    .await?;
                self.push_user_data_changed(user_id, info.item_id).await;
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
                s.now_playing_position_ticks = None;
                s.now_playing_is_paused = false;
                s.is_playing = false;
                s.last_activity_date = Utc::now();
            }
        }

        // Final play-state update per user unless playback failed (C#
        // `OnPlaybackStopped(user, item, positionTicks, playbackFailed)`),
        // then push the change to the user's other devices.
        if !info.item_id.is_nil() && !info.failed {
            for user in self.users_for(&session).await? {
                let user_id = parse_user_id(&user.id)?;
                self.user_data_manager
                    .update_play_state(user_id, info.item_id, info.position_ticks)
                    .await?;
                self.push_user_data_changed(user_id, info.item_id).await;
            }
        }

        // C# `OnPlaybackStopped`: the live stream the client was playing is
        // closed here, not only by an explicit `/LiveStreams/Close`. A client that
        // stops playback and never calls Close otherwise leaks its open stream.
        if let Some(live_stream_id) = info.live_stream_id.as_deref() {
            self.close_live_stream_if_needed(live_stream_id, session_id)
                .await?;
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
        // C# `SendPlayCommand` rewrites the request before it reaches the wire —
        // clients receive a concrete `PlayNow`/`PlayNext`/`PlayLast` over
        // playable ids, never a container id or an unresolved
        // `PlayShuffle`/`PlayInstantMix`.
        let target = self.get_session_snapshot(session_id).await?;
        let translated = self
            .translate_play_request(controlling_session_id, &target, command)
            .await?;
        let data = serde_json::to_string(&translated)
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
        // C# stamps the controlling user (as a dashless "N"-format guid) so the
        // target can attribute the command.
        let mut command = command.clone();
        if !controlling_session_id.is_empty()
            && let Ok(controller) = self.get_session_snapshot(controlling_session_id).await
            && !controller.user_id.is_nil()
        {
            command.controlling_user_id = Some(controller.user_id.simple().to_string());
        }
        let data = serde_json::to_string(&command)
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
                admin_ids.push(parse_user_id(&user.id)?);
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

    async fn send_message_to_all_sessions(
        &self,
        message_type: SessionMessageType,
        data: &str,
    ) -> Result<(), ServiceError> {
        // Every session with a signed-in user (anonymous sockets have nothing
        // to refresh) — the target set of C# `SendMessageToSessions(Sessions, …)`
        // as the library/server notifiers use it.
        self.broadcast(message_type, data, |s| !s.user_id.is_nil())
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
            result.push((dto, session.now_playing_item_id));
        }
        drop(sessions); // release before the async NowPlayingItem enrichment below

        // Enrich NowPlayingItem: C# `UpdateNowPlayingItem` builds a BaseItemDto for the
        // session's current item. Done after dropping the sessions lock (async DTO build).
        let mut dtos = Vec::with_capacity(result.len());
        for (mut dto, now_playing_id) in result {
            if let Some(id) = now_playing_id
                && let Some(item) = self.library_manager.get_item_by_id(id).await?
                && let Ok(mut built) = self
                    .dto_service
                    .get_base_item_dtos(
                        std::slice::from_ref(&item),
                        &ferrofin_traits::options::DtoOptions::default(),
                        None,
                        None,
                        true,
                    )
                    .await
            {
                dto.now_playing_item = built.pop();
            }
            dtos.push(dto);
        }

        // Newest activity first (C# `OrderByDescending(LastActivityDate)`).
        dtos.sort_by_key(|s| std::cmp::Reverse(s.last_activity_date));
        Ok(dtos)
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
                .get_user_by_id(parse_user_id(&device.user_id)?)
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

        self.upsert_session(
            &device.app_name,
            app_version,
            effective_device_id,
            &device.device_name,
            remote_endpoint,
            user.as_ref(),
        )
        .await
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
        live_stream_id: &str,
        _session_or_play_session_id: &str,
    ) -> Result<(), ServiceError> {
        // C# keeps a `_activeLiveStreamSessions` map so a live stream shared by
        // several sessions is closed only by the last one; when a live stream has
        // no mapping it closes outright. Ferrofin mints a fresh live-stream id per
        // `OpenLiveStream`, so no two sessions ever name the same id and the
        // no-mapping branch is the only reachable one — the refcount map (itself
        // unbounded) buys nothing here.
        if live_stream_id.is_empty() {
            return Ok(());
        }
        let Some(media_sources) = self.media_sources.as_ref() else {
            return Ok(());
        };
        // C# logs and swallows: a failed close must not fail the client's
        // playback-stopped report.
        if let Err(err) = media_sources.close_live_stream(live_stream_id).await {
            error!(%live_stream_id, %err, "error closing live stream");
        }
        Ok(())
    }

    async fn has_active_playback(&self) -> Result<bool, ServiceError> {
        let sessions = self.sessions.lock().await;
        Ok(sessions.values().any(|s| s.now_playing_item_id.is_some()))
    }
}

impl FerrofinSessionManager {
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
                    request.password.as_ref().map_or("", Secret::expose),
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
        let user_uuid = parse_user_id(&user.id)?;
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

        let mut dto = self
            .upsert_session(
                app,
                app_version,
                device_id,
                device_name,
                remote_endpoint,
                Some(&user),
            )
            .await?;
        dto.server_id = Some(self.server_id.clone());
        // The full `AuthenticationResult` envelope (UserDto + ServerId) is
        // assembled by the caller from this data; the freshly minted token
        // (persisted on the created `Device` row) is returned so the caller can
        // echo it back to the client.
        let access_token = Secret::from(created.access_token);

        let _ = self
            .event_manager
            .publish(
                "AuthenticationSucceeded",
                dto.id.as_deref().unwrap_or_default(),
            )
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

/// Parses the hyphenated `UserEntity::id` string into a [`Uuid`].
///
/// The nil GUID is *meaningful* in this module — it is C# `Guid.Empty`, a session
/// with no signed-in user — so a malformed stored id must not degrade to it.
/// Doing so would silently record playstate against the empty user (losing the
/// real user's resume position), count another session's active sessions, or
/// treat a userless session as the admin. A corrupt row is an error instead.
fn parse_user_id(id: &str) -> Result<Uuid, ServiceError> {
    Uuid::parse_str(id)
        .map_err(|_| ServiceError::Backend("stored user id is not a guid".to_owned()))
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
        play_state: Some(ferrofin_model::session::PlayerStateInfo {
            position_ticks: session.now_playing_position_ticks,
            is_paused: session.now_playing_is_paused,
            can_seek: session.now_playing_item_id.is_some(),
            ..Default::default()
        }),
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
        // Hyphenated (canonical) form: `MessageId` is `format: uuid`, and the
        // Jellyfin Kotlin SDK parses it via `UUID.fromString`, which rejects the
        // dash-less form. `.simple()` here crashed Android clients on every push.
        message_id: Uuid::new_v4().hyphenated().to_string(),
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

/// Tests for the **bus-fallback exclusivity** rule of [`FerrofinSessionManager::broadcast`]:
/// a session that has a live direct [`WebSocketConnection`] must receive a broadcast
/// **exactly once, over that connection only** — never additionally over the session bus.
/// Duplicate delivery would double every remote-control command and SyncPlay message.
///
/// These live inline (rather than in `session_manager/tests.rs`) because they need their
/// own fixtures: a fake connection whose `is_open()` is configurable, and a bus whose
/// per-session deliveries are recorded.
#[cfg(test)]
mod bus_fallback_tests {
    use std::sync::Mutex as StdMutex;

    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_model::branding::BrandingOptions;
    use ferrofin_model::configuration::ServerConfiguration;
    use ferrofin_model::dto::BaseItemDto;
    use ferrofin_traits::configuration::ServerConfigurationManager;
    use ferrofin_traits::options::{AuthorizationInfo, DtoOptions};
    use ferrofin_traits::session_bus::SessionMessageBus;
    use ferrofin_traits::system::ServerApplicationPaths;

    use super::{
        Arc, Database, DtoService, FerrofinSessionManager, ServiceError, SessionManager,
        SessionMessageType, Uuid, WebSocketConnection, async_trait,
    };
    use crate::configuration_manager::default_server_configuration;
    use crate::device_manager::FerrofinDeviceManager;
    use crate::event_manager::FerrofinEventManager;
    use crate::session_bus::FerrofinSessionMessageBus;
    use crate::user_data_manager::FerrofinUserDataManager;
    use crate::user_manager::FerrofinUserManager;

    /// A configuration manager returning the factory defaults.
    struct FixedConfig;

    #[async_trait]
    impl ServerConfigurationManager for FixedConfig {
        fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
            unreachable!("not used by broadcast")
        }
        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Ok(Arc::new(default_server_configuration()))
        }
        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(&self) -> Result<BrandingOptions, ServiceError> {
            Ok(BrandingOptions::default())
        }
        async fn update_branding(&self, _branding: &BrandingOptions) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A DTO service the broadcast path never touches.
    struct UnusedDtoService;

    #[async_trait]
    impl DtoService for UnusedDtoService {
        async fn get_primary_image_aspect_ratio(
            &self,
            _item_id: Uuid,
        ) -> Result<Option<f64>, ServiceError> {
            unreachable!("dto service is not exercised by broadcast")
        }
        async fn get_base_item_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _user: Option<&ferrofin_db::entities::users::UserEntity>,
            _owner_id: Option<Uuid>,
        ) -> Result<BaseItemDto, ServiceError> {
            unreachable!("dto service is not exercised by broadcast")
        }
        async fn get_base_item_dtos(
            &self,
            _items: &[BaseItemEntity],
            _options: &DtoOptions,
            _user: Option<&ferrofin_db::entities::users::UserEntity>,
            _owner_id: Option<Uuid>,
            _skip_visibility_check: bool,
        ) -> Result<Vec<BaseItemDto>, ServiceError> {
            unreachable!("dto service is not exercised by broadcast")
        }
        async fn get_item_by_name_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _tagged_item_ids: Option<&[Uuid]>,
            _user: Option<&ferrofin_db::entities::users::UserEntity>,
        ) -> Result<BaseItemDto, ServiceError> {
            unreachable!("dto service is not exercised by broadcast")
        }
    }

    /// A fake connection recording every frame pushed to it, with a fixed
    /// `is_open()` answer so the "connection reports closed" path is reachable.
    struct FakeConnection {
        auth: AuthorizationInfo,
        open: bool,
        sent: StdMutex<Vec<Vec<u8>>>,
    }

    impl FakeConnection {
        fn new(open: bool) -> Arc<Self> {
            Arc::new(Self {
                auth: AuthorizationInfo::default(),
                open,
                sent: StdMutex::new(Vec::new()),
            })
        }
        fn frames(&self) -> Vec<Vec<u8>> {
            self.sent.lock().expect("fake connection mutex").clone()
        }
    }

    #[async_trait]
    impl WebSocketConnection for FakeConnection {
        fn remote_endpoint(&self) -> Option<&str> {
            None
        }
        fn authorization_info(&self) -> &AuthorizationInfo {
            &self.auth
        }
        fn is_open(&self) -> bool {
            self.open
        }
        async fn send(&self, message: &[u8]) -> Result<(), ServiceError> {
            self.sent
                .lock()
                .expect("fake connection mutex")
                .push(message.to_vec());
            Ok(())
        }
        async fn apply_request_culture(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// Every `(session_id, message)` pair the bus delivered, in order.
    type BusLog = Arc<StdMutex<Vec<(String, String)>>>;

    /// Builds a session manager over `db` wired to `bus`.
    fn manager_with_bus(
        db: &Database,
        bus: &Arc<FerrofinSessionMessageBus>,
    ) -> FerrofinSessionManager {
        let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig);
        FerrofinSessionManager::new(
            Arc::new(FerrofinUserManager::new(db.clone())),
            Arc::new(FerrofinDeviceManager::new(db.clone())),
            Arc::new(FerrofinUserDataManager::new(db.clone(), config)),
            crate::test_support::library_manager_over(db.clone()),
            Arc::new(UnusedDtoService),
            Arc::new(FerrofinEventManager::new()),
            db.clone(),
            "server-1",
        )
        .with_session_bus(
            Arc::clone(bus) as Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>
        )
    }

    /// Registers a bus sink for `session_id` that appends to `log`.
    fn register_sink(bus: &FerrofinSessionMessageBus, session_id: &str, log: &BusLog) {
        let id = session_id.to_owned();
        let log = Arc::clone(log);
        bus.register(
            session_id.to_owned(),
            Box::new(move |msg| {
                log.lock()
                    .expect("bus log mutex")
                    .push((id.clone(), msg.clone()));
            }),
        );
    }

    /// The three delivery regimes in one broadcast, all for the same user and all
    /// with a bus sink registered:
    ///
    /// - **open direct connection** → the frame lands on the connection exactly once
    ///   and **nothing** reaches the bus for that session (the exclusivity rule);
    /// - **no connection at all** → exactly one bus delivery;
    /// - **connection reporting `is_open() == false`** → nothing on the connection,
    ///   exactly one bus delivery (a dead socket must not swallow the message).
    #[tokio::test]
    async fn open_connection_suppresses_the_bus_fallback() {
        let db = crate::test_support::test_db().await;
        let bus = Arc::new(FerrofinSessionMessageBus::new());
        let mgr = manager_with_bus(&db, &bus);

        let user_id = Uuid::new_v4();
        let user = crate::test_support::seed_user(&db, user_id).await;

        // Three sessions of the same user, one per regime.
        let direct = mgr
            .log_session_activity("Direct", "1.0", "dev-direct", "Mac", "e", &user)
            .await
            .expect("direct session")
            .id
            .expect("direct session id");
        let busonly = mgr
            .log_session_activity("BusOnly", "1.0", "dev-bus", "TV", "e", &user)
            .await
            .expect("bus session")
            .id
            .expect("bus session id");
        let stale = mgr
            .log_session_activity("Stale", "1.0", "dev-stale", "Phone", "e", &user)
            .await
            .expect("stale session")
            .id
            .expect("stale session id");

        let open_conn = FakeConnection::new(true);
        mgr.add_web_socket(
            &direct,
            Arc::clone(&open_conn) as Arc<dyn WebSocketConnection>,
        )
        .await
        .expect("attach open connection");
        let closed_conn = FakeConnection::new(false);
        mgr.add_web_socket(
            &stale,
            Arc::clone(&closed_conn) as Arc<dyn WebSocketConnection>,
        )
        .await
        .expect("attach closed connection");

        // Every session — including the directly connected one — has a bus sink,
        // so a bus delivery to it is possible and only the exclusivity rule
        // prevents it.
        let log: BusLog = Arc::new(StdMutex::new(Vec::new()));
        for id in [&direct, &busonly, &stale] {
            register_sink(&bus, id, &log);
        }

        mgr.send_message_to_user_sessions(&[user_id], SessionMessageType::RestartRequired, "")
            .await
            .expect("broadcast");

        // 1. The open direct connection got the payload exactly once …
        let frames = open_conn.frames();
        assert_eq!(
            frames.len(),
            1,
            "session with an open connection must receive exactly one direct frame"
        );
        let envelope: serde_json::Value =
            serde_json::from_slice(&frames[0]).expect("frame is a JSON envelope");
        assert_eq!(envelope["MessageType"], "RestartRequired");

        // 2. … and the closed connection got nothing.
        assert!(
            closed_conn.frames().is_empty(),
            "a connection reporting is_open() == false must not be written to"
        );

        // 3. Bus deliveries: exactly one each for the bus-only and stale sessions,
        //    and NONE for the directly connected one.
        let delivered = log.lock().expect("bus log mutex").clone();
        let for_direct: Vec<_> = delivered.iter().filter(|(id, _)| id == &direct).collect();
        assert!(
            for_direct.is_empty(),
            "a session with an open direct connection must NEVER also receive the \
             message over the bus (duplicate delivery); got {for_direct:?}"
        );
        assert_eq!(
            delivered.iter().filter(|(id, _)| id == &busonly).count(),
            1,
            "a session with no connection must fall back to the bus exactly once"
        );
        assert_eq!(
            delivered.iter().filter(|(id, _)| id == &stale).count(),
            1,
            "a session whose connection is closed must take the bus path exactly once"
        );
        assert_eq!(
            delivered.len(),
            2,
            "exactly two of the three sessions may use the bus: {delivered:?}"
        );

        // The bus payload is the same envelope the direct path sends.
        let (_, bus_text) = delivered
            .iter()
            .find(|(id, _)| id == &busonly)
            .expect("bus delivery for the connection-less session");
        let bus_envelope: serde_json::Value =
            serde_json::from_str(bus_text).expect("bus payload is a JSON envelope");
        assert_eq!(bus_envelope["MessageType"], "RestartRequired");
    }

    /// A bus with no sinks that counts every delivery *attempt*, so a broadcast
    /// aimed at unreachable sessions is visible as work rather than as silence.
    #[derive(Default)]
    struct CountingBus {
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl ferrofin_traits::session_bus::SessionMessageBus for CountingBus {
        fn register(
            &self,
            _session_id: String,
            _sink: ferrofin_traits::session_bus::MessageSink,
        ) -> ferrofin_traits::session_bus::SinkToken {
            0
        }
        fn unregister(
            &self,
            _session_id: &str,
            _token: ferrofin_traits::session_bus::SinkToken,
        ) -> bool {
            false
        }
        fn send(&self, _session_id: &str, _message: String) -> bool {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        }
        fn is_connected(&self, _session_id: &str) -> bool {
            false
        }
    }

    /// The session pool keeps every device that ever authenticated, so a
    /// broadcast's predicate routinely matches sessions that have no live socket
    /// at all. Those must cost nothing: no bus delivery is attempted for them.
    #[tokio::test]
    async fn a_broadcast_no_one_can_receive_attempts_no_delivery() {
        let db = crate::test_support::test_db().await;
        let bus = Arc::new(CountingBus::default());
        let config: Arc<dyn ServerConfigurationManager> = Arc::new(FixedConfig);
        let mgr = FerrofinSessionManager::new(
            Arc::new(FerrofinUserManager::new(db.clone())),
            Arc::new(FerrofinDeviceManager::new(db.clone())),
            Arc::new(FerrofinUserDataManager::new(db.clone(), config)),
            crate::test_support::library_manager_over(db.clone()),
            Arc::new(UnusedDtoService),
            Arc::new(FerrofinEventManager::new()),
            db.clone(),
            "server-1",
        )
        .with_session_bus(
            Arc::clone(&bus) as Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>
        );

        let user_id = Uuid::new_v4();
        let user = crate::test_support::seed_user(&db, user_id).await;
        for device in ["dev-a", "dev-b", "dev-c"] {
            mgr.log_session_activity("Web", "1.0", device, "Box", "e", &user)
                .await
                .expect("session");
        }

        mgr.send_message_to_user_sessions(&[user_id], SessionMessageType::RestartRequired, "")
            .await
            .expect("broadcast");

        assert_eq!(
            bus.attempts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no session has a registered sink, so the bus must never be asked to deliver",
        );
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod broadcast_lock_tests {
    //! Lock-discipline tests for [`FerrofinSessionManager::broadcast`].
    //!
    //! The rest of the session-manager tests live in `session_manager/tests.rs`;
    //! these need a WebSocket connection whose `send` *parks*, which is what
    //! distinguishes "the lock is held across the await" from "it isn't", so they
    //! carry their own fixtures.

    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use ferrofin_db::Database;
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_model::configuration::ServerConfiguration;
    use ferrofin_model::dto::BaseItemDto;
    use ferrofin_model::session::SessionMessageType;
    use tokio::sync::{Semaphore, mpsc};
    use tokio::time::timeout;
    use uuid::Uuid;

    use ferrofin_traits::configuration::ServerConfigurationManager;
    use ferrofin_traits::dto::DtoService;
    use ferrofin_traits::error::ServiceError;
    use ferrofin_traits::net::WebSocketConnection;
    use ferrofin_traits::options::{AuthorizationInfo, DtoOptions};
    use ferrofin_traits::session::SessionManager;
    use ferrofin_traits::session_bus::SessionMessageBus;
    use ferrofin_traits::system::ServerApplicationPaths;

    use super::FerrofinSessionManager;
    use crate::configuration_manager::default_server_configuration;
    use crate::device_manager::FerrofinDeviceManager;
    use crate::event_manager::FerrofinEventManager;
    use crate::user_data_manager::FerrofinUserDataManager;
    use crate::user_manager::FerrofinUserManager;

    /// The wall-clock budget a "must still make progress" assertion gets. Under
    /// the pre-fix code the operation deadlocks, so any finite budget fails the
    /// test; this one is only generous enough not to flake on a loaded CI box.
    const PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);

    /// A config manager returning the factory-default configuration.
    struct FixedConfig;

    #[async_trait]
    impl ServerConfigurationManager for FixedConfig {
        fn application_paths(&self) -> Arc<dyn ServerApplicationPaths> {
            unreachable!("not used in these tests")
        }
        async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
            Ok(Arc::new(default_server_configuration()))
        }
        async fn update_configuration(
            &self,
            _configuration: &ServerConfiguration,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
            Ok(ferrofin_model::branding::BrandingOptions::default())
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// A DTO service the tested paths never invoke.
    struct UnusedDtoService;

    #[async_trait]
    impl DtoService for UnusedDtoService {
        async fn get_primary_image_aspect_ratio(
            &self,
            _item_id: Uuid,
        ) -> Result<Option<f64>, ServiceError> {
            unreachable!("dto service is not exercised by these tests")
        }
        async fn get_base_item_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
        ) -> Result<BaseItemDto, ServiceError> {
            unreachable!("dto service is not exercised by these tests")
        }
        async fn get_base_item_dtos(
            &self,
            _items: &[BaseItemEntity],
            _options: &DtoOptions,
            _user: Option<&UserEntity>,
            _owner_id: Option<Uuid>,
            _skip_visibility_check: bool,
        ) -> Result<Vec<BaseItemDto>, ServiceError> {
            unreachable!("dto service is not exercised by these tests")
        }
        async fn get_item_by_name_dto(
            &self,
            _item: &BaseItemEntity,
            _options: &DtoOptions,
            _tagged_item_ids: Option<&[Uuid]>,
            _user: Option<&UserEntity>,
        ) -> Result<BaseItemDto, ServiceError> {
            unreachable!("dto service is not exercised by these tests")
        }
    }

    /// A connection that stands in for a slow/stalled WebSocket client: `send`
    /// announces that it was entered, then parks on `gate` until the test opens
    /// it. This is the shape a real client that stops reading its socket takes.
    struct GatedConnection {
        auth: AuthorizationInfo,
        open: bool,
        entered: mpsc::UnboundedSender<()>,
        gate: Arc<Semaphore>,
        sent: StdMutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl WebSocketConnection for GatedConnection {
        fn remote_endpoint(&self) -> Option<&str> {
            None
        }
        fn authorization_info(&self) -> &AuthorizationInfo {
            &self.auth
        }
        fn is_open(&self) -> bool {
            self.open
        }
        async fn send(&self, message: &[u8]) -> Result<(), ServiceError> {
            let _ = self.entered.send(());
            let _permit = self.gate.acquire().await.expect("gate is not closed");
            self.sent
                .lock()
                .expect("sent frames")
                .push(message.to_vec());
            Ok(())
        }
        async fn apply_request_culture(&self) -> Result<(), ServiceError> {
            Ok(())
        }
    }

    /// Builds a session manager over `db` with the real sibling managers.
    fn manager(db: &Database) -> Arc<FerrofinSessionManager> {
        Arc::new(FerrofinSessionManager::new(
            Arc::new(FerrofinUserManager::new(db.clone())),
            Arc::new(FerrofinDeviceManager::new(db.clone())),
            Arc::new(FerrofinUserDataManager::new(
                db.clone(),
                Arc::new(FixedConfig),
            )),
            crate::test_support::library_manager_over(db.clone()),
            Arc::new(UnusedDtoService),
            Arc::new(FerrofinEventManager::new()),
            db.clone(),
            "server-1".to_owned(),
        ))
    }

    /// A broadcast must not wedge the session table while one client is slow to
    /// accept its frame.
    ///
    /// Before the fix, `broadcast` held the `sessions` mutex across
    /// `connection.send(..).await`, so a single stalled WebSocket client froze
    /// **every** session operation — playback start/progress/stop, session
    /// upsert on each authenticated request, capability reports, socket attach.
    /// With the guard released before the sends, the stalled client delays only
    /// its own frame.
    #[tokio::test]
    async fn broadcast_does_not_hold_the_session_lock_across_a_socket_send() {
        let db = crate::test_support::test_db().await;
        let mgr = manager(&db);
        let user_id = Uuid::new_v4();
        let user = crate::test_support::seed_user(&db, user_id).await;

        let stalled = mgr
            .log_session_activity("Stalled Client", "1.0", "dev-stalled", "TV", "e", &user)
            .await
            .expect("session created");
        let stalled_id = stalled.id.clone().expect("session id");

        let gate = Arc::new(Semaphore::new(0));
        let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
        let conn = Arc::new(GatedConnection {
            auth: AuthorizationInfo::default(),
            open: true,
            entered: entered_tx,
            gate: Arc::clone(&gate),
            sent: StdMutex::new(Vec::new()),
        });
        mgr.add_web_socket(
            &stalled_id,
            Arc::clone(&conn) as Arc<dyn WebSocketConnection>,
        )
        .await
        .expect("socket attached");

        // Push a message; it parks inside the stalled client's `send`.
        let broadcaster = {
            let mgr = Arc::clone(&mgr);
            tokio::spawn(async move {
                mgr.send_message_to_user_sessions(
                    &[user_id],
                    SessionMessageType::RestartRequired,
                    "",
                )
                .await
            })
        };

        // Wait until the send is genuinely in flight — this is precisely the
        // point at which the old code was still holding the session lock.
        timeout(PROGRESS_TIMEOUT, entered_rx.recv())
            .await
            .expect("the stalled client's send was entered")
            .expect("entered signal");

        // …and now every other session operation must still complete.
        timeout(PROGRESS_TIMEOUT, async {
            assert!(!mgr.has_active_playback().await.expect("playback probe"));
            let other = mgr
                .log_session_activity("Web", "1.0", "dev-web", "Mac", "e", &user)
                .await
                .expect("second session created");
            assert_ne!(other.id, stalled.id);
            let listed = mgr
                .get_sessions(Uuid::nil(), None, None, None, true)
                .await
                .expect("sessions listed");
            assert_eq!(listed.len(), 2);
        })
        .await
        .expect("session operations proceed while one client's socket is stalled");

        // Releasing the client completes the delivery — the frame is not lost.
        gate.add_permits(1);
        broadcaster
            .await
            .expect("broadcast task joins")
            .expect("broadcast succeeds");
        let frames = conn.sent.lock().expect("sent frames");
        assert_eq!(frames.len(), 1);
        let frame: serde_json::Value = serde_json::from_slice(&frames[0]).expect("frame is JSON");
        assert_eq!(frame["MessageType"], "RestartRequired");
    }

    /// Delivery parity: a session whose only direct connection is **closed**
    /// counts as having no direct connection, so the message falls back to the
    /// session bus (and the closed socket is not written to).
    #[tokio::test]
    async fn a_closed_direct_connection_falls_back_to_the_bus() {
        let db = crate::test_support::test_db().await;
        let bus: Arc<dyn SessionMessageBus> = Arc::new(crate::FerrofinSessionMessageBus::new());
        let mgr = Arc::new(
            manager(&db)
                .as_ref()
                .clone()
                .with_session_bus(Arc::clone(&bus)),
        );
        let user_id = Uuid::new_v4();
        let user = crate::test_support::seed_user(&db, user_id).await;
        let session = mgr
            .log_session_activity("Web", "1.0", "dev-1", "Mac", "e", &user)
            .await
            .expect("session created");
        let session_id = session.id.clone().expect("session id");

        // A stale (closed) controller is still attached to the session.
        let (entered_tx, _entered_rx) = mpsc::unbounded_channel();
        let closed = Arc::new(GatedConnection {
            auth: AuthorizationInfo::default(),
            open: false,
            entered: entered_tx,
            gate: Arc::new(Semaphore::new(16)),
            sent: StdMutex::new(Vec::new()),
        });
        mgr.add_web_socket(
            &session_id,
            Arc::clone(&closed) as Arc<dyn WebSocketConnection>,
        )
        .await
        .expect("socket attached");

        let received = Arc::new(StdMutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&received);
        bus.register(
            session_id.clone(),
            Box::new(move |msg| sink.lock().expect("bus messages").push(msg)),
        );

        mgr.send_message_to_user_sessions(&[user_id], SessionMessageType::RestartRequired, "")
            .await
            .expect("broadcast succeeds");

        assert!(
            closed.sent.lock().expect("sent frames").is_empty(),
            "a closed connection is never written to"
        );
        let messages = received.lock().expect("bus messages");
        assert_eq!(messages.len(), 1);
        let envelope: serde_json::Value =
            serde_json::from_str(&messages[0]).expect("envelope is JSON");
        assert_eq!(envelope["MessageType"], "RestartRequired");
    }
}
