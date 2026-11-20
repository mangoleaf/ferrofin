//! OpenID Connect discovery: a provider's discovery document and the key set
//! it names, fetched through [`OidcHttpClient`] and checked by
//! `openidconnect` (the document must name the configured issuer exactly),
//! summarised for the settings page ([`summary`]) and kept for sign-in in a
//! [`MetadataCache`]. A save's test fills the cache ([`MetadataCache::refresh`]);
//! its readers ([`MetadataCache::get`], [`MetadataCache::get_or_refresh`]) are
//! for the sign-in phases.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ferrofin_model::sso::{OidcDiscovery, OidcProvider};
use openidconnect::core::{
    CoreAuthDisplay, CoreClaimName, CoreClaimType, CoreClientAuthMethod, CoreGrantType,
    CoreJsonWebKey, CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm,
    CoreResponseMode, CoreResponseType, CoreSubjectIdentifierType,
};
use openidconnect::{AdditionalProviderMetadata, DiscoveryError, IssuerUrl};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::oidc_http::{OidcHttpClient, OidcHttpError};

/// How long a provider's discovery document and key set are used before they
/// are fetched again: 1 hour. Candidate setting (D19): a constant until the
/// owner asks for it.
pub const METADATA_CACHE_LIFETIME: Duration = Duration::from_hours(1);

/// The members of a discovery document that `openidconnect`'s core set
/// leaves out and Ferrofin reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryExtras {
    /// The PKCE methods the provider lists (RFC 8414), when it lists any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

impl AdditionalProviderMetadata for DiscoveryExtras {}

/// A provider's discovery document, with the key set its `jwks_uri` names.
pub type ProviderMetadata = openidconnect::ProviderMetadata<
    DiscoveryExtras,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

/// Why a provider's discovery failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryFailure {
    /// The reason, for the admin. It may name the issuer and the URLs the
    /// document names, so it is shown, never logged.
    pub message: String,
    /// What failed, in a word, for the log line.
    pub kind: &'static str,
}

impl DiscoveryFailure {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }
}

/// Fetches `provider`'s discovery document,
/// `{issuer}/.well-known/openid-configuration`, and the key set it names,
/// through `http` (`http` URLs only with the provider's `AllowHttpIssuer`).
/// A redirect, an answer that is not JSON, a document naming another issuer
/// and a key set that does not read each fail it.
///
/// # Errors
///
/// A [`DiscoveryFailure`] saying why.
pub async fn discover(
    http: &OidcHttpClient,
    provider: &OidcProvider,
) -> Result<ProviderMetadata, DiscoveryFailure> {
    let issuer = IssuerUrl::new(provider.issuer.clone())
        .map_err(|e| DiscoveryFailure::new("issuer", format!("the issuer is not a URL ({e})")))?;
    let http = http.allowing_http(provider.allow_http_issuer);
    ProviderMetadata::discover_async(issuer, &http)
        .await
        .map_err(failure)
}

/// `e` as the admin is told it, on the settings page only: it may name the
/// discovery URL, the issuer the document names, and, for a document that does
/// not read, the member and `serde`'s reason, which can quote one value. The
/// body itself is never quoted.
fn failure(e: DiscoveryError<OidcHttpError>) -> DiscoveryFailure {
    match e {
        DiscoveryError::Request(e) => DiscoveryFailure::new("request", e.to_string()),
        DiscoveryError::Response(_, _, why) => DiscoveryFailure::new(
            "response",
            format!("the identity provider's answer is refused: {why}"),
        ),
        DiscoveryError::Parse(e) => DiscoveryFailure::new(
            "parse",
            format!(
                "the discovery document or its key set does not read at `{}`: {}",
                e.path(),
                e.inner()
            ),
        ),
        DiscoveryError::Validation(why) => DiscoveryFailure::new(
            "issuer",
            format!("the discovery document is for another issuer: {why}"),
        ),
        DiscoveryError::UrlParse(e) => {
            DiscoveryFailure::new("issuer", format!("the discovery URL is not a URL ({e})"))
        }
        DiscoveryError::Other(why) => DiscoveryFailure::new("other", why),
        _ => DiscoveryFailure::new("other", "discovery failed"),
    }
}

