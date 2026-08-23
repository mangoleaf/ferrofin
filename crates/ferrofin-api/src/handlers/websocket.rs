//! The session WebSocket (`/socket`, and the legacy `/embywebsocket` alias).
//!
//! jellyfin-web opens a WebSocket immediately after authenticating and treats it
//! as *the* server connection — if it never establishes, the client reports
//! "Connection Failure" and refuses to enter the dashboard, even though the REST
//! calls all succeeded. These routes are NOT part of the OpenAPI contract (Swagger
//! doesn't describe WebSocket endpoints), so they are registered as extras.
//!
//! The socket accepts the upgrade, answers pings, sends periodic keep-alives,
//! and — when the caller authenticates via the `api_key` query parameter —
//! **registers a message sink** on the [`SessionMessageBus`] keyed by the
//! caller's session id, so server→client pushes (SyncPlay commands/group updates
//! and the remote-control `Play`/`Playstate`/`GeneralCommand` casts) reach this
//! client. The session manager also treats a bus-registered session as having a
//! live controller, which is what makes it appear in the cast-to-device menu
//! (`GET /Sessions?ControllableByUserId=…` → `SupportsRemoteControl`). The sink
//! is unregistered when the socket closes. An anonymous socket still holds open (keep-alive only), so a client
//! that opens the socket before authenticating is never dropped.
//!
//! Closing the socket also **ends the session** when no other socket took its
//! place -- the port of `WebSocketController.OnConnectionClosed` ->
//! `SessionManager.CloseIfNeededAsync`, which is how upstream keeps
//! `_activeConnections` from accumulating one entry per client that ever
//! connected. Without it a closed browser tab stays in `GET /Sessions` forever
//! and its `SessionEnded` activity entry is never written.

use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use tokio::sync::mpsc;

use crate::state::AppState;

/// The server keep-alive interval advertised to the client, in seconds.
const KEEPALIVE_SECS: u64 = 60;

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
/// jellyfin-web passes the access token as the `api_key` query parameter. We
/// resolve it to the caller's session id (best-effort; an anonymous socket still
/// upgrades) so the socket can register a delivery sink for server→client pushes.
async fn websocket_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let caller = resolve_caller(&state, &headers, query.as_deref()).await;
    ws.read_buffer_size(READ_BUFFER_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, caller))
}

/// The resolved identity of an authenticated socket: its session id (the bus
/// sink key) and the signed-in user (who the subscription streams answer for).
#[derive(Clone)]
struct SocketCaller {
    session_id: String,
    user_id: uuid::Uuid,
}

/// Why a socket ended up anonymous — kept distinct so the logs can tell
/// "client never sent a token" (expected for pre-auth clients) from "a token
/// was sent but didn't resolve" (bad client input, or a server bug that would
/// silently drop SyncPlay/push delivery).
#[derive(Clone, Copy)]
enum AnonymousReason {
    /// No `api_key`/`ApiKey` query parameter and no token header.
    NoToken,
    /// A token was presented but the session manager could not resolve it.
    TokenUnresolved,
    /// The token resolved to a session that carries no session id.
    SessionWithoutId,
}

impl AnonymousReason {
    /// The value logged in the `reason` field.
    fn as_str(self) -> &'static str {
        match self {
            Self::NoToken => "no-token",
            Self::TokenUnresolved => "token-unresolved",
            Self::SessionWithoutId => "session-without-id",
        }
    }
}

/// Resolves the caller's session id from the socket's access token, or the
/// reason the socket is anonymous.
///
/// The WebSocket carries its token as the `api_key`/`ApiKey` query parameter (the
/// Jellyfin convention) or, failing that, an `X-Emby-Token`/`X-MediaBrowser-Token`
/// header. We resolve it straight through the session manager
/// ([`get_session_by_authentication_token`]) rather than the general
/// [`AuthService`], because the latter gates the lowercase `api_key` query
/// parameter behind legacy-authorization config — which would silently drop the
/// socket's session and its SyncPlay delivery. Passing an empty `device_id` lets
/// the manager key the session off the token's own device, so the derived id
/// matches the one the `/SyncPlay/*` handlers compute for the same client.
///
/// [`get_session_by_authentication_token`]: ferrofin_traits::session::SessionManager::get_session_by_authentication_token
/// [`AuthService`]: ferrofin_traits::net::AuthService
async fn resolve_caller(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<SocketCaller, AnonymousReason> {
    let token = query_param(query, "api_key")
        .or_else(|| query_param(query, "ApiKey"))
        .or_else(|| header_token(headers))
        .ok_or(AnonymousReason::NoToken)?;
    let session = state
        .sessions
        .get_session_by_authentication_token(&token, "", "")
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "websocket token did not resolve to a session");
            AnonymousReason::TokenUnresolved
        })?;
    let session_id = session.id.ok_or(AnonymousReason::SessionWithoutId)?;
    Ok(SocketCaller {
        session_id,
        user_id: session.user_id,
    })
}

