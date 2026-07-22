//! Networking-layer traits — request authorization and WebSocket plumbing.
//!
//! Port of the `MediaBrowser.Controller.Net` interfaces:
//! `IAuthorizationContext`, `IAuthService`, `IWebSocketConnection`,
//! `IWebSocketListener`, `IWebSocketManager`, plus the `WebSocketListenerState`
//! value type.
//!
//! Port rules applied throughout:
//! - The C# methods take `HttpContext`/`HttpRequest` (ASP.NET Core). To keep this
//!   crate transport-light — it must not pull in `axum`/`hyper` just to name a
//!   request — the request input is modelled as a small owned [`RequestContext`]
//!   (the handful of header/endpoint fields the authorization logic actually
//!   reads). The `hermit-core`/HTTP layer builds one from its real request type.
//! - `AuthorizationInfo` is the ported [`crate::options::AuthorizationInfo`]
//!   (a server-side context value, never serialized).
//! - `IAsyncDisposable`/`IDisposable`, .NET events, and the concrete
//!   `System.Net.WebSockets` state enum are dropped; the connection's liveness is
//!   exposed as a plain [`bool`].
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; `CancellationToken` is
//!   dropped for v1.
//!
//! Every trait is object-safe and carries a `_assert_object_safe_*` assertion.

use async_trait::async_trait;

use crate::error::ServiceError;
use crate::options::AuthorizationInfo;

/// A minimal, transport-agnostic view of an incoming HTTP request.
///
/// Stands in for the C# `HttpRequest`/`HttpContext` arguments of
/// [`AuthorizationContext`] and [`AuthService`]. It carries only what the
/// authorization logic reads: the raw header set, the query string, and the
/// caller's remote address. The HTTP layer (Wave 6) constructs one from its
/// concrete request; this keeps `hermit-traits` free of a web-framework
/// dependency while remaining object-safe (no borrowed request type in the
/// signature).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    /// The request headers as `(name, value)` pairs. Header names are compared
    /// case-insensitively by the authorization logic, matching HTTP semantics.
    pub headers: Vec<(String, String)>,

    /// The raw query string (without the leading `?`), if any. Jellyfin also
    /// accepts the access token via the `api_key`/`ApiKey` query parameter.
    pub query_string: Option<String>,

    /// The remote endpoint (client IP, possibly with port) as a string, if
    /// known. Mirrors the C# `RemoteEndPoint` used for logging and LAN checks.
    pub remote_endpoint: Option<String>,
}

impl RequestContext {
    /// Looks up the first value of a header by case-insensitive name.
    ///
    /// Returns `None` when the header is absent. HTTP header names are
    /// case-insensitive, so `authorization` and `Authorization` are equivalent.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Resolves the [`AuthorizationInfo`] for an incoming request.
///
/// Port of `IAuthorizationContext`. The two C# overloads (`HttpContext` and
/// `HttpRequest`) collapse to a single method taking a [`RequestContext`].
#[async_trait]
pub trait AuthorizationContext: Send + Sync {
    /// Parses the request's authorization header/query into an
    /// [`AuthorizationInfo`], resolving the token to a user where possible.
    async fn get_authorization_info(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError>;
}

fn _assert_object_safe_authorization_context(_: &dyn AuthorizationContext) {}

/// Authenticates a request, enforcing that it carries valid credentials.
///
/// Port of `IAuthService`. Where [`AuthorizationContext`] merely *parses* the
/// request, this *validates* it: the C# method returns `null` when
/// unauthenticated, which becomes [`ServiceError::Unauthorized`] here.
#[async_trait]
pub trait AuthService: Send + Sync {
    /// Authenticates the request, returning its [`AuthorizationInfo`] on success
    /// or [`ServiceError::Unauthorized`] when the credentials are missing or
    /// invalid.
    async fn authenticate(
        &self,
        request: &RequestContext,
    ) -> Result<AuthorizationInfo, ServiceError>;
}

fn _assert_object_safe_auth_service(_: &dyn AuthService) {}

/// A single live client WebSocket connection.
///
/// Port of `IWebSocketConnection` (the object-safe subset). The C#
/// `IAsyncDisposable`/`IDisposable` bases, the `OnReceive` callback property,
/// the `Closed` event, and the raw `WebSocketState` are dropped; liveness is a
/// plain [`Self::is_open`] check. Outbound messages are the serialized envelope
/// carried as bytes so the trait stays non-generic and object-safe (the typed
/// `SendAsync<T>` overload collapses into this one).
#[async_trait]
pub trait WebSocketConnection: Send + Sync {
    /// The client's remote endpoint (IP, possibly with port), if known.
    fn remote_endpoint(&self) -> Option<&str>;

