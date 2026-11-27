//! The **Single Sign-On** extension: Ferrofin's own sign-in through OpenID
//! Connect identity providers (the "OIDC sign-in" milestone).
//!
//! An internal plugin like LDAP-Auth (D1), under a Ferrofin identity of its
//! own (D15): it is not a port of the community SSO-Auth plugin and does not
//! present itself as one, so it has no row in `docs/PLUGINS_UPSTREAM.md`. It
//! is off by default, and while it is off every `/sso` route answers `404`
//! (B1).
//!
//! What it holds so far:
//! - [`SsoExtension`]: the `/Plugins` entry, the plugin configuration
//!   ([`SsoConfig`]) and the settings page, authored in this repository;
//! - [`SsoService`]: the provider settings behind
//!   `/sso/admin/oidc/providers` (the [`SsoManager`] trait), which keeps each
//!   provider's client secret in the plugin's private data, a file no route
//!   serves, never in its configuration (D16), and tests a saved provider's
//!   discovery;
//! - [`oidc_http`]: the HTTP client every call to an identity provider goes
//!   through, with the D19 limits;
//! - [`discovery`]: a provider's discovery document and key set, and their
//!   cache;
//! - [`sign_in`]: a sign-in at a provider, from `/sso/oidc/{provider}/start`
//!   to the [`VerifiedIdentity`](sign_in::VerifiedIdentity) its callback ends
//!   with, through the pending flows of [`flows`];
//! - [`accounts`]: the Ferrofin account that identity signs in to, found by
//!   its link or created, and [`SsoAuthProvider`](accounts::SsoAuthProvider),
//!   the authentication provider of the accounts it creates, which refuses
//!   every password;
//! - [`roles`]: a user's roles in the provider's role claim, the required
//!   role a sign-in is refused without, and role sync, which keeps the
//!   administrator flag, the libraries and Live TV in step with them;
//! - [`avatar_sync`]: a user's profile picture, kept in step with the
//!   provider's `picture` claim after each sign-in;
//! - [`complete`]: the completion page the callback answers, and the
//!   exchange of its one-time code for the session, under the account rules
//!   of a password sign-in;
//! - [`links`]: the links between accounts and identities, which a user
//!   makes on the link page through a link flow, and an admin lists, adds and
//!   removes;
//! - [`login_buttons`]: the "Sign in with" buttons jellyfin-web's login page
//!   shows under its form;
//! - [`quick_connect`]: a native app's Quick Connect code approved through a
//!   sign-in and a confirmation page, under the account rules of a sign-in;
//! - [`pacing`]: the pace of the routes that create a pending flow or take a
//!   one-time value, per address or IPv6 /64.
//!
//! An identity provider is contacted only when an admin saves a provider or a
//! user signs in with it (B1).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{
    OidcAdminLink, OidcDiscovery, OidcProvider, OidcProviderInfo, OidcProviderList, OidcUserLinks,
};
use ferrofin_traits::activity::ActivityManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::ExternalIdentityStore;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::plugins::{PluginConfigPage, PluginDescriptor, PluginManager};
use ferrofin_traits::security::{AuthenticationManager, QuickConnect};
use ferrofin_traits::session::{AuthenticationRequest, SessionManager};
use ferrofin_traits::sso::{
    OidcCallback, OidcExchangeReply, OidcLinkAt, OidcPacedRoute, SsoManager, SsoReply,
};
use ferrofin_traits::tasks::ScheduledTask;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Extension, ExtensionContext};

pub mod accounts;
pub mod avatar_sync;
pub mod complete;
pub mod discovery;
#[cfg(any(test, feature = "test-util"))]
pub mod fake_idp;
pub mod flows;
pub mod links;
pub mod login_buttons;
pub mod oidc_http;
pub mod pacing;
pub mod quick_connect;
pub mod roles;
pub mod sign_in;

