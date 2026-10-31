//! The session WebSocket (`/socket`, and the legacy `/embywebsocket` alias).
//!
//! jellyfin-web opens a WebSocket immediately after authenticating and treats it
//! as *the* server connection — if it never establishes, the client reports
//! "Connection Failure" and refuses to enter the dashboard, even though the REST
//! calls all succeeded. These routes are NOT part of the OpenAPI contract (Swagger
//! doesn't describe WebSocket endpoints), so they are registered as extras.
//!
//! The socket resolves its credentials as an HTTP request does (C#
//! `WebSocketManager.WebSocketRequestHandler` → `AuthService.Authenticate`), so
//! the Emby-era credential forms follow `EnableLegacyAuthorization` here too, and
//! a request without valid credentials is refused with `403` before the upgrade.
//! No route policy runs on it (remote access, access schedule), as upstream.
//! It then answers pings, sends periodic keep-alives, and **registers a message
//! sink** on the [`SessionMessageBus`] keyed by the
//! caller's session id, so server→client pushes (SyncPlay commands/group updates
//! and the remote-control `Play`/`Playstate`/`GeneralCommand` casts) reach this
//! client. The session manager also treats a bus-registered session as having a
//! live controller, which is what makes it appear in the cast-to-device menu
//! (`GET /Sessions?ControllableByUserId=…` → `SupportsRemoteControl`). The sink
//! is unregistered when the socket closes.
//!
//! Closing the socket also **ends the session** when no other socket took its
//! place -- the port of `WebSocketController.OnConnectionClosed` ->
//! `SessionManager.CloseIfNeededAsync`, which is how upstream keeps
//! `_activeConnections` from accumulating one entry per client that ever
//! connected. Without it a closed browser tab stays in `GET /Sessions` forever
//! and its `SessionEnded` activity entry is never written.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ferrofin_traits::activity::ActivityManager;
use ferrofin_traits::options::AuthorizationInfo;
use ferrofin_traits::session::{SessionEvent, SessionManager};
use tokio::sync::mpsc;

use crate::auth::AuthenticatedIdentity;
use crate::error::ApiError;
use crate::state::AppState;

/// The server keep-alive interval advertised to the client, in seconds
/// (C# `SessionWebSocketListener.WebSocketLostTimeout`). It is the value carried
/// in the `ForceKeepAlive` payload AND the deadline the watchdog below measures
/// silence against.
const KEEPALIVE_SECS: u64 = 60;

// The watchdog's two derived thresholds. Upstream stores them as FACTORS of
// `WebSocketLostTimeout` (`IntervalFactor` 0.2f, `ForceKeepAliveFactor` 0.75f)
// and multiplies at each use; here they are written out in seconds so the
// arithmetic below stays in `f64` without casting a `u64` at every comparison.
// The compile-time assertion keeps all three tied to the one timeout.
const _: () = assert!(
    KEEPALIVE_SECS == 60,
    "the watchdog constants below assume 60 s"
);

/// How often the keep-alive watchdog looks at the socket — C#
/// `SessionWebSocketListener.IntervalFactor` (0.2f) × `WebSocketLostTimeout`.
const KEEPALIVE_WATCH_SECS: f64 = 12.0;

/// How long a socket may stay silent before the watchdog prods it with another
/// `ForceKeepAlive` — C# `ForceKeepAliveFactor` (0.75f) × `WebSocketLostTimeout`.
const KEEPALIVE_FORCE_AFTER_SECS: f64 = 45.0;

/// The silence after which a socket leaves the watchlist — C#
/// `WebSocketLostTimeout` itself, as `f64` for the comparison.
const KEEPALIVE_LOST_SECS: f64 = 60.0;

/// How many server→client pushes may sit queued for one socket before the
/// server gives up on that client.
///
/// The queue exists because the bus sink must be non-blocking: a broadcast
/// enqueues onto every recipient's channel and returns, so one slow reader can
/// never stall the others. Unbounded, that is a memory bomb — a client that
/// completes the handshake and then stops reading makes the server buffer every
/// message aimed at it for as long as it stays connected (measured: 20 such
/// clients grew RSS by 864 MB over 120k broadcasts, with no ceiling). Bounded,
/// the worst case is `depth × message size` per socket and the socket is closed
/// on overflow so the client reconnects and refetches state.
const PUSH_QUEUE_DEPTH: usize = 256;

/// Capacity of each socket's inbound frame buffer.
///
/// This is not a protocol knob — it is the per-socket scratch buffer tungstenite
/// reads into, and it is charged on the **push** path, not the receive path:
/// `read_in` does `in_buffer.resize(capacity, 0)` — a memset of the whole
/// buffer — before *every* read attempt, then truncates back. The socket task's
/// `select!` re-polls `socket.recv()` on every loop iteration, and every
/// server→client push wakes that loop, so one push costs one full-buffer memset
/// even though the client sent nothing.
///
/// tungstenite's default (which axum inherits) is 128 KiB, so 500 idle-reading
/// sockets memset 64 MiB per broadcast round and hold 64 MiB resident for it.
/// Measured at 500 sockets receiving a `UserDataChanged` fan-out (interleaved
/// A/B, 3 cycles, ~30 s each, noise floor +/-5%):
///
/// | read buffer | CPU per pushed message | RSS at 500 sockets |
/// |-------------|------------------------|--------------------|
/// | 256 KiB     | 105 us                 | 235 MB             |
/// | 128 KiB     | 36 us                  | 197 MB             |
/// | 16 KiB      |  9.5 us                | 121 MB             |
/// | 4 KiB       |  8.3 us                | 130 MB             |
///
/// 4 KiB is chosen because nothing a Jellyfin client sends over this socket is
/// large: the inbound vocabulary is `KeepAlive` and the `*Start`/`*Stop`
/// subscription messages, all well under 100 bytes. A larger inbound frame is
/// still handled correctly — tungstenite reserves the frame's full length once
/// the header is parsed and reads it across several passes — it just costs more
/// than one read.
const READ_BUFFER_BYTES: usize = 4 * 1024;

/// Registers the WebSocket routes (`/socket` + the legacy `/embywebsocket`).
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/socket", get(websocket_upgrade))
        .route("/embywebsocket", get(websocket_upgrade))
}

/// `GET /socket` — upgrade the connection to a WebSocket.
///
/// Port of `WebSocketManager.WebSocketRequestHandler`: the credentials are
/// resolved as for any request — jellyfin-web sends `?ApiKey=`,
/// jellyfin-sdk-kotlin an `Authorization: MediaBrowser` header — and a request
/// they do not authenticate is the `SecurityException("Token is required")`
/// upstream answers with `403`. No route policy runs, so remote access and the
/// access schedule are not checked here. The authenticated token is then
/// resolved to the caller's session so the socket can register a delivery sink
/// for server→client pushes.
async fn websocket_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    identity: Result<AuthenticatedIdentity, ApiError>,
) -> Response {
    let info = match identity {
        Ok(AuthenticatedIdentity(info)) => info,
        Err(e) if e.status() == axum::http::StatusCode::UNAUTHORIZED => {
            return ApiError::Forbidden("Token is required".to_owned()).into_response();
        }
        Err(e) => return e.into_response(),
    };
    let caller = resolve_caller(&state, &info).await;
    ws.read_buffer_size(READ_BUFFER_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, caller))
}

/// The resolved identity of an authenticated socket: its session id (the bus
/// sink key), the signed-in user (who the subscription streams answer for), and
/// whether that caller is an administrator. The role is read once, at connect,
/// as upstream reads the connection's `AuthorizationInfo` captured at upgrade.
#[derive(Clone)]
struct SocketCaller {
    session_id: String,
    /// Nil for an API-key socket, whose session has no user.
    user_id: uuid::Uuid,
    is_admin: bool,
    /// The socket authenticated with an API key: its session has no user and
    /// it sees every session (`SessionInfoWebSocketListener` applies no user
    /// filter to an API key).
    is_api_key: bool,
}

/// Why an authenticated socket carries no session — kept distinct so the logs
/// can tell an API key (expected) from a user token that didn't resolve (a
/// server bug that would silently drop SyncPlay/push delivery).
#[derive(Clone, Copy)]
enum SessionlessReason {
    /// An API-key socket's session could not be opened.
    ApiKeySessionFailed,
    /// A token was presented but the session manager could not resolve it.
    TokenUnresolved,
    /// The token resolved to a session that carries no session id.
    SessionWithoutId,
}

impl SessionlessReason {
    /// The value logged in the `reason` field.
    fn as_str(self) -> &'static str {
        match self {
            Self::ApiKeySessionFailed => "api-key-session-failed",
            Self::TokenUnresolved => "token-unresolved",
            Self::SessionWithoutId => "session-without-id",
        }
    }
}

