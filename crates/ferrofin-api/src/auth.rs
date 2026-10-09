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
    // C# `RequestHelpers.GetSession` re-logs session activity with
    // `httpContext.GetNormalizedRemoteIP().ToString()` on every request, so a
    // session's `RemoteEndPoint` tracks a roaming client. Without it every
    // session reports an empty address and every activity-log `ShortOverview`
    // built from it is null.
    let remote = state.client_address(&parts).to_string();
    let ctx = request_context(&parts.headers, parts.uri.query(), Some(remote));
    let info = state
        .auth_context()
        .get_authorization_info(&ctx)
        .await
        .unwrap_or_default();
    parts.extensions.insert(info);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// Resolves credentials without applying a route's authorization policy.
async fn authenticated_info(
    parts: &mut Parts,
    state: &AppState,
) -> Result<AuthorizationInfo, ApiError> {
    if let Some(info) = parts
        .extensions
        .get::<AuthorizationInfo>()
        .filter(|info| info.is_authenticated)
    {
        return Ok(info.clone());
    }
    let ctx = request_context(
        &parts.headers,
        parts.uri.query(),
        Some(state.client_address(parts).to_string()),
    );
    let info = state.auth_service().authenticate(&ctx).await?;
    if !info.is_authenticated {
        return Err(ApiError::Unauthorized("invalid credentials".to_owned()));
    }
    Ok(info)
}

/// Authenticated identity without a route authorization policy. Public routes
/// use `Result<AuthenticatedIdentity, ApiError>` to retain an optional caller
/// for visibility checks even when remote access or schedules deny other routes.
#[derive(Debug, Clone)]
pub struct AuthenticatedIdentity(pub AuthorizationInfo);
impl FromRequestParts<AppState> for AuthenticatedIdentity {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(authenticated_info(parts, state).await?))
    }
}

/// Applies DefaultAuthorizationHandler in upstream order. Production contexts
/// carry a shared policy from the token cache; alternate injected auth services
/// can resolve it through UserManager once here.
async fn default_authorization(
    mut info: AuthorizationInfo,
    parts: &Parts,
    state: &AppState,
    validate_schedule: bool,
) -> Result<AuthorizationInfo, ApiError> {
    if info.is_api_key {
        return Ok(info);
    }
    if info.user_id().is_nil() {
        return Err(ApiError::Forbidden("user identity required".to_owned()));
    }
    if info.user_policy.is_none()
        && let Some(user) = &info.user
    {
        info.user_policy = crate::handlers::users::user_policy(state, user)
            .await?
            .map(std::sync::Arc::new);
    }
    let policy = info
        .user_policy
        .as_ref()
        .ok_or_else(|| ApiError::Forbidden("user policy required".to_owned()))?;
    if policy.is_disabled {
        return Err(ApiError::Unauthorized("account disabled".to_owned()));
    }
    if !state.is_in_local_network(state.client_address(parts)) && !policy.enable_remote_access {
        return Err(ApiError::Forbidden(
            "remote access is not permitted for this user".to_owned(),
        ));
    }
    if !policy.is_administrator
        && validate_schedule
        && !parental_schedule_allows(&policy.access_schedules, chrono::Local::now())
    {
        return Err(ApiError::Forbidden(
            "outside this user's access schedule".to_owned(),
        ));
    }
    Ok(info)
}

/// Jellyfin's default policy: authenticated API key or user, with remote access
/// and the current access schedule checked on every request.
#[derive(Debug, Clone)]
pub struct RequireAuth(pub AuthorizationInfo);
impl FromRequestParts<AppState> for RequireAuth {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let info = authenticated_info(parts, state).await?;
        Ok(Self(default_authorization(info, parts, state, true).await?))
    }
}

/// Default authorization with the schedule exemption used by GetUserById.
/// Remote-access restrictions still apply.
#[derive(Debug, Clone)]
pub struct RequireAuthIgnoringSchedule(pub AuthorizationInfo);
impl FromRequestParts<AppState> for RequireAuthIgnoringSchedule {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let info = authenticated_info(parts, state).await?;
        Ok(Self(
            default_authorization(info, parts, state, false).await?,
        ))
    }
}

