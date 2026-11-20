//! Single Sign-On manager trait — the DI seam for the `/sso` routes.
//!
//! Ferrofin's own plugin (D1, D15 of the "OIDC sign-in" milestone): no
//! Jellyfin or community plugin surface is ported. The concrete implementation
//! lives with the extension (`ferrofin-extensions::sso`), so every piece of the
//! plugin is grouped there; `ferrofin-api` handlers reach it only through this
//! object-safe trait.

use std::net::IpAddr;

use async_trait::async_trait;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcDiscovery, OidcProvider, OidcProviderInfo, OidcProviderList};

use crate::error::ServiceError;
use crate::session::{AuthenticationRequest, AuthenticationResultData};

/// The cookie that binds a pending OIDC sign-in to the browser that started
/// it: [`SsoManager::start_oidc_sign_in`] sets it, the callback route reads it
/// back into [`OidcCallback::flow_cookie`], and the exchange route into
/// [`SsoManager::exchange_oidc_sign_in`]'s `flow_cookie`.
pub const OIDC_FLOW_COOKIE: &str = "ferrofin_sso_flow";

/// What a browser-facing Single Sign-On route answers: a redirect when
/// [`location`](Self::location) is set, else an HTML page, with the cookies
/// to set either way. `Debug` leaves the cookies, the page (the completion
/// page holds an exchange code) and the redirect's query (its `state` and
/// nonce) out.
#[derive(Clone, PartialEq, Eq)]
pub struct SsoReply {
    /// The status code: `302` for a redirect.
    pub status: u16,
    /// Where a redirect sends the browser.
    pub location: Option<String>,
    /// The page; empty for a redirect.
    pub html: String,
    /// Each `Set-Cookie` header value, in order.
    pub set_cookies: Vec<String>,
    /// The page's `Content-Security-Policy`, which names the one script it
    /// may run, if any.
    pub content_security_policy: Option<String>,
}

impl std::fmt::Debug for SsoReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsoReply")
            .field("status", &self.status)
            .field(
                "location",
                &self.location.as_deref().and_then(|l| l.split('?').next()),
            )
            .field("html", &self.html.len())
            .field("set_cookies", &self.set_cookies.len())
            .finish_non_exhaustive()
    }
}

/// What `POST /sso/oidc/exchange` answers: the session the exchange code was
/// swapped for, or why there is none, and the cookies to set either way.
#[derive(Debug, Clone, PartialEq)]
pub struct OidcExchangeReply {
    /// The session, its token included, or the refusal.
    pub outcome: Result<AuthenticationResultData, OidcExchangeRefusal>,
    /// Each `Set-Cookie` header value, in order: the flow cookie, cleared.
    pub set_cookies: Vec<String>,
}

/// An exchange that opened no session: its status (`400` for a code or
/// cookie that does not check out, `403` for an account the sign-in rules
/// refuse) and the sentence the completion page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcExchangeRefusal {
    /// The status code.
    pub status: u16,
    /// What the completion page tells the user.
    pub message: String,
}

/// What the identity provider's redirect brought back to
/// `GET /sso/oidc/{provider}/callback`: its query parameters, and the
/// browser's [`OIDC_FLOW_COOKIE`]. A callback that repeats any of these
/// parameters is read as having none of them. `Debug` says only which are
/// present.
#[derive(Clone, Default)]
pub struct OidcCallback {
    /// `state`: names the pending sign-in.
    pub state: Option<String>,
    /// `code`: the authorization code to exchange.
    pub code: Option<Secret>,
    /// `iss`: the issuer that answered (RFC 9207).
    pub iss: Option<String>,
    /// `error`: the identity provider refused or failed the sign-in.
    pub error: Option<String>,
    /// The value of the browser's [`OIDC_FLOW_COOKIE`].
    pub flow_cookie: Option<Secret>,
}

impl std::fmt::Debug for OidcCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcCallback")
            .field("state", &self.state.is_some())
            .field("code", &self.code.is_some())
            .field("iss", &self.iss.is_some())
            .field("error", &self.error.is_some())
            .field("flow_cookie", &self.flow_cookie.is_some())
            .finish()
    }
}

/// The Single Sign-On plugin's operations.
///
/// Every method self-gates on the plugin's enabled flag, reporting
/// [`ServiceError::NotFound`] while it is disabled, so a disabled plugin's
/// routes answer as if they did not exist (B1).
#[async_trait]
pub trait SsoManager: Send + Sync {
    /// Succeeds while the plugin is enabled, and is
    /// [`ServiceError::NotFound`] otherwise, so a route can answer `404`
    /// before it looks at the caller or the request body.
    async fn ensure_enabled(&self) -> Result<(), ServiceError>;

