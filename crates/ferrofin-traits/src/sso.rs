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

/// The cookie that binds a pending OIDC sign-in to the browser that started
/// it: [`SsoManager::start_oidc_sign_in`] sets it, and the callback route
/// reads it back into [`OidcCallback::flow_cookie`].
pub const OIDC_FLOW_COOKIE: &str = "ferrofin_sso_flow";

/// What a browser-facing Single Sign-On route answers: a redirect when
/// [`location`](Self::location) is set, else an HTML page, with the cookies
/// to set either way. `Debug` leaves the cookies and the redirect's query
/// (its `state` and nonce) out.
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
}

impl std::fmt::Debug for SsoReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsoReply")
            .field("status", &self.status)
            .field(
                "location",
                &self.location.as_deref().and_then(|l| l.split('?').next()),
            )
            .field("html", &self.html)
            .field("set_cookies", &self.set_cookies.len())
            .finish()
    }
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
    /// provider creates accounts, and the page names it; with no account, a
    /// `403` page says what to ask an administrator. A failed check is the
    /// refusal page. With any page, the pending sign-in is used up and the
    /// [`OIDC_FLOW_COOKIE`] cleared.
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
}

fn _assert_object_safe_sso_manager(_: &dyn SsoManager) {}
