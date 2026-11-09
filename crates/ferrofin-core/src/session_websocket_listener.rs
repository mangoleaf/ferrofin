//! [`FerrofinSessionWebSocketListener`] + [`FerrofinWebSocketManager`] — the session
//! WebSocket plumbing.
//!
//! Ports of `Emby.Server.Implementations.Session.SessionWebSocketListener`
//! ([`WebSocketListener`]) and the `WebSocketManager` upgrade entry point
//! ([`WebSocketManager`]).
//!
//! Neither runs in production: `ferrofin-api`'s `/socket` handler upgrades the
//! connection, registers it for its session on the `SessionMessageBus` and
//! sends the connect-time `ForceKeepAlive` itself, and calls neither this
//! listener nor the manager.
//! TODO(gl-248): open work item: serve `/socket` through one path, not two.
//!
//! ## What this unit ports
//! On a newly established connection the C# listener resolves the client's
//! [`SessionInfo`](crate::session_manager) from the request (its access token),
//! attaches a `WebSocketController` to that session so server-initiated messages
//! reach the client, and registers the socket on a keep-alive watchlist.
//!
//! Port rules applied:
//! - The `HttpContext` argument becomes the transport-agnostic
//!   [`RequestContext`]; the connection is a `&dyn `[`WebSocketConnection`]. On
//!   connect, the token/device/endpoint are read from the connection's captured
//!   `AuthorizationInfo` and the session is resolved via
//!   [`SessionManager::get_session_by_authentication_token`]. The listener gets
//!   only a borrow of the connection, so it does not attach it: the attach seam
//!   (the C# `EnsureController` + `OnSessionControllerConnected`) is
//!   [`FerrofinSessionManager::add_web_socket`](crate::session_manager::FerrofinSessionManager::add_web_socket),
//!   for a caller that holds the connection's `Arc`.
//! - `ProcessMessageAsync` is a **no-op** — exactly as upstream
//!   (`=> Task.CompletedTask`); inbound frames are routed elsewhere.
//! - The connect-time `ForceKeepAlive` is sent here, matching the C#
//!   post-registration send (`KeepAliveWebSocket`, :168-176). The `_keepAlive`
//!   WATCHDOG that follows it does not live in this crate — it has to sit where
//!   the socket's own frames are, so it is ported into the axum socket loop
//!   (`ferrofin-api::handlers::websocket::handle_socket`): every
//!   `IntervalFactor * WebSocketLostTimeout`, a socket silent for more than
//!   `ForceKeepAliveFactor * WebSocketLostTimeout` is prodded again, and one
//!   past the full timeout leaves the watchlist. Liveness itself is the
//!   connection's own [`WebSocketConnection::is_open`].
//! - The [`WebSocketManager`] upgrade (`WebSocketRequestHandler(HttpContext)`)
//!   performs the ASP.NET protocol upgrade in place; here the socket upgrade
//!   belongs to the HTTP layer. This manager only checks that the request
//!   carries an upgrade intent — it never fabricates a connection.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::secret::Secret;
use ferrofin_model::session::SessionMessageType;
use tracing::warn;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::{
    RequestContext, WebSocketConnection, WebSocketListener, WebSocketManager,
};
use ferrofin_traits::session::SessionManager;

use crate::session_manager::FerrofinSessionManager;

/// The keep-alive timeout (seconds) advertised to a freshly connected client
/// (C# `SessionWebSocketListener.WebSocketLostTimeout`).
const WEB_SOCKET_LOST_TIMEOUT_SECS: i32 = 60;

/// The concrete session WebSocket listener.
///
/// Holds the concrete [`FerrofinSessionManager`] (not the trait object), whose
/// beyond-trait [`add_web_socket`](FerrofinSessionManager::add_web_socket) is
/// the attach seam; the listener itself never calls it (see the module docs).
#[derive(Clone)]
pub struct FerrofinSessionWebSocketListener {
    session_manager: Arc<FerrofinSessionManager>,
}

impl std::fmt::Debug for FerrofinSessionWebSocketListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinSessionWebSocketListener")
            .finish_non_exhaustive()
    }
}

impl FerrofinSessionWebSocketListener {
    /// Creates the listener over the concrete session manager it resolves
    /// sessions from.
    #[must_use]
    pub fn new(session_manager: Arc<FerrofinSessionManager>) -> Self {
        Self { session_manager }
    }
}

/// Serializes a `ForceKeepAlive` envelope carrying the timeout, matching the C#
/// `ForceKeepAliveMessage(WebSocketLostTimeout)` sent on connect.
fn force_keep_alive_bytes() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "MessageType": SessionMessageType::ForceKeepAlive,
        // Required `format: uuid` field on every outbound message; strict clients
        // (the Jellyfin Kotlin SDK) crash without it. Jellyfin's guid spelling.
        "MessageId": Uuid::new_v4().simple().to_string(),
        "Data": WEB_SOCKET_LOST_TIMEOUT_SECS,
    }))
    .unwrap_or_default()
}