/// What the settings page shows of a discovery that succeeded: the endpoints
/// found, how many keys were read, and what sign-in needs that the provider
/// does not offer: the `code` response type, `S256` among the PKCE methods it
/// lists, a token endpoint, and a key Ferrofin can read.
#[must_use]
pub fn summary(metadata: &ProviderMetadata) -> OidcDiscovery {
    let mut warnings = Vec::new();
    if !metadata
        .response_types_supported()
        .iter()
        .any(|types| types.as_slice() == [CoreResponseType::Code])
    {
        warnings.push(
            "the provider does not advertise the code response type, which sign-in uses".to_owned(),
        );
    }
    if let Some(methods) = &metadata
        .additional_metadata()
        .code_challenge_methods_supported
        && !methods.is_empty()
        && !methods.iter().any(|method| method == "S256")
    {
        warnings
            .push("the provider lists PKCE methods without S256, which sign-in uses".to_owned());
    }
    if metadata.token_endpoint().is_none() {
        warnings.push(
            "the provider advertises no token endpoint, which sign-in exchanges its code at"
                .to_owned(),
        );
    }
    if metadata.jwks().keys().is_empty() {
        warnings.push(
            "the provider's key set holds no key Ferrofin can read, so no ID token can be checked"
                .to_owned(),
        );
    }
    OidcDiscovery {
        error: None,
        authorization_endpoint: Some(metadata.authorization_endpoint().as_str().to_owned()),
        token_endpoint: metadata.token_endpoint().map(|url| url.as_str().to_owned()),
        userinfo_endpoint: metadata
            .userinfo_endpoint()
            .map(|url| url.as_str().to_owned()),
        jwks_uri: Some(metadata.jwks_uri().as_str().to_owned()),
        signing_keys: Some(metadata.jwks().keys().len()),
        warnings,
    }
}

/// One provider's cached metadata, and what it was fetched for.
struct Cached {
    issuer: String,
    allow_http_issuer: bool,
    fetched: Instant,
    metadata: Arc<ProviderMetadata>,
}

/// Each provider's discovery document and key set, by provider key, for
/// [`METADATA_CACHE_LIFETIME`]. An entry is used only for the issuer and
/// `AllowHttpIssuer` it was fetched with, so a provider edited since is
/// fetched again even before its entry is dropped. Saving or deleting a
/// provider drops its entry ([`invalidate`](Self::invalidate)).
#[derive(Default)]
pub struct MetadataCache {
    entries: Mutex<HashMap<String, Cached>>,
}

impl MetadataCache {
    /// `provider`'s cached metadata, while it is younger than
    /// [`METADATA_CACHE_LIFETIME`] and was fetched for its current issuer.
    #[must_use]
    pub fn get(&self, provider: &OidcProvider) -> Option<Arc<ProviderMetadata>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(&provider.key)
            .filter(|cached| {
                cached.issuer == provider.issuer
                    && cached.allow_http_issuer == provider.allow_http_issuer
                    && cached.fetched.elapsed() < METADATA_CACHE_LIFETIME
            })
            .map(|cached| Arc::clone(&cached.metadata))
    }

    /// Fetches `provider`'s metadata fresh ([`discover`]) and caches it, as a
    /// save's test does; a failure leaves the cache as it was.
    ///
    /// # Errors
    ///
    /// The [`DiscoveryFailure`] of [`discover`].
    pub async fn refresh(
        &self,
        http: &OidcHttpClient,
        provider: &OidcProvider,
    ) -> Result<Arc<ProviderMetadata>, DiscoveryFailure> {
        let metadata = Arc::new(discover(http, provider).await?);
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                provider.key.clone(),
                Cached {
                    issuer: provider.issuer.clone(),
                    allow_http_issuer: provider.allow_http_issuer,
                    fetched: Instant::now(),
                    metadata: Arc::clone(&metadata),
                },
            );
        Ok(metadata)
    }

    /// `provider`'s cached metadata ([`get`](Self::get)), else fetched fresh
    /// ([`refresh`](Self::refresh)).
    ///
    /// # Errors
    ///
    /// The [`DiscoveryFailure`] of [`discover`].
    pub async fn get_or_refresh(
        &self,
        http: &OidcHttpClient,
        provider: &OidcProvider,
    ) -> Result<Arc<ProviderMetadata>, DiscoveryFailure> {
        match self.get(provider) {
            Some(metadata) => Ok(metadata),
            None => self.refresh(http, provider).await,
        }
    }

    /// Drops the entry of the provider `key`, if there is one.
    pub fn invalidate(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

#[cfg(test)]
mod tests;
