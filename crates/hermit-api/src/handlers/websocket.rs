//! The session WebSocket (`/socket`, and the legacy `/embywebsocket` alias).
//!
//! jellyfin-web opens a WebSocket immediately after authenticating and treats it
//! as *the* server connection — if it never establishes, the client reports
//! "Connection Failure" and refuses to enter the dashboard, even though the REST
//! calls all succeeded. These routes are NOT part of the OpenAPI contract (Swagger
//! doesn't describe WebSocket endpoints), so they are registered as extras.
//!
//! This is a minimal, always-open socket: it accepts the upgrade, answers pings,
//! sends periodic keep-alives, and stays open until the peer closes. Real
//! server→client event pushing (session/now-playing/remote-control messages) is a
//! follow-up; *establishing and holding* the connection is what unblocks the UI.

use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;

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
/// jellyfin-web passes the access token as the `api_key` query parameter; the
/// upgrade is accepted so the client can open its session channel. (Token
/// validation on the socket is a follow-up — see the module docs.)
async fn websocket_upgrade(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
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

/// Holds a WebSocket open: answer pings, send a periodic keep-alive, and close
/// cleanly when the peer goes away.
async fn handle_socket(mut socket: WebSocket) {
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
            _ = keepalive.tick() => {
                // Jellyfin's `ForceKeepAlive`: tells the client the keep-alive interval.
                let msg = Message::Text(force_keep_alive_message().into());
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// The `ForceKeepAlive` message body Jellyfin's protocol uses.
fn force_keep_alive_message() -> String {
    format!("{{\"MessageType\":\"ForceKeepAlive\",\"Data\":{KEEPALIVE_SECS}}}")
}

#[cfg(test)]
mod tests {
    use super::{Action, KEEPALIVE_SECS, action_for, force_keep_alive_message};
    use axum::extract::ws::Message;

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
    }
}
