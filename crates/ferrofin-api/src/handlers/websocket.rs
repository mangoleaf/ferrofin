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
    ws.on_upgrade(move |socket| handle_socket(socket, state, caller))
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
    // closing the loop.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let registration = match (caller.as_ref().ok(), state.session_bus.as_ref()) {
        (Some(c), Some(bus)) => {
            let sink_tx = tx.clone();
            bus.register(
                c.session_id.clone(),
                Box::new(move |msg| {
                    // Failure means the socket's receiver is gone; the socket
                    // unregisters on close, so dropping the message is correct.
                    let _ = sink_tx.send(msg);
                }),
            );
            Some((c.session_id.clone(), std::sync::Arc::clone(bus)))
        }
        _ => None,
    };
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
                    if socket.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Action::Stop => break,
                Action::Ignore => {}
                Action::Inbound(Inbound::KeepAlive) => {
                    // Ack the client's ping (C# `SendKeepAliveResponse`).
                    if socket.send(Message::Text(keep_alive_ack().into())).await.is_err() {
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
                if socket.send(Message::Text(push.into())).await.is_err() {
                    break;
                }
            },
            _ = keepalive.tick() => {
                // Jellyfin's `ForceKeepAlive`: tells the client the keep-alive interval.
                let msg = Message::Text(force_keep_alive_message().into());
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
            () = tick(&mut streams.sessions) => {
                if let Some(c) = caller.as_ref()
                    && let Some(msg) = sessions_message(&state, c.user_id).await
                    && socket.send(Message::Text(msg.into())).await.is_err()
                {
                    break;
                }
            }
            () = tick(&mut streams.tasks) => {
                if let Some(msg) = tasks_message(&state).await
                    && socket.send(Message::Text(msg.into())).await.is_err()
                {
                    break;
                }
            }
            () = tick(&mut streams.activity) => {
                if let Some(msg) = activity_message(&state, &mut streams.activity_since).await
                    && socket.send(Message::Text(msg.into())).await.is_err()
                {
                    break;
                }
            }
        }
    }

    if let Some((sid, bus)) = registration {
        bus.unregister(&sid);
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
        Action, DEFAULT_STREAM_MILLIS, Inbound, KEEPALIVE_SECS, action_for,
        force_keep_alive_message, header_token, keep_alive_ack, parse_inbound, query_param,
    };
    use axum::extract::ws::Message;
    use axum::http::HeaderMap;

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
}
