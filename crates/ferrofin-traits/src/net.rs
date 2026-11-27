//! Networking-layer traits — request authorization.
//!
//! Port of the `MediaBrowser.Controller.Net` interfaces `IAuthorizationContext`
//! and `IAuthService`. The WebSocket ones (`IWebSocketConnection`,
//! `IWebSocketListener`, `IWebSocketManager`) have no trait here:
//! `ferrofin-api`'s `/socket` handler is their port, and it delivers server
//! pushes through [`crate::session_bus::SessionMessageBus`].
//!
//! Port rules applied throughout:
//! - The C# methods take `HttpContext`/`HttpRequest` (ASP.NET Core). To keep this
//!   crate transport-light — it must not pull in `axum`/`hyper` just to name a
//!   request — the request input is modelled as a small owned [`RequestContext`]
//!   (the handful of header/endpoint fields the authorization logic actually
//!   reads). The `ferrofin-core`/HTTP layer builds one from its real request type.
//! - `AuthorizationInfo` is the ported [`crate::options::AuthorizationInfo`]
//!   (a server-side context value, never serialized).
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
/// concrete request; this keeps `ferrofin-traits` free of a web-framework
/// dependency while remaining object-safe (no borrowed request type in the
/// signature).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    /// The request headers as `(name, value)` pairs. Header names are compared
    /// case-insensitively by the authorization logic, matching HTTP semantics.
    ///
    /// The HTTP layer gives [`AuthorizationContext`] and [`AuthService`] only
    /// the headers they read (`Authorization`, `X-Emby-Authorization`,
    /// `X-Emby-Token` and `X-MediaBrowser-Token`), and every header to the
    /// other readers, such as the `Host` a system address is built from (a
    /// known proxy's `X-Forwarded-Host` in its place, as
    /// `ForwardedHeadersMiddleware` leaves it).
    pub headers: Vec<(String, String)>,

    /// The raw query string (without the leading `?`), if any. Jellyfin also
    /// accepts the access token via the `ApiKey` query parameter (`api_key`
    /// only with `EnableLegacyAuthorization`).
    pub query_string: Option<String>,

    /// The remote endpoint (client IP, possibly with port) as a string, if
    /// known. Mirrors the C# `RemoteEndPoint` used for logging and LAN checks.
    pub remote_endpoint: Option<String>,

    /// The scheme the request arrived with (`HttpRequest.Scheme`): `https`
    /// over an HTTPS listener, or the scheme a known proxy forwarded. `None`
    /// when the caller did not resolve it, which reads as `http`.
    pub scheme: Option<String>,
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
        assert!(ctx.headers.is_empty(), "{:?}", ctx.headers);
        assert_eq!(ctx.query_string, None);
        assert_eq!(ctx.remote_endpoint, None);
    }
}