use discovery::MetadataCache;
use flows::{ExchangeCodes, PendingFlows};
use oidc_http::OidcHttpClient;
use pacing::{PaceLimits, Pacing};

/// The Single Sign-On plugin's id in `/Plugins`: Ferrofin's own, generated
/// once and fixed for good, so its enabled flag, configuration and private
/// data survive every upgrade (D15). Never the community SSO-Auth plugin's
/// `505ce9d1-d916-42fa-86ca-673ef241d7df`.
pub const EXTENSION_ID: Uuid = Uuid::from_u128(0x67a2_9a1d_e02b_4aaa_bba0_4b22_75e7_65d0);

/// The settings page's name, which jellyfin-web loads it by
/// (`configurationpage?name=SingleSignOn`).
pub const SETTINGS_PAGE_NAME: &str = "SingleSignOn";

/// The longest provider key: the milestone's phase 2 fixes it at 32.
pub const MAX_PROVIDER_KEY_LEN: usize = 32;

/// The plugin configuration, stored as `{plugins}/67a29a1d-…/config.json` and
/// returned by `GET /Plugins/{id}/Configuration`. It holds no secret.
///
/// Each provider is kept as the document stored, and read with
/// [`checked_providers`](Self::checked_providers): the generic configuration
/// route and the file on disk can hold one that does not read, or that the
/// provider routes would refuse, and a save of one provider keeps the others
/// as they are, members a newer server added included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SsoConfig {
    /// The server's public URL the callback URLs are built on, base path
    /// included; empty uses the server's published URL.
    #[serde(deserialize_with = "crate::ldap_auth::null_as_empty")]
    pub public_url: String,
    /// The OIDC providers, each an [`OidcProvider`] document.
    #[serde(deserialize_with = "crate::ldap_auth::null_as_empty")]
    pub providers: Vec<serde_json::Value>,
    /// Shows a sign-in button per provider on jellyfin-web's login page
    /// ([`SsoManager::login_buttons`]); on by default (D7), a stored `null`
    /// included.
    #[serde(deserialize_with = "null_as_on")]
    pub show_login_button: bool,
}

impl Default for SsoConfig {
    fn default() -> Self {
        Self {
            public_url: String::new(),
            providers: Vec::new(),
            show_login_button: true,
        }
    }
}

impl SsoConfig {
    /// Every stored provider with the reason it is refused, if it is: it does
    /// not read, it fails [`validate_provider`], or an earlier provider has
    /// its key. A refused provider is treated as disabled. A secret in the
    /// configuration is never read (D16).
    #[must_use]
    pub fn checked_providers(&self) -> Vec<(OidcProvider, Option<String>)> {
        let mut keys = HashSet::new();
        self.providers
            .iter()
            .map(|stored| match OidcProvider::deserialize(stored) {
                Ok(mut provider) => {
                    provider.client_secret = None;
                    let repeated = !keys.insert(provider.key.clone());
                    let refused = validate_provider(&provider).err().or_else(|| {
                        repeated.then(|| "an earlier provider has the same key".to_owned())
                    });
                    (provider, refused)
                }
                Err(_) => (
                    OidcProvider {
                        key: stored_key(stored).unwrap_or_default().to_owned(),
                        enabled: false,
                        ..OidcProvider::default()
                    },
                    // Not serde's message, which could quote a stored value.
                    Some(
                        "the stored provider does not read: it is not an object, or a member \
                         has the wrong type"
                            .to_owned(),
                    ),
                ),
            })
            .collect()
    }
}

/// Reads a switch that is on by default: a JSON `null` is on, as an absent
/// member is.
fn null_as_on<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(true))
}

/// The provider members a stored document must not keep: a client secret
/// lives in the private data only (D16), whatever spelling the generic
/// configuration route stored it under.
fn scrub_secrets(config: &mut SsoConfig) {
    for stored in &mut config.providers {
        if let Some(members) = stored.as_object_mut() {
            members.retain(|name, _| !name.eq_ignore_ascii_case("ClientSecret"));
        }
    }
}