    /// The [`AuthorizationInfo`] captured when the connection was established.
    fn authorization_info(&self) -> &AuthorizationInfo;

    /// Whether the connection is still open and usable for sending.
    fn is_open(&self) -> bool;

    /// Sends an already-serialized outbound message envelope to the client.
    async fn send(&self, message: &[u8]) -> Result<(), ServiceError>;

    /// Applies the culture captured from the connection's upgrade request to
    /// the current task, so server-initiated payloads localise for the client.
    async fn apply_request_culture(&self) -> Result<(), ServiceError>;
}

fn _assert_object_safe_web_socket_connection(_: &dyn WebSocketConnection) {}

/// Handles messages and lifecycle events for WebSocket connections.
///
/// Port of `IWebSocketListener`. The `HttpContext` argument of
/// `ProcessWebSocketConnectedAsync` becomes a [`RequestContext`]; the connection
/// is passed as `&dyn` [`WebSocketConnection`] to preserve object safety. The
/// incoming message is the raw serialized frame (the typed
/// `WebSocketMessageInfo` is decoded by the implementation).
#[async_trait]
pub trait WebSocketListener: Send + Sync {
    /// Processes a single inbound message from a connection.
    async fn process_message(
        &self,
        connection: &dyn WebSocketConnection,
        message: &[u8],
    ) -> Result<(), ServiceError>;

    /// Reacts to a newly established WebSocket connection.
    async fn process_web_socket_connected(
        &self,
        connection: &dyn WebSocketConnection,
        request: &RequestContext,
    ) -> Result<(), ServiceError>;
}

fn _assert_object_safe_web_socket_listener(_: &dyn WebSocketListener) {}

/// Upgrades incoming HTTP requests to WebSocket connections.
///
/// Port of `IWebSocketManager`. The C# `WebSocketRequestHandler(HttpContext)` —
/// which performs the protocol upgrade in place on ASP.NET's context — becomes a
/// request-handling entry point taking a [`RequestContext`]; the actual socket
/// upgrade is performed by the HTTP layer that satisfies this trait.
#[async_trait]
pub trait WebSocketManager: Send + Sync {
    /// Handles a WebSocket upgrade request.
    async fn handle_request(&self, request: &RequestContext) -> Result<(), ServiceError>;
}

fn _assert_object_safe_web_socket_manager(_: &dyn WebSocketManager) {}

/// Per-listener timing state for periodic WebSocket push loops.
///
/// Port of `WebSocketListenerState`. The C# `DateTime`/`long` fields map to
/// [`chrono::DateTime<Utc>`](chrono::DateTime) and [`i64`] millisecond delays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketListenerState {
    /// UTC timestamp of the last send in this loop.
    pub date_last_send_utc: chrono::DateTime<chrono::Utc>,

    /// The initial delay before the first send, in milliseconds.
    pub initial_delay_ms: i64,

    /// The interval between subsequent sends, in milliseconds.
    pub interval_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::RequestContext;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let ctx = RequestContext {
            headers: vec![("Authorization".to_owned(), "Bearer abc".to_owned())],
            ..Default::default()
        };
        assert_eq!(ctx.header("authorization"), Some("Bearer abc"));
        assert_eq!(ctx.header("AUTHORIZATION"), Some("Bearer abc"));
        assert_eq!(ctx.header("x-missing"), None);
    }

    #[test]
    fn default_request_context_is_empty() {
        let ctx = RequestContext::default();
        assert!(ctx.headers.is_empty());
        assert_eq!(ctx.query_string, None);
        assert_eq!(ctx.remote_endpoint, None);
    }
}
