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

/// Extractor for handlers behind Jellyfin's `RequiresElevation` policy.
///
/// Port of `Policies.RequiresElevation`, which upstream declares on 74
/// controller actions: the caller must be an API key, or a user whose policy
/// grants `IsAdministrator`. Anything else is `403`.
///
/// This exists because several controllers carried a comment saying elevation
/// was "applied at the composition root's auth layer" — a layer that was never
/// built. The result was that every elevation-gated route was reachable by any
/// authenticated account: an ordinary user could `POST /Users/{ownId}/Policy`
/// with `IsAdministrator: true` and be an administrator in one request, or read
/// `GET /Devices` and harvest the administrator's plaintext access token. Gate
/// in the extractor, never in a comment.
///
/// An API key is elevated, matching C# — `Policies.RequiresElevation` is
/// satisfied by the `ApiKey` scheme without a user attached.
#[derive(Debug, Clone)]
pub struct RequireAdmin(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let RequireAuth(info) = RequireAuth::from_request_parts(parts, state).await?;
        if info.is_api_key {
            return Ok(Self(info));
        }
        if let Some(user) = &info.user
            && crate::handlers::users::is_administrator(state, user).await?
        {
            return Ok(Self(info));
        }
        Err(ApiError::Forbidden(
            "administrator access required".to_owned(),
        ))
    }
}

/// Extractor for handlers behind Jellyfin's `LiveTvAccess` policy.
///
/// Port of `options.AddPolicy(Policies.LiveTvAccess, new
/// UserPermissionRequirement(PermissionKind.EnableLiveTvAccess))`
/// (v10.11.8 ApiServiceCollectionExtensions.cs:80), evaluated by
/// `UserPermissionHandler`: an API key carries global permissions and succeeds
/// outright; any other caller must hold the permission, and anything else is
/// `403`.
///
/// v10.11.8's `LiveTvController` declares this on 22 read actions. Ferrofin
/// served them under plain `RequireAuth`, so "Allow Live TV access" was a
/// checkbox the dashboard rendered and the server ignored — an account with it
/// cleared could still browse the whole guide. `EnableLiveTvAccess` defaults to
/// `true` (UserEntityExtensions.cs:187), which is why a single-admin fixture
/// never noticed, and why this gate changes nothing for a stock account.
///
/// Unlike [`RequireLiveTvManagement`], this one is a bare permission check: the
/// disagreement measured there is between "administrator" and "permission
/// denied", and it cannot arise here, because no shipped account has
/// `EnableLiveTvAccess` cleared unless an administrator cleared it on purpose.
#[derive(Debug, Clone)]
pub struct RequireLiveTvAccess(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireLiveTvAccess {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let info = require_live_tv_permission(parts, state, |p| p.enable_live_tv_access).await?;
        Ok(Self(info))
    }
}

/// Extractor for handlers behind Jellyfin's `LiveTvManagement` policy —
/// administrator **or** the `EnableLiveTvManagement` permission.
///
/// v10.11.8's `LiveTvController` declares
/// `[Authorize(Policy = Policies.LiveTvManagement)]` on its seven
/// timer/recording mutations, registered as
/// `UserPermissionRequirement(PermissionKind.EnableLiveTvManagement)`. Reading
/// only that, this would be a bare permission check.
///
/// It is not, because the permission check alone does not reproduce what a real
/// Jellyfin 10.11.8 does. Measured against the lab oracle: for a caller who is
/// an administrator and whose stored `EnableLiveTvManagement` permission is
/// `0` (the shipped default — `UserEntityExtensions.cs:188`), Jellyfin served
/// `DELETE /LiveTv/Timers/{id}` (404 from the handler) and
/// `POST /LiveTv/SeriesTimers` rather than `403`; the same held for an
/// unrelated `UserPermissionRequirement` policy, `CollectionManagement` with
/// `EnableCollectionManagement = 0`, on `POST /Collections` (200). So an
/// administrator is not refused by these policies in practice, whatever
/// `UserPermissionHandler` reads like in isolation.
///
/// Admitting *either* is therefore the only gate that matches the oracle on
/// both sides of the disagreement: it never refuses the administrator Jellyfin
/// admitted, and it admits the non-administrator the dashboard explicitly
/// granted recording rights — whom Ferrofin's previous `RequireAdmin` refused,
/// and who is the entire point of the permission existing. It is also strictly
/// tighter than what Ferrofin shipped on most of these routes, which was plain
/// `RequireAuth`: any authenticated account could create and cancel timers.
#[derive(Debug, Clone)]
pub struct RequireLiveTvManagement(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireLiveTvManagement {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let info = require_live_tv_permission(parts, state, |p| {
            p.is_administrator || p.enable_live_tv_management
        })
        .await?;
        Ok(Self(info))
    }
}

/// The shared body of the two Live TV permission extractors.
///
/// Port of `UserPermissionHandler.HandleRequirement`: "Api keys have global
/// permissions, so just succeed the requirement"; otherwise the user must hold
/// the permission the policy names.
async fn require_live_tv_permission(
    parts: &mut Parts,
    state: &AppState,
    granted: fn(&ferrofin_model::users::UserPolicy) -> bool,
) -> Result<AuthorizationInfo, ApiError> {
    let RequireAuth(info) = RequireAuth::from_request_parts(parts, state).await?;
    if info.is_api_key {
        return Ok(info);
    }
    if let Some(user) = &info.user
        && crate::handlers::users::user_policy(state, user)
            .await?
            .is_some_and(|p| granted(&p))
    {
        return Ok(info);
    }
    Err(ApiError::Forbidden("live tv access required".to_owned()))
}