#[async_trait]
impl WebSocketListener for FerrofinSessionWebSocketListener {
    async fn process_message(
        &self,
        _connection: &dyn WebSocketConnection,
        _message: &[u8],
    ) -> Result<(), ServiceError> {
        // C# `ProcessMessageAsync` returns `Task.CompletedTask` — inbound frames
        // are handled by other listeners, not this one.
        Ok(())
    }

    async fn process_web_socket_connected(
        &self,
        connection: &dyn WebSocketConnection,
        request: &RequestContext,
    ) -> Result<(), ServiceError> {
        // Resolve the session from the token the connection captured at upgrade
        // (C# `RequestHelpers.GetSession`).
        let auth = connection.authorization_info();
        let token =
            auth.token.as_ref().map(Secret::expose).ok_or_else(|| {
                ServiceError::unauthorized("web socket connection carries no token")
            })?;
        let device_id = auth.device_id.as_deref().unwrap_or("");
        let remote_endpoint = connection
            .remote_endpoint()
            .or(request.remote_endpoint.as_deref())
            .unwrap_or("");

        let session = self
            .session_manager
            .get_session_by_authentication_token(token, device_id, remote_endpoint)
            .await?;
        let session_id = session
            .id
            .ok_or_else(|| ServiceError::backend("resolved session has no id"))?;

        // C# attaches the connection here (`EnsureController` +
        // `OnSessionControllerConnected`). This listener has only a borrow of
        // a connection that is not `Clone`, so it attaches nothing; only tests
        // call `add_web_socket`, and `/socket` registers on the message bus
        // instead (see the module docs, gl-248).

        // Notify the client of the keep-alive timeout (C# `SendForceKeepAlive`).
        if connection.is_open() {
            let send_result = connection.send(&force_keep_alive_bytes()).await;
            if let Err(err) = send_result {
                warn!(%err, "cannot send ForceKeepAlive to web socket");
            }
        }

        // A no-op: the resolved session id has no use here, since nothing is
        // attached (gl-248).
        let _ = session_id;
        Ok(())
    }
}

fn _assert_object_safe_listener(_: &dyn WebSocketListener) {}

/// The concrete WebSocket upgrade manager.
///
/// Port of `IWebSocketManager`. The real HTTP → WS upgrade lives in the HTTP
/// layer; this manager only validates the request and never fabricates a
/// connection. Nothing calls it today (see the module docs).
#[derive(Clone, Default)]
pub struct FerrofinWebSocketManager {
    listeners: Vec<Arc<dyn WebSocketListener>>,
}

impl std::fmt::Debug for FerrofinWebSocketManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinWebSocketManager")
            .field("listeners", &self.listeners.len())
            .finish()
    }
}

impl FerrofinWebSocketManager {
    /// Creates a WebSocket manager with the given listeners registered.
    ///
    /// The listeners are the `IWebSocketListener`s to run on an upgraded
    /// connection (the session listener); the HTTP layer does not drive them
    /// today (see the module docs).
    #[must_use]
    pub fn new(listeners: Vec<Arc<dyn WebSocketListener>>) -> Self {
        Self { listeners }
    }

    /// The number of registered listeners (introspection aid).
    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.listeners.len()
    }
}

#[async_trait]
impl WebSocketManager for FerrofinWebSocketManager {
    async fn handle_request(&self, request: &RequestContext) -> Result<(), ServiceError> {
        // A genuine upgrade request advertises `Upgrade: websocket`. Without it
        // there is nothing to upgrade (the C# handler 400s a non-WS request).
        let is_upgrade = request
            .header("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        if !is_upgrade {
            return Err(ServiceError::invalid_input(
                "not a web socket upgrade request",
            ));
        }
        // The protocol upgrade and the per-connection receive loop belong to
        // the HTTP layer. `ferrofin-api`'s `/socket` handler does both itself
        // and calls neither this manager nor its listeners. TODO(gl-248): open
        // work item: serve `/socket` through one path, not two.
        Ok(())
    }
}

fn _assert_object_safe_manager(_: &dyn WebSocketManager) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handle_request_rejects_non_upgrade() {
        let manager = FerrofinWebSocketManager::new(Vec::new());
        let ctx = RequestContext::default();
        let err = manager.handle_request(&ctx).await.unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn handle_request_accepts_upgrade() {
        let manager = FerrofinWebSocketManager::new(Vec::new());
        let ctx = RequestContext {
            headers: vec![("Upgrade".to_owned(), "websocket".to_owned())],
            ..RequestContext::default()
        };
        manager.handle_request(&ctx).await.unwrap();
    }

    #[test]
    fn listener_count_reflects_registration() {
        assert_eq!(
            FerrofinWebSocketManager::new(Vec::new()).listener_count(),
            0
        );
    }

    #[test]
    fn force_keep_alive_encodes_timeout() {
        let bytes = force_keep_alive_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["MessageType"], "ForceKeepAlive");
        assert_eq!(value["Data"], WEB_SOCKET_LOST_TIMEOUT_SECS);
    }
}