/// The `Key` of a stored provider document, when it has one.
fn stored_key(stored: &serde_json::Value) -> Option<&str> {
    stored.get("Key").and_then(serde_json::Value::as_str)
}

/// The plugin's private data, kept through
/// [`PluginManager::set_plugin_private_data`] and never in its configuration:
/// what no route may return (D16). `Debug` leaves every secret out.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct SsoSecrets {
    /// Each provider's client secret, by provider key.
    oidc_client_secrets: BTreeMap<String, Secret>,
}

/// Checks a provider key: 1 to [`MAX_PROVIDER_KEY_LEN`] lowercase ASCII
/// letters, digits and `-`, so it can sit in a URL path as it is.
///
/// # Errors
///
/// The message to show the admin.
pub fn validate_key(key: &str) -> Result<(), String> {
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    if (1..=MAX_PROVIDER_KEY_LEN).contains(&key.len()) && key.bytes().all(allowed) {
        Ok(())
    } else {
        Err(format!(
            "the provider key must be 1 to {MAX_PROVIDER_KEY_LEN} lowercase letters, digits or \
             hyphens"
        ))
    }
}

/// Checks a provider's settings, its key included. No setting skips a check:
/// `AllowHttpIssuer` only widens the issuer's scheme to `http` (D11).
///
/// # Errors
///
/// The message to show the admin, naming the first setting refused.
pub fn validate_provider(provider: &OidcProvider) -> Result<(), String> {
    validate_key(&provider.key)?;
    // Each is used as written (the issuer is compared with the token's `iss`),
    // so a save trims them and a stored one must already be trimmed.
    for (setting, value) in [
        ("provider name", &provider.name),
        ("issuer", &provider.issuer),
        ("client id", &provider.client_id),
        ("scopes", &provider.scopes),
        ("username claim", &provider.username_claim),
        ("role claim", &provider.role_claim),
        ("required role", &provider.required_role),
        ("administrator role", &provider.admin_role),
    ] {
        if value.chars().any(char::is_control) || value.trim() != value.as_str() {
            return Err(format!(
                "the {setting} must not hold control characters or start or end with a space"
            ));
        }
    }
    if provider.name.trim().is_empty() {
        return Err("the provider name is required".to_owned());
    }
    validate_issuer(&provider.issuer, provider.allow_http_issuer)?;
    if provider.client_id.trim().is_empty() {
        return Err("the client id is required".to_owned());
    }
    if !provider
        .scopes
        .split_ascii_whitespace()
        .any(|s| s == "openid")
    {
        return Err("the scopes must include openid".to_owned());
    }
    if provider.username_claim.trim().is_empty() {
        return Err("the username claim is required".to_owned());
    }
    if roles::reads_roles(provider) && provider.role_claim.is_empty() {
        return Err("the role claim is required for a required role or role sync".to_owned());
    }
    roles::validate_mappings(provider)
}