/// Elevation combined with a controller-level default policy, as on
/// ConfigurationController and ItemLookupController. Unlike bare elevation,
/// these routes also enforce the user's remote-access restriction.
#[derive(Debug, Clone)]
pub struct RequireAdminWithDefault(pub AuthorizationInfo);
impl FromRequestParts<AppState> for RequireAdminWithDefault {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let RequireAdmin(info) = RequireAdmin::from_request_parts(parts, state).await?;
        Ok(Self(default_authorization(info, parts, state, true).await?))
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
        let info = authenticated_info(parts, state).await?;
        if info.is_api_key {
            return Ok(Self(info));
        }
        if let Some(user) = &info.user
            && match &info.user_policy {
                Some(policy) => policy.is_administrator,
                None => crate::handlers::users::is_administrator(state, user).await?,
            }
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
/// (v10.11.8 `ApiServiceCollectionExtensions.cs:80`). v10.11.8's
/// `LiveTvController` declares it on 22 read actions. Ferrofin served them under
/// plain [`RequireAuth`], so "Allow Live TV access" was a checkbox the dashboard
/// rendered and the server ignored — an account with it cleared could still
/// browse the whole guide.
///
/// The policy itself is [`require_user_permission`], which both Live TV
/// extractors share; this one names `EnableLiveTvAccess` as the permission the
/// non-administrator arm demands. `EnableLiveTvAccess` defaults to `true`
/// (`UserEntityExtensions.cs:187`), which is why a stock account sees no change.
#[derive(Debug, Clone)]
pub struct RequireLiveTvAccess(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireLiveTvAccess {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (info, _) = require_user_permission(parts, state, |p| p.enable_live_tv_access).await?;
        Ok(Self(info))
    }
}

/// Extractor for handlers behind Jellyfin's `LiveTvManagement` policy.
///
/// Port of `options.AddPolicy(Policies.LiveTvManagement, new
/// UserPermissionRequirement(PermissionKind.EnableLiveTvManagement))`
/// (v10.11.8 `ApiServiceCollectionExtensions.cs:81`), declared on the
/// controller's seven timer/recording mutations. Ferrofin shipped most of them
/// under plain [`RequireAuth`] — any authenticated account could cancel a
/// timer — and the rest under [`RequireAdmin`], which refused the
/// non-administrator the dashboard had explicitly granted recording rights.
///
/// The policy itself is [`require_user_permission`]; this one names
/// `EnableLiveTvManagement`, which defaults to `false`
/// (`UserEntityExtensions.cs:188`).
#[derive(Debug, Clone)]
pub struct RequireLiveTvManagement(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireLiveTvManagement {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (info, _) =
            require_user_permission(parts, state, |p| p.enable_live_tv_management).await?;
        Ok(Self(info))
    }
}

/// Jellyfin's `SubtitleManagement` policy for upload, remote search/download
/// and remote subtitle retrieval. Deleting subtitles still requires elevation.
#[derive(Debug, Clone)]
pub struct RequireSubtitleManagement(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireSubtitleManagement {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (info, _) =
            require_user_permission(parts, state, |p| p.enable_subtitle_management).await?;
        Ok(Self(info))
    }
}

/// Enforces Jellyfin's `Download` policy before resolving or serving a file.
#[derive(Debug, Clone)]
pub struct RequireDownload(
    pub AuthorizationInfo,
    pub Option<ferrofin_model::users::UserPolicy>,
);

impl FromRequestParts<AppState> for RequireDownload {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (info, policy) =
            require_user_permission(parts, state, |p| p.enable_content_downloading).await?;
        Ok(Self(info, policy))
    }
}