/// Reads a bare-token header (`X-Emby-Token` / `X-MediaBrowser-Token`).
fn header_token(headers: &HeaderMap) -> Option<String> {
    for name in ["x-emby-token", "x-mediabrowser-token"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
        {
            return Some(value.to_owned());
        }
    }
    None
}

/// Extracts a query-string parameter value by exact key (no percent-decoding —
/// access tokens are URL-safe hex).
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_owned())
    })
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
    /// `SessionsStart` — stream the session list every `period`.
    SessionsStart(Duration),
    /// `SessionsStop` — stop the session stream.
    SessionsStop,
    /// `ScheduledTasksInfoStart` — stream the task list every `period`.
    TasksStart(Duration),
    /// `ScheduledTasksInfoStop` — stop the task stream.
    TasksStop,
    /// `ActivityLogEntryStart` — stream new activity entries every `period`.
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
fn parse_inbound(text: &str) -> Option<Inbound> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let period = || subscription_period(value.get("Data").and_then(|d| d.as_str()));
    match value.get("MessageType")?.as_str()? {
        "KeepAlive" => Some(Inbound::KeepAlive),
        "SessionsStart" => Some(Inbound::SessionsStart(period())),
        "SessionsStop" => Some(Inbound::SessionsStop),
        "ScheduledTasksInfoStart" => Some(Inbound::TasksStart(period())),
        "ScheduledTasksInfoStop" => Some(Inbound::TasksStop),
        "ActivityLogEntryStart" => Some(Inbound::ActivityStart(period())),
        "ActivityLogEntryStop" => Some(Inbound::ActivityStop),
        _ => None,
    }
}

/// The default stream period when the subscription's `Data` is absent or
/// malformed — jellyfin-web subscribes with "0,1500".
const DEFAULT_STREAM_MILLIS: u64 = 1500;

/// Parses the `"initialDelayMs,periodMs"` subscription payload (the C#
/// `BasePeriodicWebSocketListener` Data convention) into the stream period.
/// The initial delay is folded into the period (first send after one period).
fn subscription_period(data: Option<&str>) -> Duration {
    let millis = data
        .and_then(|d| d.split(',').nth(1))
        .and_then(|p| p.trim().parse::<u64>().ok())
        .filter(|&p| p > 0)
        .unwrap_or(DEFAULT_STREAM_MILLIS);
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

/// Registers an authenticated socket's push sink on the session bus, if the
/// caller resolved to a session and a bus is wired. An anonymous socket (or a
/// server with no bus) gets no registration and simply never receives pushes.
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

/// Holds a WebSocket open: register the caller's push sink (if authenticated),
/// answer pings, forward server→client pushes, send a periodic keep-alive, and
/// close cleanly — unregistering the sink — when the peer goes away.
#[tracing::instrument(
    name = "ws_session",
    skip_all,
    fields(session_id = caller.as_ref().map_or("anonymous", |c| c.session_id.as_str()))
)]
async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    caller: Result<SocketCaller, AnonymousReason>,
) {
    let started = std::time::Instant::now();
    // `tx` feeds this socket; the bus holds a clone as the session's delivery
    // sink. Keeping `tx` alive here also keeps `rx` open for anonymous sockets
    // (no sink registered) so the forward branch stays pending rather than
    // closing the loop. The channel is bounded (see `PUSH_QUEUE_DEPTH`) so a
    // client that stops reading cannot make the server buffer without limit.
    let (tx, mut rx) = mpsc::channel::<String>(PUSH_QUEUE_DEPTH);
    // Raised by the sink when the queue is full; the loop closes the socket.
    let overflowed = std::sync::Arc::new(tokio::sync::Notify::new());
    let registration = register_sink(&state, caller.as_ref().ok(), &tx, &overflowed);
    match &caller {
        Ok(_) => tracing::info!(
            authenticated = registration.is_some(),
            "websocket connected"
        ),
        Err(reason) => tracing::info!(
            authenticated = false,
            reason = reason.as_str(),
            "websocket connected"
        ),
    }
    let caller = caller.ok();

    let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
    keepalive.tick().await; // consume the immediate first tick

    // The dashboard's periodic streams, armed by *Start subscription messages
    // (each is `None` until subscribed). Only an authenticated socket may
    // subscribe — the streams answer as the socket's user.
    let mut streams = Streams::default();

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
                    // Ack the client's ping (C# `SendKeepAliveResponse`).
                    let ack = Message::Text(keep_alive_ack().into());
                    if !send_frame(&mut socket, ack, &overflowed).await {
                        break;
                    }
                }
                Action::Inbound(inbound) => {
                    if caller.is_some() {
                        streams.apply(inbound);
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
            _ = keepalive.tick() => {
                // Jellyfin's `ForceKeepAlive`: tells the client the keep-alive interval.
                let msg = Message::Text(force_keep_alive_message().into());
                if !send_frame(&mut socket, msg, &overflowed).await {
                    break;
                }
            }
            () = tick(&mut streams.sessions) => {
                if let Some(c) = caller.as_ref()
                    && let Some(msg) = sessions_message(&state, c.user_id).await
                    && !send_frame(&mut socket, Message::Text(msg.into()), &overflowed).await
                {
                    break;
                }
            }
            () = tick(&mut streams.tasks) => {
                if let Some(msg) = tasks_message(&state).await
                    && !send_frame(&mut socket, Message::Text(msg.into()), &overflowed).await
                {
                    break;
                }
            }
            () = tick(&mut streams.activity) => {
                if let Some(msg) = activity_message(&state, &mut streams.activity_since).await
                    && !send_frame(&mut socket, Message::Text(msg.into()), &overflowed).await
                {
                    break;
                }
            }
        }
    }

    if let Some((sid, bus, token)) = registration {
        end_session_if_last_socket(bus.as_ref(), &sid, token, || {
            state.sessions.report_session_ended(&sid)
        })
        .await;
    }
    drop(tx);
    tracing::info!(
        elapsed_s = started.elapsed().as_secs(),
        "websocket disconnected"
    );
}