/// Resolves an authenticated socket's session, or the reason it has none.
///
/// The token is the one [`websocket_upgrade`] authenticated. It goes to the
/// session manager ([`get_session_by_authentication_token`]) with an empty
/// `device_id`, which keys the session off the token's own device, so the
/// derived id matches the one the `/SyncPlay/*` handlers compute for the same
/// client, and with the caller's address, which upstream's `GetSession` records
/// on every connect (`GetNormalizedRemoteIP`).
///
/// An API key has no device row, so its socket gets the session upstream's
/// `RequestHelpers.GetSession` logs for it instead: one with no user
/// (`LogSessionActivity(client, version, deviceId, device, remoteIp, null)`),
/// keyed by the client and device the auth layer filled in for the key.
/// The key is upstream's `{appName}{deviceId}`, so a key whose name and
/// supplied `DeviceId` equal a signed-in client's would share that client's
/// session (and clear its user), exactly as upstream; without a supplied
/// `DeviceId` the key's device is the server's system id, which no user
/// session carries.
///
/// [`get_session_by_authentication_token`]: ferrofin_traits::session::SessionManager::get_session_by_authentication_token
async fn resolve_caller(
    state: &AppState,
    info: &AuthorizationInfo,
) -> Result<SocketCaller, SessionlessReason> {
    if info.is_api_key {
        let session = state
            .sessions
            .log_session_activity(
                info.client.as_deref().unwrap_or_default(),
                info.version.as_deref().unwrap_or_default(),
                info.device_id.as_deref().unwrap_or_default(),
                info.device.as_deref().unwrap_or_default(),
                info.remote_endpoint.as_deref().unwrap_or_default(),
                None,
            )
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "websocket API-key session could not be opened");
                SessionlessReason::ApiKeySessionFailed
            })?;
        let session_id = session.id.ok_or(SessionlessReason::SessionWithoutId)?;
        // An API key is elevated (`is_administrator_identity`), so it may
        // subscribe to the activity log as upstream's listener allows.
        return Ok(SocketCaller {
            session_id,
            user_id: session.user_id,
            is_admin: true,
            is_api_key: true,
        });
    }
    let token = info
        .token
        .as_ref()
        .ok_or(SessionlessReason::TokenUnresolved)?;
    let session = state
        .sessions
        .get_session_by_authentication_token(
            token.expose(),
            "",
            info.remote_endpoint.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "websocket token did not resolve to a session");
            SessionlessReason::TokenUnresolved
        })?;
    let session_id = session.id.ok_or(SessionlessReason::SessionWithoutId)?;
    // A failed policy lookup leaves the caller without the role: fail closed.
    let is_admin = crate::auth::is_administrator_identity(state, info)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "websocket caller's role did not resolve");
            false
        });
    Ok(SocketCaller {
        session_id,
        user_id: session.user_id,
        is_admin,
        is_api_key: false,
    })
}

/// Whether `caller` may start the subscription `inbound` asks for.
///
/// Port of `ActivityLogWebSocketListener.Start`, which throws
/// `AuthenticationException("Only admin users can retrieve the activity log.")`
/// unless the caller is an API key or an administrator, the same role
/// `GET /System/ActivityLog/Entries` demands. Upstream's connection logs the
/// throw as a warning and stays open with the stream unarmed, as here. The
/// other streams need only a session (`SessionInfoWebSocketListener` scopes
/// what it sends; `ScheduledTasksWebSocketListener` has no check).
fn may_subscribe(caller: &SocketCaller, inbound: Inbound) -> bool {
    if matches!(inbound, Inbound::ActivityStart(_)) && !caller.is_admin {
        tracing::warn!(
            user_id = %caller.user_id,
            "only admin users can retrieve the activity log"
        );
        return false;
    }
    true
}

/// The action to take for one inbound frame — split out so the decision logic is
/// unit-testable without a live socket.
enum Action {
    /// Reply with a pong carrying the given payload.
    Pong(axum::body::Bytes),
    /// The peer is gone (close frame, stream end, or error) — stop.
    Stop,
    /// Nothing to do (binary/pong, or an unrecognized text message).
    Ignore,
    /// A recognized inbound protocol message (parsed from a text frame).
    Inbound(Inbound),
}

/// The inbound client→server messages the socket protocol handles: the
/// keep-alive ping and the dashboard's periodic-stream subscriptions
/// (Jellyfin's `BasePeriodicWebSocketListener` Start/Stop pairs).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Inbound {
    /// `KeepAlive` — the client's periodic ping; answered with a `KeepAlive` ack.
    KeepAlive,
    /// `SessionsStart` — send the session list on each session event, at most
    /// one non-forced send per `period`.
    SessionsStart(Duration),
    /// `SessionsStop` — stop the session stream.
    SessionsStop,
    /// `ScheduledTasksInfoStart` — send the task list now, on each task start
    /// and completion, and on progress at most once per `period`.
    TasksStart(Duration),
    /// `ScheduledTasksInfoStop` — stop the task stream.
    TasksStop,
    /// `ActivityLogEntryStart` — send an empty `ActivityLogEntry` frame on
    /// each created entry, a signal to refetch the entries.
    ActivityStart(Duration),
    /// `ActivityLogEntryStop` — stop the activity stream.
    ActivityStop,
}

/// Decides how to react to one received frame.
fn action_for(frame: Option<Result<Message, axum::Error>>) -> Action {
    match frame {
        Some(Ok(Message::Ping(payload))) => Action::Pong(payload),
        Some(Ok(Message::Close(_)) | Err(_)) | None => Action::Stop,
        Some(Ok(Message::Text(text))) => {
            parse_inbound(&text).map_or(Action::Ignore, Action::Inbound)
        }
        Some(Ok(_)) => Action::Ignore,
    }
}

/// Parses a text frame as an inbound protocol message, or `None` for anything
/// unrecognized (matching the C# socket, which ignores unknown message types).
///
/// Upstream deserializes the frame with `JsonDefaults.Options`
/// (`WebSocketConnection.cs:60,262`): member names (`MessageType`, `Data`) match
/// exactly, as there is no MVC binder to fold them, but `MessageType` is a
/// `SessionMessageType` read by `JsonStringEnumConverter`, so its VALUE matches
/// ignoring case. Numeric message types retain their actual C# discriminants.
fn parse_inbound(text: &str) -> Option<Inbound> {
    use ferrofin_model::session::SessionMessageType as Kind;
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Envelope {
        message_type: Kind,
        #[serde(default)]
        data: Option<serde_json::Value>,
    }
    let value: Envelope = crate::extract::deserialize_defaults(text).ok()?;
    // C# forwards JsonElement.ToString(). Only a string can carry the
    // comma-separated subscription period; other token kinds use the fallback.
    let period = || subscription_period(value.data.as_ref().and_then(serde_json::Value::as_str));
    match value.message_type {
        Kind::KeepAlive => Some(Inbound::KeepAlive),
        Kind::SessionsStart => Some(Inbound::SessionsStart(period())),
        Kind::SessionsStop => Some(Inbound::SessionsStop),
        Kind::ScheduledTasksInfoStart => Some(Inbound::TasksStart(period())),
        Kind::ScheduledTasksInfoStop => Some(Inbound::TasksStop),
        Kind::ActivityLogEntryStart => Some(Inbound::ActivityStart(period())),
        Kind::ActivityLogEntryStop => Some(Inbound::ActivityStop),
        _ => None,
    }
}

/// The default stream period when the subscription's `Data` is absent or
/// malformed — jellyfin-web subscribes with "0,1500".
const DEFAULT_STREAM_MILLIS: u64 = 1500;

/// Parses the `"initialDelayMs,periodMs"` subscription payload (the C#
/// `BasePeriodicWebSocketListener` Data convention) into the stream period.
/// It is upstream's throttle interval (`WebSocketListenerState.IntervalMs`)
/// for every stream. The initial delay is ignored, as upstream stores
/// `InitialDelayMs` and never uses it.
///
/// There is no floor, as upstream has none (`BasePeriodicWebSocketListener.cs:177`):
/// 0 is kept, and a negative period, which upstream's `long.Parse` accepts and
/// whose throttle passes every send, is 0. No timer runs on the period: every
/// send is triggered by an event, forced ones coalesce to one frame per wake
/// (`drain`), and throttled ones are also spaced by the period, so nothing
/// loops whatever a client asks.
fn subscription_period(data: Option<&str>) -> Duration {
    let millis = data
        .and_then(|d| d.split(',').nth(1))
        .and_then(|p| p.trim().parse::<i64>().ok())
        .map_or(DEFAULT_STREAM_MILLIS, |p| u64::try_from(p).unwrap_or(0));
    Duration::from_millis(millis)
}

/// Builds one socket's bus sink: a non-blocking enqueue onto its bounded push
/// queue.
///
/// The sink must never block (a broadcast calls it once per recipient, in line),
/// so a client that has stopped draining can only be handled two ways: buffer
/// for it without limit, or stop. We stop — the message is dropped and
/// `overflowed` is raised so [`handle_socket`] closes the connection, after
/// which the client reconnects and refetches state. A `Closed` channel is the
/// ordinary "socket already gone" race and is simply swallowed.
fn push_sink(
    tx: mpsc::Sender<String>,
    overflowed: std::sync::Arc<tokio::sync::Notify>,
) -> ferrofin_traits::session_bus::MessageSink {
    Box::new(move |msg| {
        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg) {
            overflowed.notify_one();
        }
    })
}

/// Logs the one reason the server hangs up on a client of its own accord.
fn warn_overflow() {
    tracing::warn!(
        depth = PUSH_QUEUE_DEPTH,
        "websocket push queue overflowed; closing the socket",
    );
}

/// Writes one frame, abandoning the socket if this client's push queue
/// overflows while the write is blocked. Returns whether the socket is still
/// usable.
///
/// A client that stops reading shuts its TCP window, so the write blocks
/// indefinitely and the task never returns to the main `select!` — waiting on
/// `send` alone would leave such a socket registered on the bus forever,
/// counting as a live listener while receiving nothing. Racing the write against
/// the overflow signal is what actually retires it. `WebSocket::send` is not
/// cancel-safe, but the loser of this race is a socket we are about to drop, so
/// a half-written frame is irrelevant.
async fn send_frame(
    socket: &mut WebSocket,
    msg: Message,
    overflowed: &tokio::sync::Notify,
) -> bool {
    tokio::select! {
        result = socket.send(msg) => result.is_ok(),
        () = overflowed.notified() => {
            warn_overflow();
            false
        }
    }
}

/// One socket's bus registration: the session it delivers for, the bus, and the
/// token that unregisters *this* socket without disturbing the session's others.
type Registration = (
    String,
    std::sync::Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>,
    ferrofin_traits::session_bus::SinkToken,
);