/// The shared body of the user permission extractors — a port of the
/// **whole** `UserPermissionRequirement` policy, which is two handlers deep.
///
/// Read alone, `UserPermissionHandler` (v10.11.8
/// `Jellyfin.Api/Auth/UserPermissionPolicy/UserPermissionHandler.cs`) is a bare
/// permission check: API key succeeds, otherwise `user.HasPermission(kind)`.
/// That is not the policy. `UserPermissionRequirement` **subclasses**
/// `DefaultAuthorizationRequirement` (`UserPermissionRequirement.cs:9`), and
/// `DefaultAuthorizationHandler` — registered as an `IAuthorizationHandler`
/// first, "so that it is evaluated first"
/// (`ApiServiceCollectionExtensions.cs:59`) — is an
/// `AuthorizationHandler<DefaultAuthorizationRequirement>`, whose base
/// `HandleAsync` dispatches over `context.Requirements.OfType<TRequirement>()`
/// and therefore matches every subclass. So `DefaultAuthorizationHandler` runs
/// against these permission policies as well, and its own closing comment says so:
/// "Only succeed if the requirement isn't a subclass as any subclassed
/// requirement will handle success in its own handler"
/// (`DefaultAuthorizationHandler.cs:86-90`).
///
/// The composed decision, in `DefaultAuthorizationHandler`'s order — the order
/// matters, because ASP.NET Core's `context.Fail()` is unconditional and beats
/// any `Succeed`, so an arm's *position* relative to the admin arm decides
/// whether an administrator escapes it:
///
/// 1. **API key → allow** (`:53-57`, and `UserPermissionHandler` agrees):
///    "Api keys are unrestricted."
/// 2. **Remote caller without `EnableRemoteAccess` → deny** (`:66-70`). Before
///    the admin arm, so it refuses a remote administrator too.
/// 3. **Administrator → allow** (`:73-77`): "Admins can do everything." This is
///    the arm the batch that added these extractors missed for `LiveTvAccess`.
///    It is not a lab artefact: it is why a real 10.11.8 served
///    `DELETE /LiveTv/Timers/{id}` and `POST /LiveTv/SeriesTimers` to an
///    administrator whose stored `EnableLiveTvManagement` was `0` (the shipped
///    default), and `POST /Collections` to the same caller under the unrelated
///    `CollectionManagement` policy with `EnableCollectionManagement = 0`.
///    Both measurements are of this one arm, and it is not per-policy: it
///    governs `LiveTvAccess` exactly as it governs `LiveTvManagement`.
/// 4. **Outside the parental/access schedule → deny** (`:81-84`).
///    `UserPermissionRequirement` leaves `validateParentalSchedule` at its
///    default `true` (`UserPermissionRequirement.cs:17`). After the admin arm,
///    so an administrator is not schedule-bound.
/// 5. **The named permission** — `DefaultAuthorizationHandler` deliberately does
///    *not* succeed a subclassed requirement (`:87-90`), leaving
///    `UserPermissionHandler` to allow only a holder of the permission.
///
/// Anything that reaches neither an allow arm nor a `Fail` is `403`: the
/// requirement is simply never satisfied.
async fn require_user_permission(
    parts: &mut Parts,
    state: &AppState,
    permission: fn(&ferrofin_model::users::UserPolicy) -> bool,
) -> Result<(AuthorizationInfo, Option<ferrofin_model::users::UserPolicy>), ApiError> {
    let RequireAuth(info) = RequireAuth::from_request_parts(parts, state).await?;
    if info.is_api_key {
        return Ok((info, None));
    }
    let policy = info
        .user_policy
        .as_deref()
        .cloned()
        .ok_or_else(|| ApiError::Forbidden("user permission required".to_owned()))?;
    if policy.is_administrator {
        return Ok((info, Some(policy)));
    }
    // 5. The permission the policy names.
    if permission(&policy) {
        return Ok((info, Some(policy)));
    }
    Err(ApiError::Forbidden("user permission required".to_owned()))
}

/// Whether `now` falls inside any of `schedules`, or there are none — C#
/// `UserEntityExtensions.IsParentalScheduleAllowed` (`:148-152`, `:210-220`).
///
/// C# evaluates `DateTime.UtcNow.ToLocalTime()`, i.e. server-local wall clock,
/// and compares `TimeOfDay.TotalHours` against the schedule's fractional
/// `StartHour`/`EndHour` inclusively at both ends. `ferrofin-core` already ports
/// this over the `AccessSchedules` table for the login path
/// (`user_entity_ext::is_parental_schedule_allowed`); this is the same rule over
/// the [`UserPolicy`](ferrofin_model::users::UserPolicy) projection of those
/// rows, because `ferrofin-api` may not depend on `ferrofin-core` and the policy
/// DTO already carries them.
fn parental_schedule_allows(
    schedules: &[ferrofin_model::users::AccessSchedule],
    now: chrono::DateTime<chrono::Local>,
) -> bool {
    use chrono::{Datelike as _, Timelike as _};

    if schedules.is_empty() {
        return true;
    }
    // TimeOfDay.TotalHours includes subsecond time: the first fraction of a
    // second after EndHour must not remain inside the allowed window.
    let hour = (f64::from(now.num_seconds_from_midnight())
        + f64::from(now.nanosecond()) / 1_000_000_000.0)
        / 3600.0;
    let weekday = now.date_naive().weekday();
    schedules.iter().any(|s| {
        day_of_week_contains(s.day_of_week, weekday) && hour >= s.start_hour && hour <= s.end_hour
    })
}

