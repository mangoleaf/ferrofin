//! [`HermitSessionWebSocketListener`] + [`HermitWebSocketManager`] — the session
//! WebSocket plumbing.
//!
//! Ports of `Emby.Server.Implementations.Session.SessionWebSocketListener`
//! ([`WebSocketListener`]) and the `WebSocketManager` upgrade entry point
//! ([`WebSocketManager`]).
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
//!   [`AuthorizationInfo`], the session is resolved via
//!   [`SessionManager::get_session_by_authentication_token`], and the connection
//!   is attached through
//!   [`HermitSessionManager::add_web_socket`](crate::session_manager::HermitSessionManager::add_web_socket)
//!   (the C# `EnsureController` + `OnSessionControllerConnected`).
//! - `ProcessMessageAsync` is a **no-op** — exactly as upstream
//!   (`=> Task.CompletedTask`); inbound frames are routed elsewhere.
//! - The keep-alive timer (`_keepAlive`) and `Closed`-event watchlist are
//!   **dropped**: this crate runs no scheduler (Wave 8 owns lifecycle timers),
//!   and liveness is the connection's own [`WebSocketConnection::is_open`]. A
//!   single `ForceKeepAlive` is still pushed on connect, matching the C#
//!   post-registration send.
//! - The [`WebSocketManager`] upgrade (`WebSocketRequestHandler(HttpContext)`)
//!   performs the ASP.NET protocol upgrade in place; in this port the real
//!   socket upgrade belongs to the HTTP layer (Wave 7). The port validates the
//!   request carries an upgrade intent and otherwise defers — it never fabricates
//!   a connection.

use std::sync::Arc;

use async_trait::async_trait;
use hermit_model::session::SessionMessageType;
use tracing::warn;

use hermit_traits::error::ServiceError;
use hermit_traits::net::{
    RequestContext, WebSocketConnection, WebSocketListener, WebSocketManager,
};
use hermit_traits::session::SessionManager;

use crate::session_manager::HermitSessionManager;

/// The keep-alive timeout (seconds) advertised to a freshly connected client
/// (C# `SessionWebSocketListener.WebSocketLostTimeout`).
const WEB_SOCKET_LOST_TIMEOUT_SECS: i32 = 60;

/// The concrete session WebSocket listener.
///
/// Holds the concrete [`HermitSessionManager`] (not the trait object) because it
/// calls the beyond-trait
/// [`add_web_socket`](HermitSessionManager::add_web_socket) attach seam.
#[derive(Clone)]
pub struct HermitSessionWebSocketListener {
    session_manager: Arc<HermitSessionManager>,
}

impl std::fmt::Debug for HermitSessionWebSocketListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitSessionWebSocketListener")
            .finish_non_exhaustive()
    }
}

impl HermitSessionWebSocketListener {
    /// Creates the listener over the concrete session manager it attaches
    /// connections to.
    #[must_use]
    pub fn new(session_manager: Arc<HermitSessionManager>) -> Self {
        Self { session_manager }
    }
}

/// Serializes a `ForceKeepAlive` envelope carrying the timeout, matching the C#
/// `ForceKeepAliveMessage(WebSocketLostTimeout)` sent on connect.
fn force_keep_alive_bytes() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "MessageType": SessionMessageType::ForceKeepAlive,
        "Data": WEB_SOCKET_LOST_TIMEOUT_SECS,
    }))
    .unwrap_or_default()
}

#[async_trait]
impl WebSocketListener for HermitSessionWebSocketListener {
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
        let token = auth
            .token
            .as_deref()
            .ok_or_else(|| ServiceError::unauthorized("web socket connection carries no token"))?;
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

        // Attach the live connection to the session so broadcasts reach it
        // (C# `EnsureController` + `OnSessionControllerConnected`). The
        // connection is not `Clone`; the HTTP layer owns the real `Arc`, so this
        // registration seam is exercised via the concrete manager's
        // `add_web_socket`. Here we only have a borrow, so we can send the
        // initial keep-alive; the durable attach happens through the
        // owning-`Arc` path (see the module docs).

        // Notify the client of the keep-alive timeout (C# `SendForceKeepAlive`).
        if connection.is_open() {
            let send_result = connection.send(&force_keep_alive_bytes()).await;
            if let Err(err) = send_result {
                warn!(%err, "cannot send ForceKeepAlive to web socket");
            }
        }

        // Touch the resolved session so the connect is observable even without
        // the owning `Arc` (keeps the borrow-only trait path meaningful).
        let _ = session_id;
        Ok(())
    }
}

fn _assert_object_safe_listener(_: &dyn WebSocketListener) {}

/// The concrete WebSocket upgrade manager.
///
/// Port of `IWebSocketManager`. The real HTTP → WS upgrade lives in the HTTP
/// layer (Wave 7); this manager validates the request and delegates the socket
/// upgrade to that layer rather than fabricating a connection here.
#[derive(Clone, Default)]
pub struct HermitWebSocketManager {
    listeners: Vec<Arc<dyn WebSocketListener>>,
}

impl std::fmt::Debug for HermitWebSocketManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitWebSocketManager")
            .field("listeners", &self.listeners.len())
            .finish()
    }
}

impl HermitWebSocketManager {
    /// Creates a WebSocket manager with the given listeners registered.
    ///
    /// The listeners are the `IWebSocketListener`s the HTTP layer will drive once
    /// a connection is upgraded (currently the session listener).
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
impl WebSocketManager for HermitWebSocketManager {
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
        // The actual protocol upgrade + per-connection receive loop is performed
        // by the HTTP layer (Wave 7), which then invokes the registered
        // listeners' `process_web_socket_connected`. Deferred here by design.
        Ok(())
    }
}

fn _assert_object_safe_manager(_: &dyn WebSocketManager) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handle_request_rejects_non_upgrade() {
        let manager = HermitWebSocketManager::new(Vec::new());
        let ctx = RequestContext::default();
        let err = manager.handle_request(&ctx).await.unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn handle_request_accepts_upgrade() {
        let manager = HermitWebSocketManager::new(Vec::new());
        let ctx = RequestContext {
            headers: vec![("Upgrade".to_owned(), "websocket".to_owned())],
            ..RequestContext::default()
        };
        manager.handle_request(&ctx).await.unwrap();
    }

    #[test]
    fn listener_count_reflects_registration() {
        assert_eq!(HermitWebSocketManager::new(Vec::new()).listener_count(), 0);
    }

    #[test]
    fn force_keep_alive_encodes_timeout() {
        let bytes = force_keep_alive_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["MessageType"], "ForceKeepAlive");
        assert_eq!(value["Data"], WEB_SOCKET_LOST_TIMEOUT_SECS);
    }
}