/// Registers a socket's push sink on the session bus, if the caller resolved
/// to a session and a bus is wired. A sessionless socket (or a server with no
/// bus) gets no registration and simply never receives pushes.
fn register_sink(
    state: &AppState,
    caller: Option<&SocketCaller>,
    tx: &mpsc::Sender<String>,
    overflowed: &std::sync::Arc<tokio::sync::Notify>,
) -> Option<Registration> {
    let (c, bus) = (caller?, state.session_bus.as_ref()?);
    let token = bus.register(
        c.session_id.clone(),
        push_sink(tx.clone(), std::sync::Arc::clone(overflowed)),
    );
    Some((c.session_id.clone(), std::sync::Arc::clone(bus), token))
}

/// Drops this socket's sink and, when it was the session's **last** socket, ends
/// the session — the port of `WebSocketController.OnConnectionClosed` →
/// `ISessionManager.CloseIfNeededAsync`.
///
/// `unregister` reports whether a registration survives: a socket that notices
/// its own death only after the client reconnected finds the reconnect's sink
/// still there, and must leave that live session alone. Otherwise the session
/// ends, which drops it from the session pool (upstream `_activeConnections`,
/// which is why a closed browser tab stops appearing in `GET /Sessions`) and
/// emits `SessionEnded`. Returns whether the session was ended.
async fn end_session_if_last_socket<F, Fut>(
    bus: &dyn ferrofin_traits::session_bus::SessionMessageBus,
    session_id: &str,
    token: u64,
    end_session: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), ferrofin_traits::error::ServiceError>>,
{
    if bus.unregister(session_id, token) {
        return false;
    }
    if let Err(err) = end_session().await {
        // The session may already be gone (an explicit `/Sessions/Logout` races
        // the socket close). Nothing to report to anyone — the socket is closed.
        tracing::debug!(session_id, %err, "session already ended");
    }
    true
}

/// The "close every socket" generation: bumped by [`close_all_sockets`], watched
/// by every session loop. Process-wide on purpose — an upgraded connection is
/// outside axum's graceful drain, and a socket that outlived its host would
/// keep pushing through the previous lifetime's session manager after an
/// in-process restart.
static CLOSE_ALL: std::sync::LazyLock<tokio::sync::watch::Sender<u64>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// Closes every open WebSocket: each session loop sends a Close frame and
/// ends. Called by the composition root when the server drains (Kestrel
/// closes its sockets on stop; clients reconnect to the new host).
pub fn close_all_sockets() {
    CLOSE_ALL.send_modify(|generation| *generation += 1);
}

/// Server drain: the ServerShuttingDown push was queued just before the
/// close-all fired and `select!` is unordered, so flush whatever is queued
/// first, then say goodbye properly so the client reconnects. Every send is
/// bounded by `send_frame`, so a stalled peer cannot pin the old host.
async fn say_goodbye(
    socket: &mut WebSocket,
    rx: &mut mpsc::Receiver<String>,
    overflowed: &tokio::sync::Notify,
) {
    while let Ok(push) = rx.try_recv() {
        if !send_frame(socket, Message::Text(push.into()), overflowed).await {
            return;
        }
    }
    let _ = send_frame(socket, Message::Close(None), overflowed).await;
}

/// Holds a WebSocket open: register the caller's push sink (if it has a session),
/// answer pings, forward server→client pushes, send a periodic keep-alive, and
/// close cleanly — unregistering the sink — when the peer goes away.
#[tracing::instrument(
    name = "ws_session",
    skip_all,
    fields(session_id = caller.as_ref().map_or("none", |c| c.session_id.as_str()))
)]
async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    caller: Result<SocketCaller, SessionlessReason>,
) {
    let started = std::time::Instant::now();
    let mut close_all = CLOSE_ALL.subscribe();
    // `tx` feeds this socket; the bus holds a clone as the session's delivery
    // sink. Keeping `tx` alive here also keeps `rx` open for sessionless sockets
    // (no sink registered) so the forward branch stays pending rather than
    // closing the loop. The channel is bounded (see `PUSH_QUEUE_DEPTH`) so a
    // client that stops reading cannot make the server buffer without limit.
    let (tx, mut rx) = mpsc::channel::<String>(PUSH_QUEUE_DEPTH);
    // Raised by the sink when the queue is full; the loop closes the socket.
    let overflowed = std::sync::Arc::new(tokio::sync::Notify::new());
    let registration = register_sink(&state, caller.as_ref().ok(), &tx, &overflowed);
    log_connect(caller.as_ref(), registration.is_some());
    let caller = caller.ok();

    // `SessionWebSocketListener.KeepAliveSockets` is a WATCHDOG, not a metronome:
    // it wakes every `IntervalFactor * WebSocketLostTimeout` and only sends a
    // `ForceKeepAlive` to a socket that has been SILENT for longer than
    // `ForceKeepAliveFactor * WebSocketLostTimeout`, where "silent" is reset by
    // the client's own `KeepAlive`. A socket past the full timeout is lost:
    // upstream removes it from the watchlist and disposes it (since 12.0-rc3,
    // jellyfin/jellyfin#17079), which ends the receive, so its session and
    // SyncPlay seat go with it. Here the loop ends and the teardown below runs.
    // Every socket has a session upstream (`RequestHelpers.GetSession`), but a
    // sessionless one here (see `resolve_caller`) is only dropped from the
    // watch, as before: nothing told it to send `KeepAlive`.
    //
    // A fixed 60 s metronome — what this loop used to run — is neither: it kept
    // prodding a socket the client was actively talking on, and its cadence
    // could not match the oracle's on any live comparison.
    let mut keepalive = tokio::time::interval(Duration::from_secs_f64(KEEPALIVE_WATCH_SECS));
    keepalive.tick().await; // consume the immediate first tick
    // `IWebSocketConnection.LastKeepAliveDate`, seeded at registration
    // (SessionWebSocketListener.cs:164).
    let mut last_keep_alive = tokio::time::Instant::now();
    let mut watched = true;

    // The connect-time `ForceKeepAlive` (see `greet_with_keep_alive`). Guarded
    // rather than unconditional: a peer that is already gone when the greeting
    // is sent must still fall through to the teardown below.
    let open = greet_with_keep_alive(&mut socket, &overflowed, caller.is_some()).await;

    // The dashboard's streams, armed by *Start subscription messages
    // (each is `None` until subscribed). Only a socket with a session may
    // subscribe — the streams answer as the socket's user — and the activity
    // stream is an administrator's alone (`may_subscribe`).
    let mut streams = Streams::default();

    if open {
        loop {
            tokio::select! {
                frame = socket.recv() => match action_for(frame) {
                    Action::Pong(payload) => {
                        if !send_frame(&mut socket, Message::Pong(payload), &overflowed).await {
                            break;
                        }
                    }
                    Action::Stop => break,
                    Action::Ignore => {}
                    Action::Inbound(Inbound::KeepAlive) => {
                        // Ack the client's ping. `WebSocketConnection.SendKeepAliveResponse`
                        // (:227-232) stamps `LastKeepAliveDate` as it answers, which
                        // is what stops the watchdog prodding a live client.
                        last_keep_alive = tokio::time::Instant::now();
                        let ack = Message::Text(keep_alive_ack().into());
                        if !send_frame(&mut socket, ack, &overflowed).await {
                            break;
                        }
                    }
                    Action::Inbound(inbound) => {
                        log_task_subscription(inbound, caller.is_some());
                        if caller.as_ref().is_some_and(|c| may_subscribe(c, inbound)) {
                            let message = update_task_subscription(&state, inbound, &mut streams).await;
                            if !send_optional(&mut socket, message, &overflowed).await {
                                break;
                            }
                            streams.apply(inbound, state.sessions.as_ref(), state.activity.as_ref());
                        }
                    }
                },
                Some(push) = rx.recv() => {
                    // A server→client message (SyncPlay command/update, …).
                    if !send_frame(&mut socket, Message::Text(push.into()), &overflowed).await {
                        break;
                    }
                },
                () = overflowed.notified() => {
                    warn_overflow();
                    break;
                },
                Ok(()) = close_all.changed() => {
                    say_goodbye(&mut socket, &mut rx, &overflowed).await;
                    break;
                },
                _ = keepalive.tick(), if watched => {
                    if !keep_alive_watchdog(&mut socket, &overflowed, last_keep_alive,
                        caller.is_some(), &mut watched).await
                    {
                        break;
                    }
                }
                first = next_event(&mut streams.sessions) => {
                    let message =
                        sessions_wake(&state, caller.as_ref(), &mut streams.sessions, first).await;
                    if !send_optional(&mut socket, message, &overflowed).await {
                        break;
                    }
                }
                update = next_task_update(&mut streams.task_updates) => {
                    let msg = task_transition_message(&state, update, &mut streams).await;
                    if !send_optional(&mut socket, msg, &overflowed).await {
                        break;
                    }
                }
                first = next_event(&mut streams.tasks) => {
                    let message = task_progress_wake(&state, &mut streams, first).await;
                    if !send_optional(&mut socket, message, &overflowed).await {
                        break;
                    }
                }
                first = next_event(&mut streams.activity) => {
                    let message = activity_wake(&mut streams.activity, first);
                    if !send_optional(&mut socket, message, &overflowed).await {
                        break;
                    }
                }
            }
        }
    }

    unregister_socket(&state, registration).await;
    drop(tx);
    tracing::info!(
        elapsed_s = started.elapsed().as_secs(),
        "websocket disconnected"
    );
}

/// Debug diagnostics for accepted and ignored task subscriptions.
fn log_task_subscription(inbound: Inbound, session: bool) {
    if matches!(inbound, Inbound::TasksStart(_) | Inbound::TasksStop) {
        tracing::debug!(
            session,
            subscribing = matches!(inbound, Inbound::TasksStart(_)),
            "dashboard task subscription requested"
        );
    }
}