/// Whether a [`DynamicDayOfWeek`](ferrofin_model::users::DynamicDayOfWeek)
/// covers `weekday` — C# `DayOfWeekHelper.Contains` (`Jellyfin.Data/DayOfWeekHelper.cs:21-31`).
fn day_of_week_contains(
    day: ferrofin_model::users::DynamicDayOfWeek,
    weekday: chrono::Weekday,
) -> bool {
    use chrono::Weekday;
    use ferrofin_model::users::DynamicDayOfWeek as D;
    match day {
        D::Everyday => true,
        D::Weekday => !matches!(weekday, Weekday::Sat | Weekday::Sun),
        D::Weekend => matches!(weekday, Weekday::Sat | Weekday::Sun),
        D::Sunday => weekday == Weekday::Sun,
        D::Monday => weekday == Weekday::Mon,
        D::Tuesday => weekday == Weekday::Tue,
        D::Wednesday => weekday == Weekday::Wed,
        D::Thursday => weekday == Weekday::Thu,
        D::Friday => weekday == Weekday::Fri,
        D::Saturday => weekday == Weekday::Sat,
        D::Unrecognized(_) => false,
    }
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
            let info = authenticated_info(parts, state).await?;
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
            let info = state.auth_service().authenticate(&ctx).await.ok();
            let info = match info {
                Some(info) if info.is_api_key || !info.user_id().is_nil() => {
                    Some(default_authorization(info, parts, state, false).await?)
                }
                other => other,
            };
            return Ok(Self(info));
        }
        let info = authenticated_info(parts, state).await?;
        Ok(Self(Some(
            default_authorization(info, parts, state, false).await?,
        )))
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
            let info = state.auth_service().authenticate(&ctx).await.ok();
            let info = match info {
                Some(info) if info.is_api_key || !info.user_id().is_nil() => {
                    Some(default_authorization(info, parts, state, false).await?)
                }
                other => other,
            };
            return Ok(Self(info));
        }
        let RequireAdmin(info) = RequireAdmin::from_request_parts(parts, state).await?;
        Ok(Self(Some(
            default_authorization(info, parts, state, false).await?,
        )))
    }
}

/// Extractor for handlers behind Jellyfin's `LyricManagement` policy.
///
/// Port of `new UserPermissionRequirement(PermissionKind.EnableLyricManagement)`
/// (`ApiServiceCollectionExtensions.cs:88`, v10.11.8), which TWO handlers see:
///
/// 1. `UserPermissionHandler` — an API key has global permissions and succeeds
///    outright; otherwise the user must hold `EnableLyricManagement`, which
///    `UserEntityExtensions.AddDefaultPermissions` initialises to **false**.
/// 2. `DefaultAuthorizationHandler` — because `UserPermissionRequirement`
///    derives from `DefaultAuthorizationRequirement`, the default handler runs
///    for this requirement too, and its "Admins can do everything" branch
///    calls `context.Succeed(requirement)` before the permission is ever
///    consulted. So an administrator passes with the permission set to false.
///
/// That second leg is not a guess: measured on the lab pair, Jellyfin 10.11.8
/// accepts a lyric upload from the bench administrator whose
/// `Policy.EnableLyricManagement` reads `false`.
///
/// `LyricsController` puts the policy on five of its six actions; only
/// `GetLyrics` is a plain `[Authorize]`. Without it every authenticated
/// account could overwrite and delete any other user's lyrics — the same
/// "gated in a comment, not in code" hole [`RequireAdmin`] exists to close, and
/// invisible to a parity run whose bench user is an administrator.
///
/// The default handler's other two legs — refusing a remote caller without
/// `EnableRemoteAccess`, and the parental schedule — belong to the base
/// `[Authorize]` policy that [`RequireAuth`] stands for and are not ported
/// here (or in [`RequireAdmin`]); they are open work on the default policy,
/// not something specific to lyrics.
#[derive(Debug, Clone)]
pub struct RequireLyricManagement(pub AuthorizationInfo);

impl FromRequestParts<AppState> for RequireLyricManagement {
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
            && state
                .users
                .get_user_dto(user, None)
                .await?
                .policy
                .is_some_and(|p| p.is_administrator || p.enable_lyric_management)
        {
            return Ok(Self(info));
        }
        Err(ApiError::Forbidden(
            "lyric management access required".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::request_context;
    use axum::http::HeaderMap;

    #[test]
    fn access_schedule_preserves_fractional_second_boundaries() {
        use chrono::TimeZone as _;
        for (hour, minute, second, nanos, expected) in [
            (9, 29, 59, 999_999_900, false),
            (9, 30, 0, 0, true),
            (10, 30, 0, 0, true),
            (10, 30, 0, 100, false),
            (10, 30, 0, 500_000_000, false),
        ] {
            let local = chrono::NaiveDate::from_ymd_opt(2026, 10, 5)
                .unwrap()
                .and_hms_nano_opt(hour, minute, second, nanos)
                .unwrap();
            let now = chrono::Local.from_local_datetime(&local).single().unwrap();
            let schedule = ferrofin_model::users::AccessSchedule {
                day_of_week: ferrofin_model::users::DynamicDayOfWeek::Everyday,
                start_hour: 9.5,
                end_hour: 10.5,
                ..Default::default()
            };
            assert_eq!(
                super::parental_schedule_allows(&[schedule], now),
                expected,
                "{local}"
            );
        }
    }

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
