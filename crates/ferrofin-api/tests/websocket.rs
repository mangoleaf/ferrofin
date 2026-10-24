//! Session WebSocket (`/socket`) tests that need a **real** socket.
//!
//! The handler unit tests in `handlers::websocket` cover frame parsing and the
//! bounded push queue in isolation; these drive an actual TCP connection through
//! `axum::serve`, which is the only way to exercise the inbound framing (the
//! part affected by the socket's read-buffer capacity) and the authentication
//! that decides whether the upgrade happens at all.

use std::net::SocketAddr;

use ferrofin_api::state::AppState;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Serves the real router over `state` on an ephemeral loopback port and
/// returns its address.
async fn serve(state: AppState) -> SocketAddr {
    let app = ferrofin_api::create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Reads frames until a text frame arrives (ping/pong traffic is skipped).
async fn next_text<S>(socket: &mut S) -> String
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return text.to_string(),
            Some(Ok(_)) => {}
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The socket's inbound buffer is deliberately much smaller than a frame a
/// client is *allowed* to send (see `READ_BUFFER_BYTES` — it is sized for the
/// tiny `KeepAlive`/subscription vocabulary because it is memset on every push,
/// not for the largest legal frame). Shrinking it must not change what the
/// socket accepts: a frame many times the buffer has to be reassembled across
/// reads and answered exactly as a small one is.
#[tokio::test]
async fn a_frame_many_times_the_read_buffer_is_still_answered() {
    let addr = serve(ferrofin_api::test_support::authed_fake_state()).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("upgrade");

    // Control: the ordinary, tiny keep-alive is acked.
    socket
        .send(Message::text(r#"{"MessageType":"KeepAlive"}"#))
        .await
        .expect("send small");
    let small: serde_json::Value =
        serde_json::from_str(&next_text(&mut socket).await).expect("json");
    assert_eq!(small["MessageType"], "KeepAlive");

    // The same message padded to 256 KiB — 64x the read buffer, so tungstenite
    // must reserve the frame length and fill it over many reads.
    let padding = "p".repeat(256 * 1024);
    socket
        .send(Message::text(format!(
            r#"{{"MessageType":"KeepAlive","Pad":"{padding}"}}"#
        )))
        .await
        .expect("send large");
    let large: serde_json::Value =
        serde_json::from_str(&next_text(&mut socket).await).expect("json");
    assert_eq!(
        large["MessageType"], "KeepAlive",
        "a frame far larger than the read buffer must be reassembled and answered"
    );
    // The SDK's `MessageId` is a required `format: uuid` field, so it has to be
    // present on every outbound message — in Jellyfin's dashless "N" spelling,
    // which every `Guid` takes through `JsonGuidConverter`.
    assert!(
        large["MessageId"]
            .as_str()
            .is_some_and(|id| id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())),
        "every outbound message carries a dashless MessageId: {large}"
    );
}

/// The socket authenticates through the same `AuthService` as every HTTP
/// request (C# `WebSocketManager.WebSocketRequestHandler`), so whether an
/// Emby-era credential counts is `EnableLegacyAuthorization`'s call, not the
/// socket's. With an auth service that accepts nothing, every credential form,
/// including the `api_key` and bare-token headers the socket once read itself,
/// is upstream's `403` "Token is required", and the connection is never upgraded.
#[tokio::test]
async fn a_socket_the_auth_service_rejects_is_refused_before_the_upgrade() {
    let addr = serve(ferrofin_api::test_support::fake_state()).await;
    for (label, query, header) in [
        ("no credentials", "", None),
        ("?api_key=", "?api_key=tok", None),
        ("?ApiKey=", "?ApiKey=tok", None),
        ("X-Emby-Token", "", Some(("X-Emby-Token", "tok"))),
        (
            "X-MediaBrowser-Token",
            "",
            Some(("X-MediaBrowser-Token", "tok")),
        ),
        (
            "Authorization: MediaBrowser",
            "",
            Some(("Authorization", r#"MediaBrowser Token="tok""#)),
        ),
    ] {
        let mut request = format!("ws://{addr}/socket{query}")
            .into_client_request()
            .expect("request");
        if let Some((name, value)) = header {
            request
                .headers_mut()
                .insert(name, value.parse().expect("header value"));
        }
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), 403, "{label}");
            }
            other => panic!("{label}: expected a 403 refusal, got {other:?}"),
        }
    }
}

/// A lost socket closes only when it has a session to end
/// (`SessionWebSocketListener.KeepAliveSockets` disposes it since Jellyfin
/// 12.0-rc3; every socket has a session there). One without a session (this
/// fake's auth carries no token, so it resolves none) was never greeted, so it is only dropped from the
/// watch and stays open past `WebSocketLostTimeout`. The clock is paused once
/// the upgrade is done, so the timeout passes at once.
#[tokio::test]
async fn a_lost_sessionless_socket_stays_open() {
    let addr = serve(ferrofin_api::test_support::authed_fake_state()).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("upgrade");
    tokio::time::pause();

    // Read the server's prompts, never answering, well past the lost timeout.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(300), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;

    assert!(closed.is_err(), "a sessionless socket was closed when lost");
}
