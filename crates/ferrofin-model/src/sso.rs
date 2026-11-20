//! The Single Sign-On plugin's wire types: the OIDC providers its settings
//! page lists and saves through `/sso/admin/oidc/providers`.
//!
//! Ferrofin's own, not Jellyfin contract types and not the community SSO-Auth
//! plugin's: the routes are the plugin's own (D2). Their JSON is PascalCase,
//! as every Ferrofin answer is.
//!
//! [`OidcProvider`] is one provider's settings, the same document in the
//! plugin configuration's `Providers`, in the `PUT` body and in each
//! `GET` entry, so a later setting is one more member here. Its client secret
//! is only ever read from a request: it is never serialized (D16).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::secret::Secret;

/// The scopes a new provider asks for.
pub const DEFAULT_SCOPES: &str = "openid profile";

/// The ID token claim a new provider names users by.
pub const DEFAULT_USERNAME_CLAIM: &str = "preferred_username";

/// One OIDC provider's settings.
///
/// A member a body or a stored provider leaves out takes its default: enabled,
/// [`DEFAULT_SCOPES`], [`DEFAULT_USERNAME_CLAIM`], an `https` issuer only, and
/// empty text otherwise. `Debug` leaves the secret out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct OidcProvider {
    /// The provider's key, the `{provider}` of its sign-in and callback URLs:
    /// 1 to 32 lowercase letters, digits and `-`. It is fixed once the
    /// provider is created, because the callback URL registered at the
    /// identity provider contains it. A `PUT` takes it from its path, and
    /// refuses a body that names another.
    pub key: String,
    /// The name users see on the sign-in button.
    pub name: String,
    /// Whether users can sign in with this provider.
    pub enabled: bool,
    /// The issuer URL, whose discovery document is
    /// `{issuer}/.well-known/openid-configuration`: `https`, unless
    /// [`allow_http_issuer`](Self::allow_http_issuer) is on (D11).
    pub issuer: String,
    /// The client id registered at the identity provider.
    pub client_id: String,
    /// The client secret registered at the identity provider: write-only. A
    /// `PUT` without it, or with it empty or blank, keeps the stored one; no
    /// answer and no stored configuration ever holds it (D16).
    #[serde(skip_serializing)]
    #[schema(value_type = Option<String>, write_only)]
    pub client_secret: Option<Secret>,
    /// The space-separated scopes requested; must include `openid`.
    pub scopes: String,
    /// The ID token claim a user's Ferrofin name is taken from.
    pub username_claim: String,
    /// Accepts an `http` issuer: for an identity provider on a trusted
    /// network only, since its tokens then travel unencrypted (D11).
    pub allow_http_issuer: bool,
}

impl Default for OidcProvider {
    fn default() -> Self {
        Self {
            key: String::new(),
            name: String::new(),
            enabled: true,
            issuer: String::new(),
            client_id: String::new(),
            client_secret: None,
            scopes: DEFAULT_SCOPES.to_owned(),
            username_claim: DEFAULT_USERNAME_CLAIM.to_owned(),
            allow_http_issuer: false,
        }
    }
}

/// A provider as `GET /sso/admin/oidc/providers` lists it: its settings,
/// whether a client secret is stored, and what to register at the identity
/// provider.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcProviderInfo {
    /// The provider's settings, as stored, never with its secret.
    #[serde(flatten)]
    pub provider: OidcProvider,
    /// Whether a client secret is stored for the provider.
    pub has_client_secret: bool,
    /// The redirect URI to register at the identity provider:
    /// `{public URL}/sso/oidc/{provider}/callback`. Absent while neither the
    /// plugin's `PublicUrl` nor the server's published URL is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    /// Why the stored provider is refused, which treats it as disabled
    /// whatever its `Enabled` says. Absent for a valid provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_error: Option<String>,
}

/// What `GET /sso/admin/oidc/providers` answers.
#[derive(Debug, Clone, PartialEq, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcProviderList {
    /// The plugin's public URL setting, as stored (empty when unset).
    pub public_url: String,
    /// Every stored provider, in the configuration's order.
    pub providers: Vec<OidcProviderInfo>,
    /// Why the public URL setting is refused, which leaves every provider
    /// without a callback URL until it is fixed or emptied. Absent while it
    /// is usable or empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_reads_its_defaults_and_never_writes_its_secret() {
        let provider: OidcProvider = serde_json::from_str(
            r#"{"Key":"pocket","Name":"Pocket ID","ClientSecret":"s3cr3t-value"}"#,
        )
        .expect("binds");
        assert_eq!(provider.scopes, DEFAULT_SCOPES);
        assert_eq!(provider.username_claim, DEFAULT_USERNAME_CLAIM);
        assert!(provider.enabled);
        assert!(!provider.allow_http_issuer);
        assert_eq!(
            provider.client_secret.as_ref().map(Secret::expose),
            Some("s3cr3t-value")
        );
        let written = serde_json::to_string(&provider).expect("serializes");
        assert!(!written.contains("s3cr3t-value"), "{written}");
        assert!(!written.contains("ClientSecret"), "{written}");
        let debug = format!("{provider:?}");
        assert!(!debug.contains("s3cr3t-value"), "{debug}");

        let listed = serde_json::to_value(OidcProviderInfo {
            provider,
            has_client_secret: true,
            callback_url: None,
            validation_error: None,
        })
        .expect("serializes");
        assert_eq!(listed["Key"], "pocket");
        assert_eq!(listed["HasClientSecret"], true);
        assert!(listed.get("ClientSecret").is_none(), "{listed}");
        assert!(listed.get("CallbackUrl").is_none(), "{listed}");
    }
}