    /// The stored OIDC providers, each with whether a client secret is stored
    /// and its callback URL, and never a secret.
    ///
    /// A stored provider these routes would refuse (the plugin configuration
    /// route and the file on disk take anything) is listed with its
    /// [`validation_error`](OidcProviderInfo::validation_error), and is
    /// treated as disabled.
    async fn oidc_providers(&self) -> Result<OidcProviderList, ServiceError>;

    /// Creates the provider `key`, or replaces its settings, and returns it as
    /// [`oidc_providers`](Self::oidc_providers) lists it.
    ///
    /// Its text settings are trimmed. A `provider` without a client secret
    /// (or with an empty or blank one) keeps the stored secret, and one with
    /// no secret stored, a new one included, needs one. A secret is never
    /// trimmed: one with control characters or surrounding spaces is refused.
    ///
    /// [`ServiceError::InvalidInput`] names what is refused: a key outside 1
    /// to 32 lowercase letters, digits and `-`, a body naming another key, a
    /// control character, an empty name, client id or username claim, scopes
    /// without `openid`, or an issuer that is not an `https` URL (or `http`,
    /// with `AllowHttpIssuer`) naming a host, with no user name, password,
    /// query or fragment.
    async fn save_oidc_provider(
        &self,
        key: &str,
        provider: OidcProvider,
    ) -> Result<OidcProviderInfo, ServiceError>;

    /// Removes the provider `key` and its client secret;
    /// [`ServiceError::NotFound`] when no provider has that key.
    async fn delete_oidc_provider(&self, key: &str) -> Result<(), ServiceError>;

    /// Tests `provider`'s discovery, as a `PUT` does once it has saved it:
    /// fetches `{issuer}/.well-known/openid-configuration` and the key set it
    /// names, fresh, checks that the document names the configured issuer,
    /// and caches both for sign-in.
    ///
    /// A failed test is an [`OidcDiscovery`] with its
    /// [`error`](OidcDiscovery::error), never an `Err`: the provider stays
    /// saved and the admin is told why.
    async fn test_oidc_provider(
        &self,
        provider: &OidcProvider,
    ) -> Result<OidcDiscovery, ServiceError>;

    /// Starts a sign-in at the provider `key` for the browser at `client`:
    /// a redirect to its authorization endpoint (the code flow with PKCE
    /// `S256`, a `state` and a nonce) that sets the [`OIDC_FLOW_COOKIE`].
    /// When no callback URL can be built, a page naming the public URL
    /// setting; when the provider cannot be reached, the refusal page.
    ///
    /// [`ServiceError::NotFound`] while the plugin is disabled, and for a
    /// provider that is unknown, disabled or refused.
    async fn start_oidc_sign_in(&self, key: &str, client: IpAddr)
    -> Result<SsoReply, ServiceError>;

    /// Finishes the sign-in `callback` names, at the provider `key`. When
    /// every check passes, the verified identity's account is the one linked
    /// to its issuer and subject, else one created and linked when the
    /// provider creates accounts, and the answer is the completion page: it
    /// carries a one-time exchange code for that account, bound to the
    /// [`OIDC_FLOW_COOKIE`], which it keeps for the exchange, and its script
    /// swaps the code at [`exchange_oidc_sign_in`](Self::exchange_oidc_sign_in)
    /// and opens jellyfin-web signed in. A sign-in started with
    /// [`start_oidc_quick_connect`](Self::start_oidc_quick_connect) answers
    /// its confirmation page instead. With no account, a `403` page says
    /// what to ask an administrator; a failed check is the refusal page; both
    /// clear the cookie. Either way the pending sign-in is used up.
    ///
    /// [`ServiceError::NotFound`] as for
    /// [`start_oidc_sign_in`](Self::start_oidc_sign_in), and an error when
    /// the accounts cannot be read or written, the pending sign-in used up
    /// all the same.
    async fn finish_oidc_sign_in(
        &self,
        key: &str,
        callback: OidcCallback,
    ) -> Result<SsoReply, ServiceError>;

