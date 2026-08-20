//! Request authentication — Jellyfin's `MediaBrowser`/`X-Emby-Token` scheme.
//!
//! Jellyfin clients present their access token in one of several places:
//!
//! - `Authorization: MediaBrowser Token="…", Client="…", Device="…",
//!   DeviceId="…", Version="…"`
//! - `X-Emby-Authorization` (same grammar as `Authorization`)
//! - `X-Emby-Token` / `X-MediaBrowser-Token` (bare token)
//! - the `api_key` / `ApiKey` query parameter
//!
//! Two pieces mirror the C# `MediaBrowser.Controller.Net` layer:
//!
//! - [`auth_context_layer`] — a middleware that builds a transport-agnostic
//!   [`RequestContext`] from the request's headers + query, asks
//!   [`AuthorizationContext`] to parse it into an [`AuthorizationInfo`], and
//!   stashes that as a request extension. It never rejects: an anonymous request
//!   still gets a (default) [`AuthorizationInfo`], matching the C# behaviour
//!   where `[Authorize]` — not the context builder — enforces auth.
//! - [`RequireAuth`] — a `FromRequestParts` extractor for handlers behind
//!   `[Authorize]`. It runs [`AuthService::authenticate`], yielding the
//!   authenticated [`AuthorizationInfo`] or a `401`.

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Request};
use axum::middleware::Next;
use axum::response::Response;
use ferrofin_traits::net::RequestContext;
use ferrofin_traits::options::AuthorizationInfo;

use crate::error::ApiError;
use crate::state::AppState;

/// Builds a [`RequestContext`] from an axum request's headers and query string.
///
/// Copies every header as a `(name, value)` pair (dropping non-UTF-8 values,
/// which Jellyfin's ASCII header grammar never uses) and carries the raw query
/// string so the authorization logic can read the `api_key`/`ApiKey` parameter.
pub(crate) fn request_context(
    headers: &HeaderMap,
    query: Option<&str>,
    remote: Option<String>,
) -> RequestContext {
    // Sized up front: `filter_map` reports a zero lower bound, so `collect` would
    // otherwise regrow this vector several times for every request that arrives.
    let mut copied = Vec::with_capacity(headers.len());
    copied.extend(headers.iter().filter_map(|(name, value)| {
        value
            .to_str()
            .ok()
            .map(|v| (name.as_str().to_owned(), v.to_owned()))
    }));
    RequestContext {
        headers: copied,
        query_string: query.map(ToOwned::to_owned),
        remote_endpoint: remote,
    }
}

/// Middleware that resolves each request's [`AuthorizationInfo`] and stores it as
/// a request extension for downstream handlers and the [`RequireAuth`] extractor.
///
/// Mounted with [`axum::middleware::from_fn_with_state`]. It is non-rejecting:
/// a request that fails to parse still proceeds with a default
/// [`AuthorizationInfo`] (`is_authenticated == false`), so public routes keep
/// working and protected routes fail later in [`RequireAuth`].
///
/// # Errors
///
/// Never returns `Err`; the signature carries [`ApiError`] only so it composes
/// with the router's other fallible layers.
pub async fn auth_context_layer(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    let ctx = request_context(
        &parts.headers,
        parts.uri.query(),
        None, // the remote address layer is wired at the composition root
    );
    let info = state
        .auth_context()
        .get_authorization_info(&ctx)
        .await
        .unwrap_or_default();
    parts.extensions.insert(info);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// Extractor for handlers behind Jellyfin's `[Authorize]` policy.
///
/// Reads the [`AuthorizationInfo`] that [`auth_context_layer`] already resolved
/// and stashed as a request extension. When the extension carries an
/// authenticated context the extractor returns immediately — no DB, no
/// header-parsing, nothing. Only when the extension is absent or anonymous
/// does it fall through to [`AuthService::authenticate`], which is the old
/// per-handler path and serves as a safety net for tests that wire
/// `FakeAuthContext` (unauthenticated) alongside `AuthedAuthService`.
#[derive(Debug, Clone)]
pub struct RequireAuth(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(info) = parts
            .extensions
            .get::<AuthorizationInfo>()
            .filter(|i| i.is_authenticated)
        {
            return Ok(Self(info.clone()));
        }
        let ctx = request_context(&parts.headers, parts.uri.query(), None);
        let info = state.auth_service().authenticate(&ctx).await?;
        Ok(Self(info))
    }
}

/// Extractor for handlers behind Jellyfin's `FirstTimeSetupOrDefault` policy.
///
/// Port of `FirstTimeSetupHandler`: while the startup wizard is **not** complete
/// (`!IsStartupWizardCompleted`), the endpoint is reachable anonymously — the
/// first-run web wizard hits e.g. `/Localization/Options` before any user exists.
/// Once setup is complete it behaves like [`RequireAuth`] (a valid token is
/// required, else `401`). The inner `Option` is `Some` when a token was validated,
/// `None` for an anonymous first-time-setup request.
#[derive(Debug, Clone)]
pub struct FirstTimeSetupOrAuth(pub Option<AuthorizationInfo>);

impl FromRequestParts<AppState> for FirstTimeSetupOrAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ctx = request_context(&parts.headers, parts.uri.query(), None);
        // Wizard incomplete → allow anonymous (still surface a token if one is
        // present, but never reject). Treat a config read error as "not complete"
        // so a fresh install can never lock itself out of its own setup wizard.
        let wizard_complete = state
            .config
            .configuration()
            .await
            .is_ok_and(|c| c.is_startup_wizard_completed);
        if !wizard_complete {
            return Ok(Self(state.auth_service().authenticate(&ctx).await.ok()));
        }
        // Setup complete → require a valid token.
        let info = state.auth_service().authenticate(&ctx).await?;
        Ok(Self(Some(info)))
    }
}

#[cfg(test)]
mod tests {
    use super::request_context;
    use axum::http::HeaderMap;

    #[test]
    fn request_context_copies_headers_and_query() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Emby-Token", "abc123".parse().unwrap());
        let ctx = request_context(&headers, Some("api_key=xyz"), Some("1.2.3.4".to_owned()));
        assert_eq!(ctx.header("x-emby-token"), Some("abc123"));
        assert_eq!(ctx.query_string.as_deref(), Some("api_key=xyz"));
        assert_eq!(ctx.remote_endpoint.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn request_context_skips_non_utf8_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Bytes",
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        let ctx = request_context(&headers, None, None);
        assert!(ctx.header("x-bytes").is_none());
        assert_eq!(ctx.query_string, None);
    }
}