async fn unregister_socket(state: &AppState, registration: Option<Registration>) {
    if let Some((sid, bus, token)) = registration {
        end_session_if_last_socket(bus.as_ref(), &sid, token, || {
            state.sessions.close_if_needed(&sid)
        })
        .await;
    }
}

/// Send a stream update when it contains a payload.
async fn send_optional(
    socket: &mut WebSocket,
    message: Option<String>,
    overflowed: &tokio::sync::Notify,
) -> bool {
    match message {
        Some(message) => send_frame(socket, Message::Text(message.into()), overflowed).await,
        None => true,
    }
}

/// The one `websocket connected` line, which says whether the socket carries a
/// session and — when it does not — why not.
fn log_connect(caller: Result<&SocketCaller, &SessionlessReason>, registered: bool) {
    match caller {
        Ok(_) => tracing::info!(session = registered, "websocket connected"),
        Err(reason) => tracing::info!(
            session = false,
            reason = reason.as_str(),
            "websocket connected"
        ),
    }
}

/// The `ForceKeepAlive` `KeepAliveWebSocket` sends the moment a socket joins the
/// watchlist (SessionWebSocketListener.cs:168-176), before the watchdog ever
/// ticks — it is how the client learns the timeout it must answer within.
///
/// `ferrofin-core`'s listener port sends one, but that listener is not what
/// serves this route, so on the wire Ferrofin's first frame arrived 48 s late.
/// Measured against the oracle on one socket for 160 s: Jellyfin at
/// `[0.2, 48.0, 96.0, 144.1]`, Ferrofin at `[48.0, 96.1, 144.1]` — the same
/// cadence, one frame short, every run.
///
/// Only a socket with a session is greeted, as upstream: `GetSession` runs
/// before `KeepAliveWebSocket`, so a connection with no session never reaches
/// the watchlist at all. Returns whether the socket is still usable.
async fn greet_with_keep_alive(
    socket: &mut WebSocket,
    overflowed: &tokio::sync::Notify,
    has_session: bool,
) -> bool {
    if !has_session {
        return true;
    }
    send_frame(
        socket,
        Message::Text(force_keep_alive_message().into()),
        overflowed,
    )
    .await
}

/// What one watchdog tick should do about a socket, by how long it has been
/// silent — the pure half of `SessionWebSocketListener.KeepAliveSockets`'s
/// `inactive` / `lost` partition (:210-224).
#[derive(Debug, PartialEq, Eq)]
enum KeepAlive {
    /// The client spoke recently enough; say nothing.
    Quiet,
    /// `inactive` — silent past `ForceKeepAliveFactor * WebSocketLostTimeout`,
    /// so prompt it with another `ForceKeepAlive`.
    Prod,
    /// `lost` — silent past the whole `WebSocketLostTimeout`. Upstream drops it
    /// from the watchlist and disposes it (`KeepAliveSockets`, since 12.0-rc3),
    /// so the socket closes and its session ends.
    Lost,
    /// `lost`, but the socket has no session to end: it only leaves the watch.
    /// Upstream gives every socket a session (an API key's has no user, which
    /// Ferrofin ports too); a socket here is sessionless only when that failed
    /// ([`SessionlessReason`]), and such a socket was never greeted.
    Unwatch,
}

/// Classifies one watchdog tick. The two bands are half-open exactly as the C#
/// writes them: `inactive` is `elapsed > force && elapsed < timeout`, `lost` is
/// `elapsed >= timeout`, so a socket exactly at the timeout is lost, not prodded.
fn keep_alive_action(silent_secs: f64, has_session: bool) -> KeepAlive {
    if silent_secs >= KEEPALIVE_LOST_SECS {
        if has_session {
            KeepAlive::Lost
        } else {
            KeepAlive::Unwatch
        }
    } else if silent_secs > KEEPALIVE_FORCE_AFTER_SECS {
        KeepAlive::Prod
    } else {
        KeepAlive::Quiet
    }
}

/// One tick of `SessionWebSocketListener.KeepAliveSockets` for a single socket.
/// Returns whether the socket loop should continue: a lost socket with a
/// session ends it.
async fn keep_alive_watchdog(
    socket: &mut WebSocket,
    overflowed: &tokio::sync::Notify,
    last_keep_alive: tokio::time::Instant,
    has_session: bool,
    watched: &mut bool,
) -> bool {
    let silent_secs = last_keep_alive.elapsed().as_secs_f64();
    match keep_alive_action(silent_secs, has_session) {
        KeepAlive::Quiet => true,
        KeepAlive::Lost => {
            // Upstream: `Lost {0} WebSockets.` at Information.
            tracing::info!(silent_s = silent_secs, "websocket keep-alive lost; closing");
            false
        }
        KeepAlive::Unwatch => {
            *watched = false;
            tracing::debug!("sessionless websocket keep-alive lost; no longer watched");
            true
        }
        KeepAlive::Prod => {
            send_frame(
                socket,
                Message::Text(force_keep_alive_message().into()),
                overflowed,
            )
            .await
        }
    }
}

/// The armed dashboard streams of one socket.
#[derive(Default)]
struct Streams {
    sessions: Option<EventStream<SessionEvent>>,
    /// One `()` per progress report of a running task.
    tasks: Option<EventStream<()>>,
    /// The task starts and completions captured since the subscribe snapshot.
    task_updates: Option<TaskUpdateReceiver>,
    /// One `()` per created activity-log entry.
    activity: Option<EventStream<()>>,
}

impl Streams {
    /// Arms or cancels a stream for one subscription message; the task
    /// stream is [`update_task_subscription`]'s.
    fn apply(
        &mut self,
        inbound: Inbound,
        sessions: &dyn SessionManager,
        activity: &dyn ActivityManager,
    ) {
        match inbound {
            Inbound::SessionsStart(p) => {
                self.sessions = Self::event_stream(sessions.subscribe_session_events(), p);
            }
            Inbound::SessionsStop => self.sessions = None,
            Inbound::ActivityStart(p) => {
                self.activity = Self::event_stream(activity.subscribe_entry_created(), p);
            }
            Inbound::ActivityStop => self.activity = None,
            Inbound::TasksStart(_) | Inbound::TasksStop | Inbound::KeepAlive => {}
        }
    }

    /// A `*Start`'s subscription to `events` with a fresh throttle state,
    /// which replaces any earlier one. `None` (a manager that raises no
    /// events) leaves the stream unarmed, so it sends nothing.
    fn event_stream<E>(
        events: Option<tokio::sync::broadcast::Receiver<E>>,
        period: Duration,
    ) -> Option<EventStream<E>> {
        events.map(|events| EventStream {
            events,
            period,
            last_sent: None,
        })
    }
}

/// One socket's subscription to an event-driven stream: the port of the
/// `(Connection, CancellationTokenSource, WebSocketListenerState)` entry
/// `BasePeriodicWebSocketListener.Start` adds for the connection. Dropping it
/// (`*Stop`, another `*Start`, the socket closing) drops the receiver, which
/// unsubscribes, as `Stop` → `DisposeConnection` removes the entry.
struct EventStream<E> {
    events: tokio::sync::broadcast::Receiver<E>,
    /// `WebSocketListenerState.IntervalMs`: the least time between two sends
    /// of a non-forced change.
    period: Duration,
    /// `DateLastSendUtc`. `None` before the first send (upstream's
    /// `DateTime.MinValue`), so the first non-forced change sends.
    last_sent: Option<tokio::time::Instant>,
}

/// Upstream's force flag for one session event
/// (`SessionInfoWebSocketListener.cs:111-144`): `SessionActivity` is
/// `SendData(false)`, progress is `SendData(!e.IsAutomated)`, and the other
/// five are `SendData(true)`.
fn session_event_forces(event: SessionEvent) -> bool {
    !matches!(
        event,
        SessionEvent::SessionActivity | SessionEvent::PlaybackProgress { is_automated: true }
    )
}

/// What one wake of an event stream found, once its queue is drained.
#[derive(Debug, PartialEq, Eq)]
struct Wake {
    /// `Some(force)` when an event or a lag arrived: whether any of them
    /// forces a send. One snapshot answers them all, where upstream sends one
    /// per forced event; a burst yields fewer frames with the same final
    /// state (an accepted divergence).
    pending: Option<bool>,
    /// The manager dropped its sender, so nothing can arrive any more.
    closed: bool,
}

/// Drains the events queued behind `first` without waiting and ORs their
/// force flags (`forces`). A lag counts as forced, since the missed events
/// may have been: it costs one extra snapshot and loses nothing, because the
/// snapshot is rebuilt from the manager's state, never from the events.
fn drain<E: Clone>(
    first: Result<E, tokio::sync::broadcast::error::RecvError>,
    events: &mut tokio::sync::broadcast::Receiver<E>,
    forces: impl Fn(E) -> bool,
) -> Wake {
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    let mut wake = Wake {
        pending: None,
        closed: false,
    };
    let mut next = Some(first);
    while let Some(received) = next {
        match received {
            Ok(event) => wake.pending = Some(wake.pending == Some(true) || forces(event)),
            Err(RecvError::Lagged(_)) => wake.pending = Some(true),
            Err(RecvError::Closed) => {
                wake.closed = true;
                break;
            }
        }
        next = match events.try_recv() {
            Ok(event) => Some(Ok(event)),
            Err(TryRecvError::Lagged(missed)) => Some(Err(RecvError::Lagged(missed))),
            Err(TryRecvError::Closed) => Some(Err(RecvError::Closed)),
            Err(TryRecvError::Empty) => None,
        };
    }
    wake
}

/// Upstream's send-or-drop rule (`BasePeriodicWebSocketListener.cs:177`): a
/// forced change always sends; any other only when at least `period` has
/// passed since this socket's last send, and is otherwise dropped, with no
/// trailing send.
fn should_send(
    force: bool,
    last_sent: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    period: Duration,
) -> bool {
    force || last_sent.is_none_or(|sent| now.duration_since(sent) >= period)
}