/// The armed periodic streams of one socket.
struct Streams {
    sessions: Option<tokio::time::Interval>,
    tasks: Option<tokio::time::Interval>,
    activity: Option<tokio::time::Interval>,
    /// Only activity entries created after this instant are streamed (advanced
    /// on every send, so each entry is delivered once).
    activity_since: chrono::DateTime<chrono::Utc>,
}

impl Default for Streams {
    fn default() -> Self {
        Self {
            sessions: None,
            tasks: None,
            activity: None,
            activity_since: chrono::Utc::now(),
        }
    }
}

impl Streams {
    /// Arms or cancels a stream for one subscription message.
    fn apply(&mut self, inbound: Inbound) {
        let arm = |period: Duration| {
            let mut interval = tokio::time::interval(period);
            // First send one period from now, not immediately.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.reset();
            Some(interval)
        };
        match inbound {
            Inbound::SessionsStart(p) => self.sessions = arm(p),
            Inbound::SessionsStop => self.sessions = None,
            Inbound::TasksStart(p) => self.tasks = arm(p),
            Inbound::TasksStop => self.tasks = None,
            Inbound::ActivityStart(p) => {
                self.activity_since = chrono::Utc::now();
                self.activity = arm(p);
            }
            Inbound::ActivityStop => self.activity = None,
            Inbound::KeepAlive => {}
        }
    }
}