/// Extractor for handlers behind Jellyfin's `LocalAccessOrRequiresElevation`
/// policy — in the vendored contract, `POST /System/Restart` alone.
///
/// Port of `LocalAccessOrRequiresElevationHandler`: a caller whose peer address
/// is on the local network is allowed regardless of role; anyone else must be an
/// administrator. Restarting the server is the kind of thing someone standing at
/// the machine should be able to do, and the kind of thing a stranger on the
/// internet should not.
///
/// Two deliberate divergences, both in the safe direction:
///
/// 1. **Authentication is still required.** Upstream registers this policy as
///    `AddPolicy(name, new LocalAccessOrRequiresElevationRequirement())` — the
///    requirement list replaces the default policy wholesale and nothing in it
///    demands an authenticated user, so a LAN caller with no token at all
///    satisfies it. Ferrofin keeps [`RequireAuth`] in front, so an anonymous
///    request is `401` whatever its source address. No authenticated client can
///    tell the difference.
/// 2. **An unknown peer address is treated as remote.** C# accepts a null IP
///    ("Loopback will be on LAN, so we can accept null"), but in Ferrofin the
///    peer address is missing only when there is no connection to ask —
///    synthetic routing in tests, never a served request, since the composition
///    root installs `with_connect_info`. Failing closed there costs a real
///    caller nothing and keeps the unknown case from being the permissive one.
///
/// The local-network test is [`crate::handlers::system::is_in_local_network`],
/// shared with `GET /System/Endpoint` so the endpoint that *reports* whether a
/// client is in-network and the policy that *acts* on it can never disagree.
///
/// Two limits of that test, both of which make this arm **wider** than
/// upstream's. Neither is a regression — before this gate the route took plain
/// [`RequireAuth`], so any account could restart from anywhere, and both cases
/// below still land on "authenticated caller only". But do not read the gate as
/// stronger than it is:
///
/// - **Reverse proxies.** The peer address is the transport peer, and nothing
///   here consumes `X-Forwarded-For`. Behind a proxy or ingress — including
///   this repo's own `charts/ferrofin/templates/ingress.yaml` — every request
///   presents the proxy's address, which is private, so the local arm is
///   satisfied for all callers and the gate degrades to "any authenticated
///   account". Upstream needs `KnownProxies` configured to do better;
///   `NetworkConfiguration::known_proxies` exists here but nothing reads it
///   yet.
/// - **Configured subnets.** This now asks
///   [`AppState::is_in_local_network`], which is
///   `NetworkManager.IsInLocalNetwork` — `LocalNetworkSubnets` intersected and
///   the `!`-prefixed exclusions subtracted — whenever the composition root has
///   wired the policy. Without it (unit tests) the private-range fallback still
///   applies.
#[derive(Debug, Clone)]
pub struct RequireLocalAccessOrAdmin(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireLocalAccessOrAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Read the peer before `RequireAuth` borrows `parts` mutably.
        // The CLIENT's address, not the transport peer: behind a proxy the peer
        // is always the proxy, which is private, so every caller would read as
        // local and this gate would degrade to "any authenticated account".
        //
        // The `is_some` is load-bearing and stays: `client_address` defaults a
        // MISSING peer to loopback (C# `GetNormalizedRemoteIP` does), and
        // "there was no connection to ask" must keep reading as remote here —
        // the deliberate divergence documented on this extractor, and the only
        // one of the two directions that is safe to be wrong in.
        let local = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .is_some()
            && state.is_in_local_network(state.client_address(parts));
        if local {
            let RequireAuth(info) = RequireAuth::from_request_parts(parts, state).await?;
            return Ok(Self(info));
        }
        let RequireAdmin(info) = RequireAdmin::from_request_parts(parts, state).await?;
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

/// Extractor for handlers behind Jellyfin's `FirstTimeSetupOrElevated` policy.
///
/// Port of `FirstTimeSetupHandler` with `RequireAdmin` set
/// (Jellyfin.Api/Auth/FirstTimeSetupPolicy/FirstTimeSetupHandler.cs:27-44):
/// while the startup wizard is **not** complete the endpoint is reachable
/// anonymously — the first-run web wizard has no account to authenticate with —
/// and once setup is complete it requires an ADMINISTRATOR, not merely a valid
/// token.
///
/// This is what gates the whole `StartupController` upstream
/// (`[Authorize(Policy = Policies.FirstTimeSetupOrElevated)]`,
/// StartupController.cs:18). Ungated, `POST /Startup/User` lets an anonymous
/// caller rename the first administrator and set its password on a fully
/// configured server.
#[derive(Debug, Clone)]
pub struct FirstTimeSetupOrElevated(pub Option<AuthorizationInfo>);

impl FromRequestParts<AppState> for FirstTimeSetupOrElevated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ctx = request_context(&parts.headers, parts.uri.query(), None);
        // A config read error reads as "not complete", exactly as in
        // `FirstTimeSetupOrAuth`: a fresh install must never be able to lock
        // itself out of its own setup wizard.
        let wizard_complete = state
            .config
            .configuration()
            .await
            .is_ok_and(|c| c.is_startup_wizard_completed);
        if !wizard_complete {
            return Ok(Self(state.auth_service().authenticate(&ctx).await.ok()));
        }
        let RequireAdmin(info) = RequireAdmin::from_request_parts(parts, state).await?;
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
