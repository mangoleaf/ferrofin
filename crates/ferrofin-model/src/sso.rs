//! The Single Sign-On plugin's wire types: the OIDC providers its settings
//! page lists and saves through `/sso/admin/oidc/providers`, what a
//! completion page posts to `/sso/oidc/exchange`, and the links between
//! accounts and identities that `/sso/links` and `/sso/admin/links` list.
//!
//! Ferrofin's own, not Jellyfin contract types and not the community SSO-Auth
//! plugin's: the routes are the plugin's own (D2). Their JSON is PascalCase,
//! as every Ferrofin answer is.
//!
//! [`OidcProvider`] is one provider's settings, the same document in the
//! plugin configuration's `Providers`, in the `PUT` body and in each
//! `GET` entry, so a later setting is one more member here. Its client secret
//! is only ever read from a request: it is never serialized (D16).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::secret::Secret;

/// The scopes a new provider asks for.
pub const DEFAULT_SCOPES: &str = "openid profile";

/// The ID token claim a new provider names users by.
pub const DEFAULT_USERNAME_CLAIM: &str = "preferred_username";

/// One OIDC provider's settings.
///
/// A member a body or a stored provider leaves out takes its default: enabled,
/// [`DEFAULT_SCOPES`], [`DEFAULT_USERNAME_CLAIM`], an `https` issuer only,
/// accounts created at a first sign-in, and empty text otherwise. `Debug`
/// leaves the secret out.
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
    /// Creates a Ferrofin account, named by the username claim, at an
    /// identity's first sign-in, and links it (D4). Off, an identity no
    /// account is linked to is refused. A name an account already has is
    /// refused either way: no account is ever taken over by name.
    pub create_users: bool,
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
            create_users: true,
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
    /// The discovery test run when the provider was saved: in a `PUT`'s
    /// answer only, never in the list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<OidcDiscovery>,
}

/// What testing a provider's discovery found: the endpoints its discovery
/// document names, or why it could not be used. A failed test is still a
/// saved provider, so this is part of a `200` answer, never its status.
#[derive(Debug, Clone, PartialEq, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcDiscovery {
    /// Why the discovery document or its key set could not be fetched or
    /// used; absent when the test succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Where users are sent to sign in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    /// Where an authorization code is exchanged for tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// Where a user's claims are read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userinfo_endpoint: Option<String>,
    /// The key set ID tokens are checked against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// How many keys of that set Ferrofin can read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_keys: Option<usize>,
    /// What the provider advertises that sign-in cannot use: no `code`
    /// response type, or PKCE methods without `S256`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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

/// What the completion page posts to `POST /sso/oidc/exchange`: the one-time
/// exchange code it carries. The device the session is for comes from the
/// request's `Authorization` header, as for `POST /Users/AuthenticateByName`.
/// `Debug` leaves the code out.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct OidcExchangeRequest {
    /// The exchange code; a missing one is refused as an unknown one.
    #[schema(value_type = String)]
    pub code: Secret,
}

/// What `POST /sso/oidc/{provider}/link` answers: the identity provider's
/// sign-in page, for the link page to send the browser to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcLinkStart {
    /// The authorization URL: its query holds the flow's `state`.
    pub url: String,
}

/// What `GET /sso/links` answers: the caller's own links, the providers they
/// can link at, and whether their account signs in through Single Sign-On
/// only. It never holds a subject.
#[derive(Debug, Clone, PartialEq, Default, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcUserLinks {
    /// Whether the account's authentication provider is Single Sign-On's,
    /// which refuses every password: without a link, it cannot sign in.
    pub single_sign_on_account: bool,
    /// The account's links, oldest first.
    pub links: Vec<OidcLink>,
    /// The enabled providers, in the configuration's order.
    pub providers: Vec<OidcLinkProvider>,
}

/// One of the caller's links.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcLink {
    /// The key of the first provider at the link's issuer; absent when no
    /// provider has that issuer any more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// That provider's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// The issuer the identity is at.
    pub issuer: String,
    /// When the link was made.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date_created: DateTime<Utc>,
}

/// A provider a user can link at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcLinkProvider {
    /// The provider's key.
    pub key: String,
    /// The provider's name.
    pub name: String,
}

/// One link, as `GET /sso/admin/links` lists it and `POST /sso/admin/links`
/// answers it.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct OidcAdminLink {
    /// The linked account.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,
    /// Its name.
    pub user_name: String,
    /// Whether its authentication provider is Single Sign-On's: without a
    /// link, it cannot sign in.
    pub single_sign_on_account: bool,
    /// The key of the first provider at the link's issuer; absent when no
    /// provider has that issuer any more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// That provider's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// The issuer the identity is at.
    pub issuer: String,
    /// The identity at that issuer: the ID token's `sub`.
    pub subject: String,
    /// When the link was made.
    #[schema(value_type = String, format = "date-time")]
    #[serde(with = "crate::json::datetime")]
    pub date_created: DateTime<Utc>,
}

/// What an admin posts to `POST /sso/admin/links`: link the account
/// `UserId` to the identity `Subject` at the provider `Provider`, whose issuer
/// it takes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct OidcLinkRequest {
    /// The account to link.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,
    /// The provider's key.
    pub provider: String,
    /// The identity's `sub` at the provider, as its ID tokens carry it.
    pub subject: String,
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
        assert!(provider.create_users, "D4: on by default");
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
            discovery: None,
        })
        .expect("serializes");
        assert_eq!(listed["Key"], "pocket");
        assert_eq!(listed["HasClientSecret"], true);
        assert!(listed.get("ClientSecret").is_none(), "{listed}");
        assert!(listed.get("CallbackUrl").is_none(), "{listed}");
        assert!(listed.get("Discovery").is_none(), "{listed}");
    }
}