/// Awaits the next tick of an armed stream; pends forever when unarmed (so the
/// select branch never fires for a stream the client hasn't subscribed to).
async fn tick(stream: &mut Option<tokio::time::Interval>) {
    match stream {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// One outbound envelope: `{MessageType, MessageId, Data}` with the required
/// hyphenated `MessageId` (strict Kotlin-SDK clients crash without it).
fn envelope(message_type: &str, data: &serde_json::Value) -> String {
    serde_json::json!({
        "MessageType": message_type,
        "MessageId": uuid::Uuid::new_v4().hyphenated().to_string(),
        "Data": data,
    })
    .to_string()
}

/// The `Sessions` stream payload: the session list as the subscribing user
/// sees it (the session manager applies the caller's visibility).
async fn sessions_message(state: &AppState, user_id: uuid::Uuid) -> Option<String> {
    let sessions = state
        .sessions
        .get_sessions(user_id, None, None, None, false)
        .await
        .ok()?;
    Some(envelope("Sessions", &serde_json::to_value(sessions).ok()?))
}

/// The `ScheduledTasksInfo` stream payload: the current task list.
async fn tasks_message(state: &AppState) -> Option<String> {
    let tasks = state.tasks.get_tasks().await.ok()?;
    Some(envelope(
        "ScheduledTasksInfo",
        &serde_json::to_value(tasks).ok()?,
    ))
}

/// The `ActivityLogEntry` stream payload: entries created since the last send,
/// or `None` when nothing new happened (no empty pushes).
async fn activity_message(
    state: &AppState,
    since: &mut chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let query = ferrofin_traits::activity::ActivityLogQuery {
        min_date: Some(*since),
        ..Default::default()
    };
    let page = state.activity.get_paged_result(&query).await.ok()?;
    if page.items.is_empty() {
        return None;
    }
    if let Some(latest) = page.items.iter().map(|e| e.date).max() {
        // The min_date filter is inclusive (`>=`), so step past the newest
        // delivered entry or it would be re-sent on every tick.
        *since = latest + chrono::Duration::milliseconds(1);
    }
    Some(envelope(
        "ActivityLogEntry",
        &serde_json::to_value(page.items).ok()?,
    ))
}

/// The `KeepAlive` ack sent in reply to a client keep-alive ping.
fn keep_alive_ack() -> String {
    let message_id = uuid::Uuid::new_v4().hyphenated();
    format!("{{\"MessageType\":\"KeepAlive\",\"MessageId\":\"{message_id}\"}}")
}

/// The `ForceKeepAlive` message body Jellyfin's protocol uses.
///
/// Every outbound message carries a `MessageId` (C# `OutboundWebSocketMessage`
/// sets `Guid.NewGuid()`). It is `format: uuid` and *required* by strict
/// clients: without it the Jellyfin Kotlin SDK throws `MissingFieldException`
/// and the Android TV app crashes mid-playback. Emit a fresh, canonically
/// hyphenated UUID (the SDK parses it via `UUID.fromString`, which rejects the
/// dash-less form).
fn force_keep_alive_message() -> String {
    let message_id = uuid::Uuid::new_v4().hyphenated();
    format!(
        "{{\"MessageType\":\"ForceKeepAlive\",\"MessageId\":\"{message_id}\",\"Data\":{KEEPALIVE_SECS}}}"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Action, DEFAULT_STREAM_MILLIS, Inbound, KEEPALIVE_SECS, PUSH_QUEUE_DEPTH, action_for,
        force_keep_alive_message, header_token, keep_alive_ack, parse_inbound, push_sink,
        query_param,
    };
    use axum::extract::ws::Message;
    use axum::http::HeaderMap;
    use tokio::sync::mpsc;

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

    #[test]
    fn query_param_extracts_by_exact_key() {
        let q = Some("deviceId=dev1&api_key=abc123&x=1");
        assert_eq!(query_param(q, "api_key").as_deref(), Some("abc123"));
        assert_eq!(query_param(q, "deviceId").as_deref(), Some("dev1"));
        assert_eq!(query_param(q, "ApiKey"), None); // case-sensitive
        assert_eq!(query_param(Some("api_key="), "api_key"), None); // empty value
        assert_eq!(query_param(None, "api_key"), None);
    }

    #[test]
    fn header_token_reads_bare_token_headers() {
        let mut h = HeaderMap::new();
        assert_eq!(header_token(&h), None);
        h.insert("X-Emby-Token", "tok-1".parse().unwrap());
        assert_eq!(header_token(&h).as_deref(), Some("tok-1"));
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
    fn keep_alive_ack_is_valid_json_with_a_message_id() {
        let v: serde_json::Value = serde_json::from_str(&keep_alive_ack()).unwrap();
        assert_eq!(v["MessageType"], "KeepAlive");
        assert!(v["MessageId"].as_str().unwrap().contains('-'));
    }

    #[test]
    fn keep_alive_message_is_valid_json_with_the_interval() {
        let m = force_keep_alive_message();
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["MessageType"], "ForceKeepAlive");
        assert_eq!(v["Data"], KEEPALIVE_SECS);
        // The SDK requires `MessageId` (`format: uuid`) or the Android client
        // crashes with MissingFieldException. Must be a canonical, hyphenated
        // UUID — the dash-less form fails the SDK's `UUID.fromString`.
        let id = v["MessageId"].as_str().expect("MessageId present");
        assert!(
            uuid::Uuid::try_parse(id).is_ok(),
            "MessageId is a UUID: {id}"
        );
        assert_eq!(id.len(), 36, "hyphenated form (8-4-4-4-12)");
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
}
