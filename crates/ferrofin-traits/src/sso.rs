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
use ferrofin_model::sso::{
    OidcAdminLink, OidcDiscovery, OidcProvider, OidcProviderInfo, OidcProviderList, OidcUserLinks,
};
use uuid::Uuid;

use crate::error::ServiceError;
use crate::session::{AuthenticationRequest, AuthenticationResultData};

/// The cookie that binds a pending OIDC sign-in to the browser that started
/// it, on an `http` public URL: [`SsoManager::start_oidc_sign_in`] sets it,
/// the callback route reads it back into [`OidcCallback::flow_cookie`], and
/// the exchange route into [`SsoManager::exchange_oidc_sign_in`]'s
/// `flow_cookie`. On an `https` public URL the cookie is
/// [`OIDC_HOST_FLOW_COOKIE`] instead ([`SsoManager::flow_cookie_name`]).
pub const OIDC_FLOW_COOKIE: &str = "ferrofin_sso_flow";

/// The flow cookie's name on an `https` public URL. Browsers take a
/// `__Host-` cookie only with `Secure`, `Path=/` and no `Domain`, so no other
/// host, a sibling subdomain included, can plant one (D23). Only this name is
/// read there: [`OIDC_FLOW_COOKIE`] is not.
pub const OIDC_HOST_FLOW_COOKIE: &str = "__Host-ferrofin_sso_flow";

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
/// browser's flow cookie. A callback that repeats any of these
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
    /// The value of the browser's flow cookie, read under
    /// [`SsoManager::flow_cookie_name`].
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

    /// The name of the flow cookie, which the routes read the browser's
    /// under: [`OIDC_HOST_FLOW_COOKIE`] while the public URL is `https`, else
    /// [`OIDC_FLOW_COOKIE`]. The plugin sets the cookie under the same name,
    /// and a cookie under the other name is never read.
    async fn flow_cookie_name(&self) -> Result<&'static str, ServiceError>;

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
    /// `S256`, a `state` and a nonce) that sets the flow cookie.
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
    /// flow cookie, which it keeps for the exchange, and its script
    /// swaps the code at [`exchange_oidc_sign_in`](Self::exchange_oidc_sign_in)
    /// and opens jellyfin-web signed in. A sign-in started with
    /// [`start_oidc_quick_connect`](Self::start_oidc_quick_connect) answers
    /// its confirmation page instead. With no account, a `403` page says
    /// what to ask an administrator; a failed check is the refusal page; both
    /// clear the cookie. Either way the pending sign-in is used up. A link
    /// flow's callback ends on its own page instead
    /// ([`start_oidc_link`](Self::start_oidc_link)).
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
    /// `flow_cookie` must be the flow cookie the code is bound to,
    /// and the code at most a minute old. The identity must still be linked
    /// to the account (a link removed since the callback refuses it). The
    /// account must pass the sign-in rules of a password sign-in (disabled,
    /// remote access from `request.remote_endpoint`, parental schedule), then
    /// the session is opened with `request`'s device as
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

    /// The Quick Connect page (B13): a "Sign in with {name}" link for each
    /// provider that can sign users in, each starting
    /// [`start_oidc_quick_connect`](Self::start_oidc_quick_connect). The code
    /// the app shows is typed after the sign-in, on the confirmation page
    /// (D23). While Quick Connect is turned off, a `403` page saying so.
    ///
    /// [`ServiceError::NotFound`] while the plugin is disabled, without Quick
    /// Connect approval, and while no provider can sign users in.
    async fn quick_connect_page(&self) -> Result<SsoReply, ServiceError>;

    /// Starts a sign-in at the provider `key` whose verified identity approves
    /// a Quick Connect code, as
    /// [`start_oidc_sign_in`](Self::start_oidc_sign_in) starts one.
    /// `quick_connect` is the start's `quickConnect` parameter, which holds
    /// nothing: one that holds anything, a code included, is a `400` page,
    /// since a link carrying a code can be sent to someone else (D23). Its
    /// callback, at [`finish_oidc_sign_in`](Self::finish_oidc_sign_in), is
    /// checked and finds or creates the account as a sign-in's does, then
    /// answers a confirmation page instead of the completion page: it names
    /// the account, asks for the code the app shows, and holds a one-time
    /// confirmation bound to the flow cookie, which it keeps as long, for
    /// [`confirm_oidc_quick_connect`](Self::confirm_oidc_quick_connect).
    /// Nothing is approved before that. Quick Connect turned off is a `403`
    /// page. Both refusals come before the identity provider is contacted.
    ///
    /// [`ServiceError::NotFound`] as for `start_oidc_sign_in`, and without
    /// Quick Connect approval.
    async fn start_oidc_quick_connect(
        &self,
        key: &str,
        client: IpAddr,
        quick_connect: &str,
    ) -> Result<SsoReply, ServiceError>;

    /// Approves the Quick Connect `code` typed on a confirmation page, with
    /// its `confirmation`, once: `flow_cookie` must be the flow cookie the
    /// confirmation is bound to, and it at most a minute old. The
    /// confirmation is used up first, so a `code` that is not six digits, or
    /// that no app is waiting with, spends it. The identity must still be
    /// linked to the account (a link removed since the callback refuses it).
    /// The account must pass the sign-in rules of a password sign-in
    /// (disabled, remote access from the approving browser at `client`,
    /// parental schedule); then `QuickConnect::authorize_request` signs the
    /// app's device in, which applies device access and the session limit. No
    /// browser session is opened. Every answer is a page and clears the
    /// cookie: the app signed in, or why not (`400` for a confirmation or
    /// cookie that does not check out, for an identity no longer linked to
    /// the account, for a `code` that is not one, and for one that is
    /// unknown, expired or approved already; `403` for Quick Connect turned
    /// off, and for an account a sign-in rule refuses, which writes an
    /// `AuthenticationFailed` activity entry).
    ///
    /// [`ServiceError::NotFound`] while the plugin is disabled or without
    /// Quick Connect approval, and an error when the accounts or the sessions
    /// cannot be read or written.
    async fn confirm_oidc_quick_connect(
        &self,
        confirmation: Option<Secret>,
        code: Option<&str>,
        flow_cookie: Option<Secret>,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError>;

    // Linking an account and an identity (B12, D22). Linking never changes
    // the account's authentication provider or password (D5). Each link made
    // and removed writes a `UserIdentityLinked` or `UserIdentityUnlinked`
    // activity entry naming the account, the provider and who did it.

    /// The link page, `GET /sso/link`: anonymous, its script reads the
    /// session jellyfin-web keeps in the browser and calls the link routes
    /// with it.
    async fn oidc_link_page(&self) -> Result<SsoReply, ServiceError>;

    /// The links of `user_id`, the enabled providers it can link at (each
    /// with whether the account is linked at its issuer already), and
    /// whether the account signs in through Single Sign-On only.
    async fn oidc_links(&self, user_id: Uuid) -> Result<OidcUserLinks, ServiceError>;

    /// Starts a link of `user_id` at the provider `key`, for the browser at
    /// `client`, from the session whose access token is `session_token`: as
    /// [`start_oidc_sign_in`](Self::start_oidc_sign_in) starts a sign-in, a
    /// redirect that sets the flow cookie, or a page saying why not.
    ///
    /// Refused first, with no flow and no cookie:
    /// [`ServiceError::Forbidden`] for an account whose provider is Single
    /// Sign-On's (it has no password to confirm with: an administrator adds
    /// its links), [`ServiceError::Conflict`] for one linked at the
    /// provider's issuer already (a link is never replaced here: it is
    /// removed first), [`ServiceError::InvalidInput`] for an empty
    /// `password`, and [`ServiceError::Forbidden`] for a `password` that does
    /// not sign the account in. That check goes through the account's own
    /// authentication provider without opening a session, so a wrong one
    /// counts as a failed sign-in does and the login backoff paces it.
    ///
    /// The flow keeps the session token's hash, never the token. Its callback
    /// links the verified identity to `user_id` only while that session is
    /// still one of the account's (a sign-out by any route before the
    /// callback's session check ends the flow),
    /// unless another account has the identity or the account was linked at
    /// that issuer since, and ends on a page saying which; it opens no
    /// session.
    ///
    /// [`ServiceError::NotFound`] as for
    /// [`start_oidc_sign_in`](Self::start_oidc_sign_in), and for an account
    /// that no longer exists.
    async fn start_oidc_link(
        &self,
        key: &str,
        user_id: Uuid,
        password: &Secret,
        session_token: &Secret,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError>;

    /// Every link of every account, with the account's name and the link's
    /// subject.
    async fn all_oidc_links(&self) -> Result<Vec<OidcAdminLink>, ServiceError>;

    /// Links `user_id` to the subject `subject` at the provider `key`'s
    /// issuer, replacing the account's earlier link at that issuer, as the
    /// administrator `by` (nil for an API key).
    ///
    /// [`ServiceError::NotFound`] for an unknown account or provider,
    /// [`ServiceError::InvalidInput`] for an empty subject, one longer than
    /// 255 characters, holding a control character or starting or ending with
    /// a space, and [`ServiceError::Conflict`] when another account has the
    /// identity: nothing is moved.
    async fn add_oidc_link(
        &self,
        by: Uuid,
        user_id: Uuid,
        key: &str,
        subject: &str,
    ) -> Result<OidcAdminLink, ServiceError>;

    /// Removes the link of `user_id` `at` a provider or an issuer, as `by`
    /// (the account itself, an administrator, or nil for an API key), then
    /// signs every session of that account out. An exchange code or a Quick
    /// Connect confirmation issued before the removal is refused at
    /// [`exchange_oidc_sign_in`](Self::exchange_oidc_sign_in) or
    /// [`confirm_oidc_quick_connect`](Self::confirm_oidc_quick_connect),
    /// which look the link up again. An account that signs in through
    /// Single Sign-On only cannot sign in again until it is linked again or
    /// given another authentication provider.
    ///
    /// [`ServiceError::NotFound`] when there is no such provider or link.
    async fn remove_oidc_link(
        &self,
        by: Uuid,
        user_id: Uuid,
        at: OidcLinkAt<'_>,
    ) -> Result<(), ServiceError>;
}

/// Which of an account's links: the one at a provider's issuer, named by the
/// provider's key, or the one at an issuer no provider may have any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcLinkAt<'a> {
    /// The provider's key.
    Provider(&'a str),
    /// The issuer.
    Issuer(&'a str),
}

fn _assert_object_safe_sso_manager(_: &dyn SsoManager) {}