/// Checks an issuer as OpenID Connect Core 1.0 defines one: a URL with a
/// scheme, a host and optionally a port and a path, and no query or
/// fragment. Its scheme is `https`, or `http` with `allow_http`.
fn validate_issuer(issuer: &str, allow_http: bool) -> Result<(), String> {
    let url = url::Url::parse(issuer).map_err(|e| format!("the issuer is not a URL ({e})"))?;
    match url.scheme() {
        "https" => {}
        "http" if allow_http => {}
        "http" => {
            return Err(
                "the issuer must use https; allow an http issuer only for an identity provider \
                 on a trusted network"
                    .to_owned(),
            );
        }
        _ => return Err("the issuer must be an https URL".to_owned()),
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err("the issuer must name a host, and no user name or password".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("the issuer must have no query or fragment".to_owned());
    }
    // The parser forgives spellings a token's exact `iss` would never match
    // (`https:host`, a backslash, dot segments, upper case, a default port):
    // the issuer must be the URL it parses to, but for an empty path's `/`.
    let parsed = url.as_str();
    if issuer != parsed && Some(issuer) != parsed.strip_suffix('/') {
        // The `/` the parser gives an empty path is not the issuer's; any
        // other trailing `/` is.
        let suggestion = if url.path() == "/" && !issuer.ends_with('/') {
            parsed.trim_end_matches('/')
        } else {
            parsed
        };
        return Err(format!(
            "the issuer must be written as the identity provider names it, such as {suggestion}"
        ));
    }
    Ok(())
}

/// `text` as a public URL to build callback URLs on (an `http` or `https` URL
/// with a host and no user name, query or fragment), without its trailing
/// `/`; `None` for anything else.
fn public_base(text: &str) -> Option<String> {
    let url = url::Url::parse(text.trim()).ok()?;
    let usable = matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    usable.then(|| url.as_str().trim_end_matches('/').to_owned())
}

/// Whether the Single Sign-On plugin is enabled.
async fn plugin_enabled(plugins: &dyn PluginManager) -> Result<bool, ServiceError> {
    Ok(plugins
        .get_plugin(EXTENSION_ID)
        .await?
        .is_some_and(|plugin| plugin.enabled))
}

/// The Single Sign-On plugin's service: reads and saves its configuration and
/// private data through the plugin manager, finds or creates the account a
/// sign-in ends at, and opens its session. Shared by the `/sso` routes
/// through [`SsoManager`].
pub struct SsoService {
    plugins: Arc<dyn PluginManager>,
    /// The accounts a sign-in finds or creates, and their sign-in rules.
    users: Arc<dyn UserManager>,
    /// The links from an identity to its account.
    identities: Arc<dyn ExternalIdentityStore>,
    /// Opens a signed-in account's session and mints its token.
    sessions: Arc<dyn SessionManager>,
    /// The dashboard's activity log, for a refused sign-in of a known
    /// account, and each link made or removed.
    activity: Arc<dyn ActivityManager>,
    /// Held while an account is created and linked, so two first sign-ins of
    /// one identity end at one account, while a link is made, and while a
    /// removal unlinks and signs the account out ([`links`]), so no link
    /// falls between the two on this server, and while an exchange or a Quick Connect approval checks its link and
    /// mints its session, so a removal cannot fall between the two. Shared
    /// with the task a link change or a removal runs in ([`links`]).
    provisioning: Arc<tokio::sync::Mutex<()>>,
    /// The server's published URL (`--published-url`,
    /// `FERROFIN_PUBLISHED_URL`), the public URL when the plugin sets none.
    published_url: Option<String>,
    /// Serialises the read-modify-write of the configuration and the private
    /// data, so two saves at once cannot lose each other's providers.
    saving: tokio::sync::Mutex<()>,
    /// The client for every call to an identity provider.
    http: OidcHttpClient,
    /// Downloads and stores profile pictures ([`avatar_sync`]), once
    /// [`with_remote_images`](Self::with_remote_images) gives it the server's
    /// image downloads.
    avatars: Option<avatar_sync::AvatarSync>,
    /// Each provider's discovery document and key set, for sign-in.
    metadata: MetadataCache,
    /// The sign-ins started and not yet called back.
    flows: Arc<PendingFlows>,
    /// The sign-ins called back whose completion page has not yet swapped
    /// its exchange code.
    exchanges: ExchangeCodes,
    /// The pace of the routes that create a pending flow or take a one-time
    /// value ([`pacing`]).
    pacing: Pacing,
    /// Approves a native app's Quick Connect code ([`quick_connect`]), once
    /// [`with_quick_connect`](Self::with_quick_connect) gives it.
    quick_connect: Option<Arc<dyn QuickConnect>>,
}

impl SsoService {
    /// A service over the plugin manager (the enabled flag, the configuration
    /// and the private data), the accounts and their identity links, the
    /// sessions a sign-in opens, the activity log, and the server's published
    /// URL, which the callback URLs fall back to. Never the request's `Host` header.
    #[must_use]
    pub fn new(
        plugins: Arc<dyn PluginManager>,
        users: Arc<dyn UserManager>,
        identities: Arc<dyn ExternalIdentityStore>,
        sessions: Arc<dyn SessionManager>,
        activity: Arc<dyn ActivityManager>,
        published_url: Option<String>,
    ) -> Self {
        Self {
            plugins,
            users,
            identities,
            sessions,
            activity,
            provisioning: Arc::default(),
            published_url,
            saving: tokio::sync::Mutex::new(()),
            http: OidcHttpClient::new(),
            avatars: None,
            metadata: MetadataCache::default(),
            flows: Arc::default(),
            exchanges: ExchangeCodes::default(),
            pacing: Pacing::new(PaceLimits::DEFAULT),
            quick_connect: None,
        }
    }

    /// The service paced under `limits` instead of [`PaceLimits::DEFAULT`]:
    /// for a test that makes more requests from one address than a person
    /// would. No address is exempt from the pace.
    #[must_use]
    pub fn with_pace_limits(mut self, limits: PaceLimits) -> Self {
        self.pacing = Pacing::new(limits);
        self
    }

    /// The plugin's current configuration.
    ///
    /// # Errors
    ///
    /// When the plugin manager cannot read it, or what it holds is not a
    /// configuration.
    pub async fn config(&self) -> Result<SsoConfig, ServiceError> {
        let bytes = self.plugins.get_plugin_configuration(EXTENSION_ID).await?;
        serde_json::from_slice(&bytes).map_err(|e| {
            ServiceError::backend(format!("read the Single Sign-On configuration: {e}"))
        })
    }

    /// Saves `config` as the plugin's configuration, without any client
    /// secret a provider document was given.
    async fn save_config(&self, config: &SsoConfig) -> Result<(), ServiceError> {
        let mut config = config.clone();
        scrub_secrets(&mut config);
        let bytes = serde_json::to_vec(&config).map_err(|e| {
            ServiceError::backend(format!("serialize the Single Sign-On configuration: {e}"))
        })?;
        self.plugins
            .set_plugin_configuration(EXTENSION_ID, bytes)
            .await
    }

    /// The plugin's private data; empty until a secret is saved.
    async fn secrets(&self) -> Result<SsoSecrets, ServiceError> {
        let Some(bytes) = self.plugins.get_plugin_private_data(EXTENSION_ID).await? else {
            return Ok(SsoSecrets::default());
        };
        // The reason names only where the file stops reading: serde's own
        // message could quote a value.
        serde_json::from_slice(&bytes).map_err(|e| {
            ServiceError::backend(format!(
                "the Single Sign-On private data (private.json beside its configuration) does \
                 not read (line {}, column {})",
                e.line(),
                e.column()
            ))
        })
    }

    /// Saves `secrets` as the plugin's private data.
    async fn save_secrets(&self, secrets: &SsoSecrets) -> Result<(), ServiceError> {
        let bytes = serde_json::to_vec(secrets)
            .map_err(|_| ServiceError::backend("serialize the Single Sign-On private data"))?;
        self.plugins
            .set_plugin_private_data(EXTENSION_ID, bytes)
            .await
    }

    /// The public URL the callback URLs are built on: the plugin's
    /// `PublicUrl`, else, while it is empty, the server's published URL when
    /// that is usable. `Err` says why a `PublicUrl` that is set is refused:
    /// nothing falls back past it.
    fn public_url(&self, config: &SsoConfig) -> Result<Option<String>, String> {
        if config.public_url.trim().is_empty() {
            return Ok(self.published_url.as_deref().and_then(public_base));
        }
        public_base(&config.public_url).map(Some).ok_or_else(|| {
            "the public URL must be an http or https URL with a host and no user name, query or \
             fragment"
                .to_owned()
        })
    }

    /// `provider` as the list shows it.
    fn info(
        provider: OidcProvider,
        validation_error: Option<String>,
        secrets: &SsoSecrets,
        public_url: Option<&str>,
    ) -> OidcProviderInfo {
        OidcProviderInfo {
            has_client_secret: secrets.oidc_client_secrets.contains_key(&provider.key),
            // A key that fails validation may not fit in a URL path.
            callback_url: public_url
                .filter(|_| validate_key(&provider.key).is_ok())
                .map(|base| format!("{base}/sso/oidc/{}/callback", provider.key)),
            provider,
            validation_error,
            discovery: None,
        }
    }
}

#[async_trait]
impl SsoManager for SsoService {
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        if plugin_enabled(self.plugins.as_ref()).await? {
            Ok(())
        } else {
            Err(ServiceError::not_found(
                "the Single Sign-On plugin is disabled",
            ))
        }
    }

    async fn flow_cookie_name(&self) -> Result<&'static str, ServiceError> {
        self.ensure_enabled().await?;
        let config = self.config().await?;
        Ok(sign_in::flow_cookie_name(
            self.public_url(&config).ok().flatten().as_deref(),
        ))
    }

    fn pace_oidc(&self, route: OidcPacedRoute, client: std::net::IpAddr) -> Option<SsoReply> {
        (!self.pacing.admit(route, client, std::time::Instant::now()))
            .then(|| pacing::paced_page(route))
    }

    async fn oidc_providers(&self) -> Result<OidcProviderList, ServiceError> {
        self.ensure_enabled().await?;
        let config = self.config().await?;
        let secrets = self.secrets().await?;
        let (public_url, public_url_error) = match self.public_url(&config) {
            Ok(url) => (url, None),
            Err(why) => (None, Some(why)),
        };
        Ok(OidcProviderList {
            providers: config
                .checked_providers()
                .into_iter()
                .map(|(provider, refused)| {
                    Self::info(provider, refused, &secrets, public_url.as_deref())
                })
                .collect(),
            public_url: config.public_url,
            public_url_error,
        })
    }

    async fn save_oidc_provider(
        &self,
        key: &str,
        mut provider: OidcProvider,
    ) -> Result<OidcProviderInfo, ServiceError> {
        self.ensure_enabled().await?;
        validate_key(key).map_err(ServiceError::invalid_input)?;
        if !provider.key.is_empty() && provider.key != key {
            return Err(ServiceError::invalid_input(
                "a provider's key cannot be changed",
            ));
        }
        provider.key = key.to_owned();
        for value in [
            &mut provider.name,
            &mut provider.issuer,
            &mut provider.client_id,
            &mut provider.scopes,
            &mut provider.username_claim,
            &mut provider.role_claim,
            &mut provider.required_role,
            &mut provider.admin_role,
        ] {
            *value = value.trim().to_owned();
        }
        roles::trim_mappings(&mut provider);
        validate_provider(&provider).map_err(ServiceError::invalid_input)?;
        // An empty or blank secret field on the page means "keep".
        let secret = provider
            .client_secret
            .take()
            .filter(|secret| !secret.expose().trim().is_empty());
        if secret.as_ref().is_some_and(|secret| {
            let secret = secret.expose();
            secret.chars().any(char::is_control) || secret.trim() != secret
        }) {
            return Err(ServiceError::invalid_input(
                "the client secret must not hold control characters or start or end with a space",
            ));
        }

        let _saving = self.saving.lock().await;
        let mut config = self.config().await?;
        let mut secrets = self.secrets().await?;
        // A secret counts only beside its provider: one left behind by a
        // provider the configuration route removed is not inherited.
        let stored = config
            .providers
            .iter()
            .any(|stored| stored_key(stored) == Some(key));
        if secret.is_none() && !(stored && secrets.oidc_client_secrets.contains_key(key)) {
            return Err(ServiceError::invalid_input(
                "the provider needs a client secret",
            ));
        }
        let document = serde_json::to_value(&provider)
            .map_err(|e| ServiceError::backend(format!("serialize the OIDC provider: {e}")))?;
        // In the place of the first provider with this key, the only one left.
        let at = config
            .providers
            .iter()
            .position(|stored| stored_key(stored) == Some(key));
        config
            .providers
            .retain(|stored| stored_key(stored) != Some(key));
        config
            .providers
            .insert(at.unwrap_or(config.providers.len()), document);
        // The provider first, then its secret: a failure between the two
        // leaves a provider asking for its secret, never a secret without a
        // provider. Secrets whose provider is gone are dropped on the way.
        self.save_config(&config).await?;
        self.metadata.invalidate(key);
        let before = secrets.clone();
        if let Some(secret) = secret {
            secrets.oidc_client_secrets.insert(key.to_owned(), secret);
        }
        let keys: HashSet<&str> = config.providers.iter().filter_map(stored_key).collect();
        secrets
            .oidc_client_secrets
            .retain(|stored, _| keys.contains(stored.as_str()));
        if secrets != before {
            self.save_secrets(&secrets).await?;
        }
        tracing::info!(oidc_provider = %key, "OIDC provider saved");
        let public_url = self.public_url(&config).ok().flatten();
        Ok(Self::info(provider, None, &secrets, public_url.as_deref()))
    }

    async fn delete_oidc_provider(&self, key: &str) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        let _saving = self.saving.lock().await;
        let mut config = self.config().await?;
        // Read before anything is written, so private data that does not
        // read changes nothing.
        let mut secrets = self.secrets().await?;
        let before = config.providers.len();
        config
            .providers
            .retain(|stored| stored_key(stored) != Some(key));
        let removed = config.providers.len() != before;
        if removed {
            self.save_config(&config).await?;
        }
        self.metadata.invalidate(key);
        // A secret left without its provider goes too.
        let removed_secret = secrets.oidc_client_secrets.remove(key).is_some();
        if removed_secret {
            self.save_secrets(&secrets).await?;
        }
        if !removed && !removed_secret {
            return Err(ServiceError::not_found(format!("OIDC provider {key}")));
        }
        tracing::info!(oidc_provider = %key, "OIDC provider removed");
        Ok(())
    }

    async fn test_oidc_provider(
        &self,
        provider: &OidcProvider,
    ) -> Result<OidcDiscovery, ServiceError> {
        self.ensure_enabled().await?;
        let failed = |why: String| OidcDiscovery {
            error: Some(why),
            ..OidcDiscovery::default()
        };
        // A provider the routes would refuse is not fetched.
        if let Err(why) = validate_provider(provider) {
            return Ok(failed(why));
        }
        match self.metadata.refresh(&self.http, provider).await {
            Ok(metadata) => {
                tracing::info!(oidc_provider = %provider.key, "OIDC provider discovery succeeded");
                Ok(discovery::summary(&metadata))
            }
            Err(failure) => {
                // The message may name the issuer's URLs: it goes to the
                // admin's page only.
                tracing::warn!(
                    oidc_provider = %provider.key,
                    reason = failure.kind,
                    "OIDC provider discovery failed"
                );
                Ok(failed(failure.message))
            }
        }
    }

    async fn start_oidc_sign_in(
        &self,
        key: &str,
        client: std::net::IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.start_sign_in(key, client, flows::FlowPurpose::SignIn)
            .await
    }

    async fn finish_oidc_sign_in(
        &self,
        key: &str,
        callback: OidcCallback,
    ) -> Result<SsoReply, ServiceError> {
        self.finish_sign_in(key, callback).await
    }

    async fn exchange_oidc_sign_in(
        &self,
        code: Secret,
        flow_cookie: Option<Secret>,
        request: AuthenticationRequest,
    ) -> Result<OidcExchangeReply, ServiceError> {
        self.exchange_sign_in(&code, flow_cookie.as_ref(), request)
            .await
    }

    async fn login_buttons(&self) -> Result<Option<String>, ServiceError> {
        self.login_button_forms().await
    }

    async fn quick_connect_page(&self) -> Result<SsoReply, ServiceError> {
        self.sign_in_links_page().await
    }

    async fn start_oidc_quick_connect(
        &self,
        key: &str,
        client: std::net::IpAddr,
        quick_connect: &str,
    ) -> Result<SsoReply, ServiceError> {
        self.start_quick_connect(key, client, quick_connect).await
    }

    async fn confirm_oidc_quick_connect(
        &self,
        confirmation: Option<Secret>,
        code: Option<&str>,
        flow_cookie: Option<Secret>,
        client: std::net::IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.confirm_quick_connect(confirmation.as_ref(), code, flow_cookie.as_ref(), client)
            .await
    }

    async fn oidc_link_page(&self) -> Result<SsoReply, ServiceError> {
        self.link_page().await
    }

    async fn oidc_links(&self, user_id: Uuid) -> Result<OidcUserLinks, ServiceError> {
        self.user_links(user_id).await
    }

    async fn start_oidc_link(
        &self,
        key: &str,
        user_id: Uuid,
        password: &Secret,
        session_token: &Secret,
        client: std::net::IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.start_link(key, user_id, password, session_token, client)
            .await
    }

    async fn all_oidc_links(&self) -> Result<Vec<OidcAdminLink>, ServiceError> {
        self.all_links().await
    }

    async fn add_oidc_link(
        &self,
        by: Uuid,
        user_id: Uuid,
        key: &str,
        subject: &str,
    ) -> Result<OidcAdminLink, ServiceError> {
        self.add_link(by, user_id, key, subject).await
    }

    async fn remove_oidc_link(
        &self,
        by: Uuid,
        user_id: Uuid,
        at: OidcLinkAt<'_>,
    ) -> Result<(), ServiceError> {
        self.remove_link(by, user_id, at).await
    }
}