/// Awaits the next event of an armed stream; pends forever when unarmed (so
/// the select branch never fires for a stream the client hasn't subscribed
/// to). `recv` is cancel-safe, so losing a `select!` race loses no event.
async fn next_event<E: Clone>(
    stream: &mut Option<EventStream<E>>,
) -> Result<E, tokio::sync::broadcast::error::RecvError> {
    match stream {
        Some(stream) => stream.events.recv().await,
        None => std::future::pending().await,
    }
}

/// One wake of an armed event stream, the port of `HandleMessages` for one
/// connection: drains the queue and applies the throttle, returning whether
/// a frame is due. A stream whose sender is gone is unarmed; a frame already
/// due is still sent.
fn wake_due<E: Clone>(
    stream: &mut Option<EventStream<E>>,
    first: Result<E, tokio::sync::broadcast::error::RecvError>,
    forces: impl Fn(E) -> bool,
) -> bool {
    let Some(subscription) = stream.as_mut() else {
        return false;
    };
    let wake = drain(first, &mut subscription.events, forces);
    let now = tokio::time::Instant::now();
    let due = wake
        .pending
        .is_some_and(|force| should_send(force, subscription.last_sent, now, subscription.period));
    if wake.closed {
        *stream = None;
    }
    due
}

/// Stamps an armed stream's `DateLastSendUtc` for a frame just built, as
/// upstream does for every send, forced or not. It is stamped just before the
/// frame is written; upstream stamps it after the write completes. A failed
/// write here ends the socket, so only the start of the throttle window moves,
/// by the time one write takes.
fn mark_sent<E>(stream: &mut Option<EventStream<E>>) {
    if let Some(subscription) = stream {
        subscription.last_sent = Some(tokio::time::Instant::now());
    }
}

/// One wake of the `Sessions` stream: the snapshot to send, if any.
async fn sessions_wake(
    state: &AppState,
    caller: Option<&SocketCaller>,
    stream: &mut Option<EventStream<SessionEvent>>,
    first: Result<SessionEvent, tokio::sync::broadcast::error::RecvError>,
) -> Option<String> {
    if !wake_due(stream, first, session_event_forces) {
        return None;
    }
    let message = sessions_message(state, caller?).await;
    if message.is_some() {
        mark_sent(stream);
    }
    message
}

/// One wake of the `ActivityLogEntry` stream, the port of
/// `ActivityLogWebSocketListener`: `OnEntryCreated` forces a send
/// (`SendData(true)`), and `GetDataToSend` is always an empty array, a signal
/// to refetch the entries over `GET /System/ActivityLog/Entries`. Entries
/// queued together are answered with one frame.
fn activity_wake(
    stream: &mut Option<EventStream<()>>,
    first: Result<(), tokio::sync::broadcast::error::RecvError>,
) -> Option<String> {
    if !wake_due(stream, first, |()| true) {
        return None;
    }
    mark_sent(stream);
    activity_log_message()
}

/// The `ActivityLogEntry` frame: always an empty array.
fn activity_log_message() -> Option<String> {
    envelope("ActivityLogEntry", &serde_json::json!([]))
}

/// One outbound envelope: `{MessageType, MessageId, Data}` with the required
/// `MessageId` (strict Kotlin-SDK clients crash without it), spelled as every
/// Jellyfin guid is (`JsonGuidConverter`: 32 hex digits). `data` is written
/// where it stands, with no second tree built around it. It stays a `Value`
/// because that is what sorts the keys inside it: `serde_json::Map` is a
/// `BTreeMap` here, and the envelope's own fields are declared in that order.
fn envelope(message_type: &str, data: &serde_json::Value) -> Option<String> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "PascalCase")]
    struct Envelope<'a> {
        data: &'a serde_json::Value,
        message_id: uuid::fmt::Simple,
        message_type: &'a str,
    }
    serde_json::to_string(&Envelope {
        data,
        message_id: uuid::Uuid::new_v4().simple(),
        message_type,
    })
    .ok()
}

/// The `Sessions` stream payload, the port of
/// `SessionInfoWebSocketListener.GetDataToSendForConnection`: an
/// administrator or API key sees every session, anyone else only the
/// sessions they are the user or an additional user of, so a session with no
/// user is not theirs. The list comes from `get_sessions`, which also blanks
/// the hardware accelerator for a non-administrator, an accepted divergence:
/// upstream's stream sends it, while its `GET /Sessions` blanks it.
///
/// A list that cannot be built is logged and skipped, and the subscription
/// stays, so the next event tries again; upstream logs it and drops the
/// connection's subscription.
async fn sessions_message(state: &AppState, caller: &SocketCaller) -> Option<String> {
    let mut sessions = match state
        .sessions
        .get_sessions(caller.user_id, None, None, None, caller.is_api_key)
        .await
    {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, "websocket Sessions snapshot could not be built");
            return None;
        }
    };
    if !caller.is_admin && !caller.is_api_key {
        sessions.retain(|session| is_session_member(session, caller.user_id));
    }
    sessions_snapshot_message(sessions)
}

/// The `Sessions` frame for `sessions`.
fn sessions_snapshot_message(sessions: Vec<ferrofin_model::dto::SessionInfoDto>) -> Option<String> {
    match serde_json::to_value(sessions) {
        Ok(data) => envelope("Sessions", &data),
        Err(error) => {
            tracing::warn!(%error, "websocket Sessions snapshot could not be serialized");
            None
        }
    }
}

/// Upstream's `s.UserId.Equals(userId) || s.ContainsUser(userId)`.
fn is_session_member(session: &ferrofin_model::dto::SessionInfoDto, user_id: uuid::Uuid) -> bool {
    session.user_id == user_id
        || session
            .additional_users
            .iter()
            .flatten()
            .any(|user| user.user_id == user_id)
}

/// The `ScheduledTasksInfo` stream payload: the current task list.
///
/// Port of `ScheduledTasksWebSocketListener.GetDataToSend`:
/// `_taskManager.ScheduledTasks.OrderBy(i => i.Name).Select(GetTaskInfo)
/// .Where(i => !i.IsHidden)` — name-ordered (which `get_tasks` already is) and
/// with hidden tasks dropped, unlike the `GET /ScheduledTasks` listing.
async fn tasks_message(state: &AppState) -> Option<String> {
    task_snapshot_message(state.tasks.get_tasks().await.ok()?)
}

fn task_snapshot_message(tasks: Vec<ferrofin_model::tasks::TaskInfo>) -> Option<String> {
    // The dashboard's task feed is distinct from per-library RefreshProgress.
    // Keep these snapshots available for debugging subscription delivery.
    for task in &tasks {
        if task.key.as_deref() == Some("RefreshLibrary")
            && task.state != ferrofin_model::tasks::TaskState::Idle
        {
            tracing::debug!(
                task = "RefreshLibrary",
                state = ?task.state,
                progress = ?task.current_progress_percentage,
                "dashboard scan task snapshot"
            );
        }
    }
    envelope(
        "ScheduledTasksInfo",
        &serde_json::to_value(visible_tasks(tasks)).ok()?,
    )
}

type TaskUpdateReceiver = tokio::sync::broadcast::Receiver<Vec<ferrofin_model::tasks::TaskInfo>>;

/// `ScheduledTasksInfoStart` and `ScheduledTasksInfoStop` for the task
/// stream, the port of `ScheduledTasksWebSocketListener`. A start subscribes
/// to the registry's transitions and progress and sends the current task
/// list at once, an accepted divergence: upstream's `Start` sends nothing
/// until a task event, but jellyfin-web's scan button has no HTTP fallback.
async fn update_task_subscription(
    state: &AppState,
    inbound: Inbound,
    streams: &mut Streams,
) -> Option<String> {
    match inbound {
        Inbound::TasksStart(period) => {
            let (message, progress) = subscribe_tasks(state, &mut streams.task_updates).await;
            streams.tasks = Streams::event_stream(progress, period);
            if message.is_some() {
                mark_sent(&mut streams.tasks);
            }
            message
        }
        Inbound::TasksStop => {
            streams.task_updates = None;
            streams.tasks = None;
            None
        }
        _ => None,
    }
}

/// Subscribes to the task registry, which takes the snapshot and the
/// subscription atomically, so a short run cannot slip between them. Returns
/// the snapshot frame and the progress signal. A registry that raises no
/// events is read instead, and leaves both streams unarmed.
async fn subscribe_tasks(
    state: &AppState,
    updates: &mut Option<TaskUpdateReceiver>,
) -> (Option<String>, Option<tokio::sync::broadcast::Receiver<()>>) {
    match state.tasks.subscribe_task_updates() {
        Some(subscription) => {
            *updates = Some(subscription.updates);
            (
                task_snapshot_message(subscription.snapshot),
                Some(subscription.progress),
            )
        }
        None => (tasks_message(state).await, None),
    }
}

/// A captured task start or completion (`TaskExecuting`, `TaskCompleted`):
/// `SendData(true)`, so it sends at once, with the snapshot captured at the
/// transition.
async fn task_transition_message(
    state: &AppState,
    update: Result<Vec<ferrofin_model::tasks::TaskInfo>, tokio::sync::broadcast::error::RecvError>,
    streams: &mut Streams,
) -> Option<String> {
    let message = match update {
        Ok(tasks) => task_snapshot_message(tasks),
        // Resubscribe atomically after lag, dropping obsolete queued
        // snapshots; the progress subscription stays as it is.
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
            subscribe_tasks(state, &mut streams.task_updates).await.0
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
            streams.task_updates = None;
            None
        }
    };
    if message.is_some() {
        mark_sent(&mut streams.tasks);
    }
    message
}