    /// Swaps the exchange code of a completion page for a session, once:
    /// `flow_cookie` must be the [`OIDC_FLOW_COOKIE`] the code is bound to,
    /// and the code at most a minute old. The account must pass the sign-in
    /// rules of a password sign-in (disabled, remote access from
    /// `request.remote_endpoint`, parental schedule), then the session is
    /// opened with `request`'s device as
    /// [`SessionManager::authenticate_direct`](crate::session::SessionManager::authenticate_direct)
    /// opens one, which applies device access and the session limit. A
    /// refusal of a known account writes an `AuthenticationFailed` activity
    /// entry; nothing touches the account's failed-login counter. The code
    /// is used up by any attempt that reaches it, and every reply clears the
    /// cookie. A Quick Connect confirmation is refused: it opens no session.
    ///
    /// [`ServiceError::NotFound`] while the plugin is disabled, and an error
    /// when the accounts or the sessions cannot be read or written.
    async fn exchange_oidc_sign_in(
        &self,
        code: Secret,
        flow_cookie: Option<Secret>,
        request: AuthenticationRequest,
    ) -> Result<OidcExchangeReply, ServiceError>;

    /// The sign-in buttons `GET /Branding/Configuration` appends to the login
    /// disclaimer, for jellyfin-web's login page to show under its form (D7):
    /// one `<form>` per provider that can sign users in, in the
    /// configuration's order, each a "Sign in with {name}" button that opens
    /// `{public URL}/sso/oidc/{provider}/start` in the same tab.
    ///
    /// `None` while the plugin is disabled, its `ShowLoginButton` is off, no
    /// public URL is usable, or no provider can sign users in: the branding
    /// is then served as stored (B1). Never [`ServiceError::NotFound`] for a
    /// disabled plugin; an error only when its configuration cannot be read.
    async fn login_buttons(&self) -> Result<Option<String>, ServiceError>;

    /// The Quick Connect code-entry page (B13): a form for the code a native
    /// app shows, with a button for each provider that can sign users in,
    /// each starting
    /// [`start_oidc_quick_connect`](Self::start_oidc_quick_connect) with the
    /// code. While Quick Connect is turned off, a `403` page saying so.
    ///
    /// [`ServiceError::NotFound`] while the plugin is disabled, without Quick
    /// Connect approval, and while no provider can sign users in.
    async fn quick_connect_page(&self) -> Result<SsoReply, ServiceError>;

    /// Starts a sign-in at the provider `key` whose verified identity approves
    /// the Quick Connect `code`, as
    /// [`start_oidc_sign_in`](Self::start_oidc_sign_in) starts one. Its
    /// callback, at [`finish_oidc_sign_in`](Self::finish_oidc_sign_in), is
    /// checked and finds or creates the account as a sign-in's does, then
    /// answers a confirmation page instead of the completion page: it names
    /// the code and the account, and holds a one-time confirmation bound to
    /// the [`OIDC_FLOW_COOKIE`], which it keeps as long, for
    /// [`confirm_oidc_quick_connect`](Self::confirm_oidc_quick_connect).
    /// Nothing is approved before that. A `code` that is not six digits is a
    /// `400` page, and Quick Connect turned off a `403` one, both before the
    /// identity provider is contacted.
    ///
    /// [`ServiceError::NotFound`] as for `start_oidc_sign_in`, and without
    /// Quick Connect approval.
    async fn start_oidc_quick_connect(
        &self,
        key: &str,
        client: IpAddr,
        code: &str,
    ) -> Result<SsoReply, ServiceError>;

    /// Approves the Quick Connect code of a confirmation page's
    /// `confirmation`, once: `flow_cookie` must be the [`OIDC_FLOW_COOKIE`] it
    /// is bound to, and it at most a minute old. The account must pass the
    /// sign-in rules of a password sign-in (disabled, remote access from the
    /// approving browser at `client`, parental schedule); then
    /// `QuickConnect::authorize_request` signs the app's device in, which
    /// applies device access and the session limit. No browser session is
    /// opened. Every answer is a page and clears the cookie: the app signed
    /// in, or why not (`400` for a confirmation or cookie that does not check
    /// out, and for a code that is unknown, expired or approved already;
    /// `403` for Quick Connect turned off, and for an account a sign-in rule
    /// refuses, which writes an `AuthenticationFailed` activity entry). The
    /// confirmation is used up by any attempt that reaches it.
    ///
    /// [`ServiceError::NotFound`] while the plugin is disabled or without
    /// Quick Connect approval, and an error when the accounts or the sessions
    /// cannot be read or written.
    async fn confirm_oidc_quick_connect(
        &self,
        confirmation: Option<Secret>,
        flow_cookie: Option<Secret>,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError>;
}

fn _assert_object_safe_sso_manager(_: &dyn SsoManager) {}
