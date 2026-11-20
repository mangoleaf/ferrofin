//! Single Sign-On manager trait — the DI seam for the `/sso` routes.
//!
//! Ferrofin's own plugin (D1, D15 of the "OIDC sign-in" milestone): no
//! Jellyfin or community plugin surface is ported. The concrete implementation
//! lives with the extension (`ferrofin-extensions::sso`), so every piece of the
//! plugin is grouped there; `ferrofin-api` handlers reach it only through this
//! object-safe trait.

use async_trait::async_trait;
use ferrofin_model::sso::{OidcProvider, OidcProviderInfo, OidcProviderList};

use crate::error::ServiceError;

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
}

fn _assert_object_safe_sso_manager(_: &dyn SsoManager) {}