/// One wake of the task stream's progress (`OnTaskProgress`,
/// `SendData(false)`): throttled by the period, and the task list is read
/// only for a frame that is due. A captured transition still queued goes
/// first and the progress frame is dropped, checked before the read and again
/// after it: a transition may race the read, and a newer snapshot sent ahead
/// of it would show a short run out of order.
async fn task_progress_wake(
    state: &AppState,
    streams: &mut Streams,
    first: Result<(), tokio::sync::broadcast::error::RecvError>,
) -> Option<String> {
    let transitions_queued = |streams: &Streams| {
        streams
            .task_updates
            .as_ref()
            .is_some_and(|updates| !updates.is_empty())
    };
    if !wake_due(&mut streams.tasks, first, |()| false) || transitions_queued(streams) {
        return None;
    }
    let message = tasks_message(state).await;
    if transitions_queued(streams) {
        return None;
    }
    if message.is_some() {
        mark_sent(&mut streams.tasks);
    }
    message
}

async fn next_task_update(
    receiver: &mut Option<TaskUpdateReceiver>,
) -> Result<Vec<ferrofin_model::tasks::TaskInfo>, tokio::sync::broadcast::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// The listener's `.Where(i => !i.IsHidden)` clause.
///
/// The push stream drops hidden tasks; `GET /ScheduledTasks` does not (it
/// filters on `isHidden` only for configurable tasks). The incoming order is
/// already the C# `OrderBy(i => i.Name)` one and is preserved.
fn visible_tasks(
    mut tasks: Vec<ferrofin_model::tasks::TaskInfo>,
) -> Vec<ferrofin_model::tasks::TaskInfo> {
    tasks.retain(|task| !task.is_hidden);
    tasks
}

/// The `KeepAlive` ack sent in reply to a client keep-alive ping.
fn keep_alive_ack() -> String {
    let message_id = uuid::Uuid::new_v4().simple();
    format!("{{\"MessageType\":\"KeepAlive\",\"MessageId\":\"{message_id}\"}}")
}

