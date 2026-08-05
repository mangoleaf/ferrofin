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
    let session_id = resolve_session_id(&state, &headers, query.as_deref()).await;
    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id))
}

/// Resolves the caller's session id from the socket's access token, or `None`
/// when anonymous.
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
/// [`get_session_by_authentication_token`]: hermit_traits::session::SessionManager::get_session_by_authentication_token
/// [`AuthService`]: hermit_traits::net::AuthService
async fn resolve_session_id(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Option<String> {
    let token = query_param(query, "api_key")
        .or_else(|| query_param(query, "ApiKey"))
        .or_else(|| header_token(headers))?;
    state
        .sessions
        .get_session_by_authentication_token(&token, "", "")
        .await
        .ok()?
        .id
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
    /// Nothing to do (text/binary/pong in the minimal socket).
    Ignore,
}

/// Decides how to react to one received frame.
fn action_for(frame: Option<Result<Message, axum::Error>>) -> Action {
    match frame {
        Some(Ok(Message::Ping(payload))) => Action::Pong(payload),
        Some(Ok(Message::Close(_)) | Err(_)) | None => Action::Stop,
        Some(Ok(_)) => Action::Ignore,
    }
}

/// Holds a WebSocket open: register the caller's push sink (if authenticated),
/// answer pings, forward server→client pushes, send a periodic keep-alive, and
/// close cleanly — unregistering the sink — when the peer goes away.
#[tracing::instrument(
    name = "ws_session",
    skip_all,
    fields(session_id = session_id.as_deref().unwrap_or("anonymous"))
)]
async fn handle_socket(mut socket: WebSocket, state: AppState, session_id: Option<String>) {
    let started = std::time::Instant::now();
    // `tx` feeds this socket; the bus holds a clone as the session's delivery
    // sink. Keeping `tx` alive here also keeps `rx` open for anonymous sockets
    // (no sink registered) so the forward branch stays pending rather than
    // closing the loop.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let registration = match (session_id.as_ref(), state.session_bus.as_ref()) {
        (Some(sid), Some(bus)) => {
            let sink_tx = tx.clone();
            bus.register(
                sid.clone(),
                Box::new(move |msg| {
                    // Failure means the socket's receiver is gone; the socket
                    // unregisters on close, so dropping the message is correct.
                    let _ = sink_tx.send(msg);
                }),
            );
            Some((sid.clone(), std::sync::Arc::clone(bus)))
        }
        _ => None,
    };
    tracing::info!(
        authenticated = registration.is_some(),
        "websocket connected"
    );

    let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
    keepalive.tick().await; // consume the immediate first tick

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
        Action, KEEPALIVE_SECS, action_for, force_keep_alive_message, header_token, query_param,
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
    fn text_and_binary_are_ignored() {
        assert!(matches!(
            action_for(Some(Ok(Message::Text("{}".into())))),
            Action::Ignore
        ));
        assert!(matches!(
            action_for(Some(Ok(Message::Binary(axum::body::Bytes::new())))),
            Action::Ignore
        ));
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