/// The `/Plugins` surface of the Single Sign-On extension.
#[derive(Debug, Default, Clone, Copy)]
pub struct SsoExtension;

impl Extension for SsoExtension {
    fn id(&self) -> Uuid {
        EXTENSION_ID
    }

    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: EXTENSION_ID,
            name: "Single Sign-On".to_owned(),
            // Ferrofin's own, so it carries the server's release, as the
            // Ferrofin Settings plugin does.
            version: crate::ferrofin_settings::plugin_version(ferrofin_health::build_version()),
            description: "Sign users in through OpenID Connect identity providers.".to_owned(),
            // Off until an administrator enables it (D1).
            enabled: false,
            has_image: false,
            can_uninstall: false,
            configuration_file_name: None,
        }
    }

    fn default_config(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&SsoConfig::default()).unwrap_or_else(|_| b"{}".to_vec())
    }

    fn config_pages(&self) -> Vec<PluginConfigPage> {
        // Authored in this repository, not vendored, so `build.rs` neither
        // pins nor copies it. It is served anonymously and holds no setting:
        // its script reads the admin-only `/sso/admin/oidc/providers`.
        vec![PluginConfigPage {
            name: SETTINGS_PAGE_NAME.to_owned(),
            bytes: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/sso/settings.html"
            ))
            .to_vec(),
            enable_in_main_menu: false,
            content_type: None,
        }]
    }

    /// The provider of the accounts sign-in creates, which refuses every
    /// password and is listed while the plugin is enabled.
    fn auth_providers(&self, cx: &ExtensionContext) -> Vec<Arc<dyn AuthenticationManager>> {
        vec![Arc::new(accounts::SsoAuthProvider::new(Arc::clone(
            &cx.plugins,
        )))]
    }

    fn tasks(&self, _cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