/// The `ForceKeepAlive` message body Jellyfin's protocol uses.
///
/// Every outbound message carries a `MessageId` (C# `OutboundWebSocketMessage`
/// sets `Guid.NewGuid()`). It is `format: uuid` and *required* by strict
/// clients: without it the Jellyfin Kotlin SDK throws `MissingFieldException`
/// and the Android TV app crashes mid-playback. It is written the way Jellyfin
/// writes every guid — 32 hex digits, no dashes (`JsonGuidConverter`); the
/// SDK's `UUIDSerializer` exists precisely to read that form.
fn force_keep_alive_message() -> String {
    let message_id = uuid::Uuid::new_v4().simple();
    format!(
        "{{\"MessageType\":\"ForceKeepAlive\",\"MessageId\":\"{message_id}\",\"Data\":{KEEPALIVE_SECS}}}"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Action, DEFAULT_STREAM_MILLIS, Inbound, KEEPALIVE_FORCE_AFTER_SECS, KEEPALIVE_LOST_SECS,
        KEEPALIVE_SECS, KeepAlive, PUSH_QUEUE_DEPTH, SessionEvent, SocketCaller, Streams, Wake,
        action_for, drain, force_keep_alive_message, keep_alive_ack, keep_alive_action,
        may_subscribe, parse_inbound, push_sink, session_event_forces, should_send, visible_tasks,
    };
    use crate::test_support::{FakeActivity, FakeSessions};
    use axum::extract::ws::Message;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// A subscription keeps the period the client asked for, however small:
    /// upstream's `IntervalMs` has no floor (`event_stream`, which every
    /// stream's subscription goes through).
    #[tokio::test]
    async fn stream_period_has_no_floor() {
        for period in [Duration::ZERO, Duration::from_millis(1)] {
            let (_events, receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
            let stream = Streams::event_stream(Some(receiver), period).unwrap();
            assert_eq!(stream.period, period);
        }
    }

    /// A second `SessionsStart` replaces the first: a new receiver, the new
    /// period and a fresh throttle, so its first non-forced change sends.
    /// `SessionsStop` drops the receiver. A manager that raises no events
    /// leaves the stream unarmed.
    #[tokio::test]
    async fn a_second_sessions_start_replaces_the_subscription() {
        let mut streams = Streams::default();
        let (events, receiver) = tokio::sync::broadcast::channel::<SessionEvent>(1);
        streams.sessions = Streams::event_stream(Some(receiver), Duration::from_millis(1500));
        streams.sessions.as_mut().unwrap().last_sent = Some(tokio::time::Instant::now());

        streams.sessions = Streams::event_stream(Some(events.subscribe()), Duration::from_secs(5));
        let subscription = streams.sessions.as_ref().unwrap();
        assert_eq!(subscription.period, Duration::from_secs(5));
        assert_eq!(subscription.last_sent, None);
        assert_eq!(events.receiver_count(), 1, "the first receiver is gone");

        streams.apply(Inbound::SessionsStop, &FakeSessions, &FakeActivity);
        assert!(streams.sessions.is_none());
        assert_eq!(events.receiver_count(), 0);

        streams.apply(
            Inbound::SessionsStart(Duration::from_secs(1)),
            &FakeSessions,
            &FakeActivity,
        );
        assert!(streams.sessions.is_none(), "FakeSessions raises no events");
    }

    /// `SessionInfoWebSocketListener.cs:111-144`: only `SessionActivity` and
    /// automated progress are throttled; every other event forces a send.
    #[test]
    fn only_session_activity_and_automated_progress_are_throttled() {
        for (event, forced) in [
            (SessionEvent::SessionStarted, true),
            (SessionEvent::SessionEnded, true),
            (SessionEvent::PlaybackStart, true),
            (SessionEvent::PlaybackStopped, true),
            (
                SessionEvent::PlaybackProgress {
                    is_automated: false,
                },
                true,
            ),
            (SessionEvent::PlaybackProgress { is_automated: true }, false),
            (SessionEvent::CapabilitiesChanged, true),
            (SessionEvent::SessionActivity, false),
        ] {
            assert_eq!(session_event_forces(event), forced, "{event:?}");
        }
    }

    /// `BasePeriodicWebSocketListener.cs:177`: `force || (now -
    /// DateLastSendUtc) >= IntervalMs`, with `DateLastSendUtc` unset before the
    /// first send.
    #[test]
    fn a_throttled_change_sends_only_once_the_period_has_passed() {
        let period = Duration::from_millis(1500);
        let sent = tokio::time::Instant::now();
        assert!(should_send(false, None, sent, period), "nothing sent yet");
        assert!(should_send(true, Some(sent), sent, period), "forced");
        assert!(!should_send(
            false,
            Some(sent),
            sent + Duration::from_millis(1499),
            period
        ));
        assert!(should_send(false, Some(sent), sent + period, period));
    }

    /// One wake drains everything queued and ORs the force flags; a lag is
    /// forced; a closed channel says so once the queue is empty.
    #[tokio::test]
    async fn a_wake_drains_the_queue_into_one_decision() {
        use tokio::sync::broadcast::error::RecvError;
        let throttled = SessionEvent::SessionActivity;
        let (events, mut receiver) = tokio::sync::broadcast::channel(2);

        events.send(throttled).unwrap();
        let first = receiver.recv().await;
        let wake = drain(first, &mut receiver, session_event_forces);
        assert_eq!(
            wake,
            Wake {
                pending: Some(false),
                closed: false
            }
        );

        events.send(throttled).unwrap();
        events.send(SessionEvent::PlaybackStart).unwrap();
        let first = receiver.recv().await;
        let wake = drain(first, &mut receiver, session_event_forces);
        assert_eq!(wake.pending, Some(true), "a queued forced event forces");
        assert!(receiver.is_empty(), "the queue is drained");

        for _ in 0..3 {
            events.send(throttled).unwrap();
        }
        let first = receiver.recv().await;
        assert!(matches!(first, Err(RecvError::Lagged(1))), "{first:?}");
        let wake = drain(first, &mut receiver, session_event_forces);
        assert_eq!(wake.pending, Some(true), "a lag is forced");

        events.send(throttled).unwrap();
        drop(events);
        let first = receiver.recv().await;
        let wake = drain(first, &mut receiver, session_event_forces);
        assert_eq!(
            wake,
            Wake {
                pending: Some(false),
                closed: true
            }
        );
    }

    /// A progress frame is dropped while a captured transition is queued, so
    /// a short run never shows out of order; once the transition is sent,
    /// progress sends again. `ScheduledTasksInfoStop` drops both receivers.
    /// The period is zero, so the throttle never holds a frame back here.
    #[tokio::test]
    async fn captured_transitions_precede_progress_snapshots_and_stop_unsubscribes() {
        use crate::test_support::{
            FakeLibrary, RecordingTasks, elevated_state_with_library_monitor_and_tasks,
        };
        use std::sync::Arc;
        let state = elevated_state_with_library_monitor_and_tasks(
            Arc::new(FakeLibrary),
            Arc::new(ferrofin_traits::stubs::NoopLibraryMonitor),
            Arc::new(RecordingTasks::default()),
        );
        let mut streams = Streams::default();
        let (transitions, receiver) = tokio::sync::broadcast::channel(4);
        let (progress, progress_receiver) = tokio::sync::broadcast::channel(4);
        streams.task_updates = Some(receiver);
        streams.tasks = Streams::event_stream(Some(progress_receiver), Duration::ZERO);

        transitions.send(Vec::new()).unwrap();
        progress.send(()).unwrap();
        let first = super::next_event(&mut streams.tasks).await;
        assert!(
            super::task_progress_wake(&state, &mut streams, first)
                .await
                .is_none(),
            "the queued transition goes first"
        );
        let update = super::next_task_update(&mut streams.task_updates).await;
        assert!(
            super::task_transition_message(&state, update, &mut streams)
                .await
                .is_some()
        );
        progress.send(()).unwrap();
        let first = super::next_event(&mut streams.tasks).await;
        assert!(
            super::task_progress_wake(&state, &mut streams, first)
                .await
                .is_some()
        );

        super::update_task_subscription(&state, Inbound::TasksStop, &mut streams).await;
        assert!(streams.task_updates.is_none() && streams.tasks.is_none());
        assert_eq!(transitions.receiver_count(), 0);
        assert_eq!(progress.receiver_count(), 0);
    }

    /// A client that stops reading must not make the server buffer without
    /// bound: the sink accepts exactly `PUSH_QUEUE_DEPTH` messages, then drops
    /// the rest and raises the overflow signal that closes the socket.
    #[tokio::test]
    async fn a_client_that_never_drains_bounds_the_queue_and_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<String>(PUSH_QUEUE_DEPTH);
        let overflowed = std::sync::Arc::new(tokio::sync::Notify::new());
        let sink = push_sink(tx, std::sync::Arc::clone(&overflowed));

        // Fill the queue exactly; nothing is dropped and no overflow is raised.
        for i in 0..PUSH_QUEUE_DEPTH {
            sink(format!("msg-{i}"));
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), overflowed.notified())
                .await
                .is_err(),
            "a full-but-not-overflowing queue must not close the socket",
        );

        // The next 10_000 messages neither block nor grow the queue.
        for i in 0..10_000 {
            sink(format!("overflow-{i}"));
        }
        tokio::time::timeout(std::time::Duration::from_millis(50), overflowed.notified())
            .await
            .expect("overflow raises the close signal");

        let mut drained = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            drained.push(msg);
        }
        assert_eq!(
            drained.len(),
            PUSH_QUEUE_DEPTH,
            "queue is capped at the configured depth, whatever the client is sent",
        );
        assert_eq!(drained[0], "msg-0", "the oldest queued messages are kept");
    }

    /// The ordinary "socket already gone" race: a closed receiver is swallowed,
    /// not reported as an overflow (which would log a warning on every close).
    #[tokio::test]
    async fn a_closed_receiver_is_swallowed_without_signalling_overflow() {
        let (tx, rx) = mpsc::channel::<String>(PUSH_QUEUE_DEPTH);
        drop(rx);
        let overflowed = std::sync::Arc::new(tokio::sync::Notify::new());
        let sink = push_sink(tx, std::sync::Arc::clone(&overflowed));
        sink("gone".to_owned());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), overflowed.notified())
                .await
                .is_err(),
            "a closed socket is not an overflow",
        );
    }

    /// Only the activity stream is an administrator's (`ActivityLogWebSocketListener`
    /// is the one listener whose `Start` demands a role); the gate must not reach wider.
    #[test]
    fn only_the_activity_stream_needs_an_administrator() {
        let caller = |is_admin| SocketCaller {
            session_id: "s".to_owned(),
            user_id: uuid::Uuid::nil(),
            is_admin,
            is_api_key: false,
        };
        let period = std::time::Duration::from_secs(1);
        assert!(!may_subscribe(
            &caller(false),
            Inbound::ActivityStart(period)
        ));
        for inbound in [
            Inbound::ActivityStart(period),
            Inbound::ActivityStop,
            Inbound::SessionsStart(period),
            Inbound::TasksStart(period),
        ] {
            assert!(may_subscribe(&caller(true), inbound), "{inbound:?}");
            if inbound != Inbound::ActivityStart(period) {
                assert!(may_subscribe(&caller(false), inbound), "{inbound:?}");
            }
        }
        // An API key is elevated, so `ActivityLogWebSocketListener` admits it.
        let api_key = SocketCaller {
            is_admin: true,
            is_api_key: true,
            ..caller(true)
        };
        assert!(may_subscribe(&api_key, Inbound::ActivityStart(period)));
    }

    #[test]
    fn ping_is_ponged_with_same_payload() {
        let a = action_for(Some(Ok(Message::Ping(axum::body::Bytes::from_static(
            b"hi",
        )))));
        match a {
            Action::Pong(p) => assert_eq!(&p[..], b"hi"),
            _ => panic!("ping should pong"),
        }
    }

    #[test]
    fn close_end_and_error_stop_the_loop() {
        assert!(matches!(
            action_for(Some(Ok(Message::Close(None)))),
            Action::Stop
        ));
        assert!(matches!(action_for(None), Action::Stop));
    }

    #[test]
    fn unknown_text_and_binary_are_ignored() {
        assert!(matches!(
            action_for(Some(Ok(Message::Text("{}".into())))),
            Action::Ignore
        ));
        assert!(matches!(
            action_for(Some(Ok(Message::Text("not json".into())))),
            Action::Ignore
        ));
        assert!(matches!(
            action_for(Some(Ok(Message::Text(
                r#"{"MessageType":"SomethingElse"}"#.into()
            )))),
            Action::Ignore
        ));
        assert!(matches!(
            action_for(Some(Ok(Message::Binary(axum::body::Bytes::new())))),
            Action::Ignore
        ));
    }

    #[test]
    fn inbound_protocol_messages_are_recognized() {
        use std::time::Duration;
        assert_eq!(
            parse_inbound(r#"{"MessageType":"KeepAlive"}"#),
            Some(Inbound::KeepAlive)
        );
        // jellyfin-web subscribes with "initialDelay,period" in milliseconds.
        assert_eq!(
            parse_inbound(r#"{"MessageType":"SessionsStart","Data":"0,1500"}"#),
            Some(Inbound::SessionsStart(Duration::from_millis(1500)))
        );
        assert_eq!(
            parse_inbound(r#"{"MessageType":"SessionsStop","Data":""}"#),
            Some(Inbound::SessionsStop)
        );
        assert_eq!(
            parse_inbound(r#"{"MessageType":"ScheduledTasksInfoStart","Data":"1000,1000"}"#),
            Some(Inbound::TasksStart(Duration::from_secs(1)))
        );
        assert_eq!(
            parse_inbound(r#"{"MessageType":"ActivityLogEntryStart","Data":"0,1000"}"#),
            Some(Inbound::ActivityStart(Duration::from_secs(1)))
        );
        // No floor: upstream's IntervalMs of 0, or below, throttles nothing.
        for data in ["0,0", "0,-5"] {
            assert_eq!(
                parse_inbound(&format!(
                    r#"{{"MessageType":"SessionsStart","Data":"{data}"}}"#
                )),
                Some(Inbound::SessionsStart(Duration::ZERO)),
                "{data}"
            );
        }
        // A period that does not parse falls back to the default; the initial
        // delay is never read.
        for (data, millis) in [
            ("0,abc", DEFAULT_STREAM_MILLIS),
            ("0,", DEFAULT_STREAM_MILLIS),
            ("0,99999999999999999999", DEFAULT_STREAM_MILLIS),
            ("abc,500", 500),
        ] {
            assert_eq!(
                parse_inbound(&format!(
                    r#"{{"MessageType":"SessionsStart","Data":"{data}"}}"#
                )),
                Some(Inbound::SessionsStart(Duration::from_millis(millis))),
                "{data}"
            );
        }
        // Malformed or missing Data falls back to the default period.
        assert_eq!(
            parse_inbound(r#"{"MessageType":"SessionsStart"}"#),
            Some(Inbound::SessionsStart(Duration::from_millis(
                DEFAULT_STREAM_MILLIS
            )))
        );
        assert_eq!(
            parse_inbound(r#"{"MessageType":"SessionsStart","Data":"junk"}"#),
            Some(Inbound::SessionsStart(Duration::from_millis(
                DEFAULT_STREAM_MILLIS
            )))
        );
    }

    #[test]
    fn inbound_member_names_are_exact_but_the_message_type_value_is_not() {
        // `JsonDefaults.Options`: no member-name fold…
        assert_eq!(parse_inbound(r#"{"messageType":"KeepAlive"}"#), None);
        // …but `JsonStringEnumConverter` reads the enum value ignoring case.
        assert_eq!(
            parse_inbound(r#"{"MessageType":"keepalive"}"#),
            Some(Inbound::KeepAlive)
        );
        assert_eq!(
            parse_inbound(r#"{"MessageType":"SESSIONSSTOP"}"#),
            Some(Inbound::SessionsStop)
        );
    }

    #[test]
    fn keep_alive_ack_is_valid_json_with_a_message_id() {
        let v: serde_json::Value = serde_json::from_str(&keep_alive_ack()).unwrap();
        assert_eq!(v["MessageType"], "KeepAlive");
        // Jellyfin's guid spelling: 32 hex digits, no dashes.
        let id = v["MessageId"].as_str().unwrap();
        assert_eq!(id.len(), 32);
        assert!(uuid::Uuid::try_parse(id).is_ok());
    }

    #[test]
    fn keep_alive_message_is_valid_json_with_the_interval() {
        let m = force_keep_alive_message();
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["MessageType"], "ForceKeepAlive");
        assert_eq!(v["Data"], KEEPALIVE_SECS);
        // The SDK requires `MessageId` (`format: uuid`) or the Android client
        // crashes with MissingFieldException. Spelled as Jellyfin spells every
        // guid (32 hex digits) — the SDK's `UUIDSerializer` reads that form.
        let id = v["MessageId"].as_str().expect("MessageId present");
        assert!(
            uuid::Uuid::try_parse(id).is_ok(),
            "MessageId is a UUID: {id}"
        );
        assert_eq!(id.len(), 32, "Jellyfin's dash-less form");
    }

    /// A bus whose `unregister` answers a canned "a sink still remains", and
    /// records what it was asked to remove.
    struct FakeBus {
        still_connected: bool,
        removed: std::sync::Mutex<Vec<(String, ferrofin_traits::session_bus::SinkToken)>>,
    }

    impl ferrofin_traits::session_bus::SessionMessageBus for FakeBus {
        fn register(
            &self,
            _session_id: String,
            _sink: ferrofin_traits::session_bus::MessageSink,
        ) -> ferrofin_traits::session_bus::SinkToken {
            0
        }
        fn unregister(
            &self,
            session_id: &str,
            token: ferrofin_traits::session_bus::SinkToken,
        ) -> bool {
            self.removed
                .lock()
                .expect("fake bus mutex")
                .push((session_id.to_owned(), token));
            self.still_connected
        }
        fn send(&self, _session_id: &str, _message: String) -> bool {
            false
        }
        fn is_connected(&self, _session_id: &str) -> bool {
            self.still_connected
        }
    }

    #[tokio::test]
    async fn last_socket_close_ends_the_session() {
        let bus = FakeBus {
            still_connected: false,
            removed: std::sync::Mutex::new(Vec::new()),
        };
        let ended = std::sync::Mutex::new(Vec::new());
        let did_end = super::end_session_if_last_socket(&bus, "sess-1", 7, || async {
            ended.lock().expect("ended mutex").push("sess-1");
            Ok(())
        })
        .await;

        assert!(did_end, "no socket remained, so the session must end");
        assert_eq!(*ended.lock().expect("ended mutex"), vec!["sess-1"]);
        assert_eq!(
            *bus.removed.lock().expect("fake bus mutex"),
            vec![("sess-1".to_owned(), 7)],
            "the closing socket removes its own registration by token"
        );
    }

    /// The last socket's close ends the session through `close_if_needed` (C#
    /// `CloseIfNeededAsync`, which releases the session's live stream first),
    /// never through `report_session_ended`, which is a logout's and releases
    /// nothing. `FakeSessions` answers `close_if_needed` and leaves
    /// `report_session_ended` unimplemented, so the panic is the assertion:
    /// the test passes only if the close never reports a logout.
    #[tokio::test]
    async fn the_last_socket_close_closes_the_session_rather_than_logging_it_out() {
        let state = crate::test_support::fake_state();
        let bus = std::sync::Arc::new(FakeBus {
            still_connected: false,
            removed: std::sync::Mutex::new(Vec::new()),
        });
        let registration: super::Registration = (
            "sess-1".to_owned(),
            bus.clone() as std::sync::Arc<dyn ferrofin_traits::session_bus::SessionMessageBus>,
            7,
        );
        super::unregister_socket(&state, Some(registration)).await;
        assert_eq!(
            *bus.removed.lock().expect("fake bus mutex"),
            vec![("sess-1".to_owned(), 7)]
        );
    }

    /// The C# guard `if (!session.SessionControllers.Any(i => i.IsSessionActive))`:
    /// a stale socket closing after the client reconnected must not end the
    /// session out from under the live one.
    #[tokio::test]
    async fn a_reconnect_keeps_the_session_alive() {
        let bus = FakeBus {
            still_connected: true,
            removed: std::sync::Mutex::new(Vec::new()),
        };
        let ended = std::sync::Mutex::new(Vec::new());
        let did_end = super::end_session_if_last_socket(&bus, "sess-1", 7, || async {
            ended.lock().expect("ended mutex").push("sess-1");
            Ok(())
        })
        .await;

        assert!(!did_end, "a reconnected socket still holds the session");
        assert!(
            ended.lock().expect("ended mutex").is_empty(),
            "the live session must not be ended"
        );
    }
    /// `ScheduledTasksWebSocketListener.GetDataToSend` ends in
    /// `.Where(i => !i.IsHidden)`, so the push stream — unlike
    /// `GET /ScheduledTasks` — never carries a hidden task, and it keeps the
    /// name order it was given.
    #[test]
    fn the_task_push_drops_hidden_tasks_and_keeps_the_order() {
        let task = |name: &str, hidden: bool| ferrofin_model::tasks::TaskInfo {
            name: Some(name.to_owned()),
            is_hidden: hidden,
            ..ferrofin_model::tasks::TaskInfo::default()
        };
        let visible = visible_tasks(vec![
            task("Clean Activity Log", false),
            task("TasksRefreshChannels", true),
            task("Scan Media Library", false),
        ]);
        assert_eq!(
            visible
                .iter()
                .filter_map(|t| t.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Clean Activity Log", "Scan Media Library"]
        );
    }

    /// A frame with its random `MessageId` taken out, after checking that it is
    /// the 32 hex digits every Jellyfin guid is written in.
    fn without_message_id(frame: &str) -> String {
        let (head, rest) = frame
            .split_once(r#""MessageId":""#)
            .expect("a frame carries a MessageId");
        let (id, tail) = rest.split_once('"').expect("the id is a string");
        assert_eq!(id.len(), 32, "{frame}");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{frame}");
        format!("{head}\"MessageId\":\"\"{tail}")
    }

    /// The exact bytes the three streams send: keys sorted at every level,
    /// `Data` first, as `serde_json::Map` has always ordered them here.
    #[test]
    fn the_stream_frames_keep_their_bytes() {
        let session = ferrofin_model::dto::SessionInfoDto {
            id: Some("session-1".to_owned()),
            user_name: Some("Zoë \"Z\"".to_owned()),
            play_state: Some(ferrofin_model::session::PlayerStateInfo {
                position_ticks: Some(5),
                can_seek: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            without_message_id(&super::sessions_snapshot_message(vec![session]).expect("a frame")),
            concat!(
                r#"{"Data":[{"HasCustomDeviceName":false,"Id":"session-1","IsActive":false,"#,
                r#""LastActivityDate":"1970-01-01T00:00:00.0000000Z","#,
                r#""LastPlaybackCheckIn":"1970-01-01T00:00:00.0000000Z","#,
                r#""PlayState":{"CanSeek":true,"IsMuted":false,"IsPaused":false,"#,
                r#""PlaybackOrder":"Default","PositionTicks":5,"RepeatMode":"RepeatNone"},"#,
                r#""PlayableMediaTypes":[],"SupportedCommands":[],"#,
                r#""SupportsMediaControl":false,"SupportsRemoteControl":false,"#,
                r#""UserId":"00000000000000000000000000000000","UserName":"Zoë \"Z\""}],"#,
                r#""MessageId":"","MessageType":"Sessions"}"#,
            )
        );

        let task = ferrofin_model::tasks::TaskInfo {
            name: Some("Scan Media Library".to_owned()),
            key: Some("RefreshLibrary".to_owned()),
            id: Some("task-1".to_owned()),
            current_progress_percentage: Some(12.5),
            ..Default::default()
        };
        assert_eq!(
            without_message_id(&super::task_snapshot_message(vec![task]).expect("a frame")),
            concat!(
                r#"{"Data":[{"CurrentProgressPercentage":12.5,"Id":"task-1","IsHidden":false,"#,
                r#""Key":"RefreshLibrary","Name":"Scan Media Library","State":"Idle","#,
                r#""Triggers":[]}],"MessageId":"","MessageType":"ScheduledTasksInfo"}"#,
            )
        );

        assert_eq!(
            without_message_id(&super::activity_log_message().expect("a frame")),
            r#"{"Data":[],"MessageId":"","MessageType":"ActivityLogEntry"}"#
        );
    }

    /// The watchdog's bands, against the C# factors of `WebSocketLostTimeout`.
    /// A flat metronome — what this loop ran before — would answer `Prod` for
    /// every one of these.
    #[test]
    fn the_keep_alive_watchdog_prods_only_a_silent_socket() {
        // `IntervalFactor * WebSocketLostTimeout`: the watchdog wakes far more
        // often than it speaks, so most ticks must be silent.
        for quiet in [0.0, 1.0, 12.0, 24.0, 44.9, KEEPALIVE_FORCE_AFTER_SECS] {
            assert_eq!(keep_alive_action(quiet, true), KeepAlive::Quiet, "{quiet}");
        }
        for prod in [45.1, 48.0, 59.9] {
            assert_eq!(keep_alive_action(prod, true), KeepAlive::Prod, "{prod}");
        }
        // `elapsed >= WebSocketLostTimeout` is `lost`, not one last prod: a
        // socket with a session closes, a sessionless one only leaves the watch.
        for lost in [KEEPALIVE_LOST_SECS, 60.1, 600.0] {
            assert_eq!(keep_alive_action(lost, true), KeepAlive::Lost, "{lost}");
            assert_eq!(keep_alive_action(lost, false), KeepAlive::Unwatch, "{lost}");
        }
        // ...and the bands are the C# factors of the one timeout the client is
        // told about in the `ForceKeepAlive` payload.
        assert_eq!(KEEPALIVE_SECS, 60);
        assert!((KEEPALIVE_FORCE_AFTER_SECS - 60.0 * 0.75).abs() < f64::EPSILON);
        assert!((KEEPALIVE_LOST_SECS - 60.0).abs() < f64::EPSILON);
    }
    #[test]
    fn inbound_enum_numbers_use_csharp_values_and_exact_property_names() {
        use ferrofin_model::session::SessionMessageType as Kind;
        for value in [
            Kind::KeepAlive.json_value().to_string(),
            format!("\" {} \"", Kind::KeepAlive.json_value()),
        ] {
            let text = format!(r#"{{"MessageType":{value}}}"#);
            assert_eq!(parse_inbound(&text), Some(Inbound::KeepAlive));
        }
        let message = format!(
            r#"{{"MessageType":{},"Data":"0,2345"}}"#,
            Kind::SessionsStart.json_value()
        );
        assert_eq!(
            parse_inbound(&message),
            Some(Inbound::SessionsStart(std::time::Duration::from_millis(
                2345
            )))
        );
        for text in [
            r#"{"MessageType":28.0}"#,
            r#"{"MessageType":"28.0"}"#,
            r#"{"MessageType":999}"#,
            r#"{"messageType":28}"#,
            r#"{"MessageType":true}"#,
        ] {
            assert_eq!(parse_inbound(text), None, "{text}");
        }
    }
}
