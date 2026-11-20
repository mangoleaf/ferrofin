//! OIDC sign-in (B4): `GET /sso/oidc/{provider}/start` sends the browser to
//! the identity provider with the authorization code flow, PKCE `S256`, a
//! `state` and a nonce, and binds the attempt to that browser with a cookie;
//! `GET /sso/oidc/{provider}/callback` accepts only a response that passes
//! every check, and hands the [`VerifiedIdentity`] it ends with to
//! `SsoService::sign_in_verified`, which finds or creates its account
//! ([`accounts`](super::accounts)); the callback then answers the completion
//! page, whose script swaps an exchange code for the session
//! ([`complete`](super::complete)).
//!
//! The callback's checks, in order; the first that fails refuses it:
//! 1. a pending flow for its `state`, at this provider, not expired, and
//!    started under the provider's current issuer and client id (taken
//!    whatever the outcome, so a `state` works once);
//! 2. the flow cookie, whose hash the flow keeps, compared in constant time;
//! 3. no `error` from the identity provider;
//! 4. an `iss` equal to the issuer (RFC 9207), present when the provider
//!    advertises it;
//! 5. the code exchanged with the PKCE verifier and the client secret;
//! 6. the ID token's signature, with an asymmetric algorithm the provider
//!    advertises and its keys (fetched again once for an unknown key id);
//! 7. its issuer, audience (the client id; another beside it only with an
//!    `azp` naming the client), `azp`, expiry and nonce;
//! 8. when the username claim, or the role claim of a provider that reads
//!    roles, is not in the ID token, the userinfo answer's `sub`, which must
//!    be the ID token's. Asked only for the `picture` claim of a provider
//!    that syncs pictures ([`avatar_sync`](super::avatar_sync)), userinfo
//!    refuses nothing: an answer that fails is not used, and of one that
//!    passes only the `picture` is taken.
//!
//! `SsoService::sign_in_verified` then refuses an identity without the
//! provider's required role ([`roles`](super::roles)) before any account is
//! looked up or created.
//!
//! A refusal is one page for the browser and one `warn` line naming the
//! provider and a reason code: never the code, a token, the verifier, the
//! cookie or a claim (B15). A failed check's page says nothing of why; a
//! refusal about the account says what to ask an administrator.
//!
//! One cookie name serves every flow, so a browser holds one sign-in at a
//! time: a second start, in another tab or at another provider, replaces the
//! first's cookie, and the first's callback is then refused.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ferrofin_common::cryptography::constant_time_eq;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::sso::{OIDC_FLOW_COOKIE, OidcCallback, SsoManager as _, SsoReply};
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreClientAuthMethod, CoreErrorResponseType, CoreIdToken,
    CoreIdTokenVerifier, CoreTokenResponse,
};
use openidconnect::{
    AccessToken, AuthType, AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret,
    CsrfToken, JwsSigningAlgorithm as _, Nonce, OAuth2TokenResponse as _, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, RequestTokenError, Scope, SignatureVerificationError,
    TokenResponse as _,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::complete::completion_page;
use super::discovery::ProviderMetadata;
use super::flows::{FLOW_LIFETIME, FlowPurpose, PendingFlow};
use super::{SsoConfig, SsoService, roles};

/// Who signed in, as the identity provider vouched for it: every value here
/// comes from an ID token whose signature, issuer, audience, `azp`, expiry
/// and nonce were checked, or from the userinfo answer for its subject.
/// `Debug` leaves the claims out (B15).
#[derive(Clone, PartialEq)]
pub struct VerifiedIdentity {
    /// The key of the provider signed in at.
    pub provider_key: String,
    /// The issuer, as the ID token's `iss` names it.
    pub issuer: String,
    /// The ID token's `sub`: with the issuer, what an account is linked by.
    pub subject: String,
    /// The value of the provider's username claim, when it is a string.
    pub username: Option<String>,
    /// The ID token's claims, then the userinfo answer's the ID token lacks
    /// (only its `picture`, when userinfo was asked for the picture alone).
    pub claims: Map<String, Value>,
}

impl std::fmt::Debug for VerifiedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedIdentity")
            .field("provider_key", &self.provider_key)
            .field("issuer", &self.issuer)
            .field("subject", &self.subject)
            .field("username", &self.username)
            .field("claims", &self.claims.len())
            .finish()
    }
}

/// Why a callback is refused: a reason code for the log, the page's status,
/// and what the page tells the user to do, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Refusal {
    pub(super) reason: &'static str,
    pub(super) status: u16,
    /// For a refusal the user can act on, what to do; `None` is the page
    /// that says nothing of why (B15).
    pub(super) advice: Option<&'static str>,
}

impl Refusal {
    /// A response that fails a check: `400`.
    pub(super) const fn bad(reason: &'static str) -> Self {
        Self {
            reason,
            status: 400,
            advice: None,
        }
    }

    /// The identity provider could not be reached or answered nonsense:
    /// `502`.
    const fn upstream(reason: &'static str) -> Self {
        Self {
            reason,
            status: 502,
            advice: None,
        }
    }

    /// No account to sign in to: `403`, with what to ask an administrator.
    pub(super) const fn account(reason: &'static str, advice: &'static str) -> Self {
        Self {
            reason,
            status: 403,
            advice: Some(advice),
        }
    }

    /// Logs the refusal at `key` ([`log_refusal`]) and answers its page.
    fn refused(self, key: &str) -> SsoReply {
        log_refusal(key, self.reason);
        match self.advice {
            Some(advice) => page(
                self.status,
                "Sign-in refused",
                &format!("<p>{}</p>", escape_html(advice)),
            ),
            None => refusal_page(self.status),
        }
    }
}

/// The provider `key` while it can sign users in: stored, accepted by the
/// checks and enabled. Any other is no such provider.
pub(super) fn sign_in_provider(
    config: &SsoConfig,
    key: &str,
) -> Result<OidcProvider, ServiceError> {
    config
        .checked_providers()
        .into_iter()
        .find_map(|(provider, refused)| {
            (provider.key == key && provider.enabled && refused.is_none()).then_some(provider)
        })
        .ok_or_else(|| ServiceError::not_found("no such OIDC provider"))
}

/// The flow cookie's `Set-Cookie` value, carrying `value` for `max_age`, or
/// clearing it with an empty value and no age: scoped to `/sso` below the
/// public URL's path, `HttpOnly`, `SameSite=Lax` (the callback is a top-level
/// navigation), and `Secure` on an `https` public URL.
pub(super) fn flow_cookie(public_url: &str, value: &str, max_age: Duration) -> String {
    let base_path = url::Url::parse(public_url).map_or_else(
        |_| String::new(),
        |url| url.path().trim_end_matches('/').to_owned(),
    );
    let max_age = max_age.as_secs();
    let secure = if public_url.starts_with("https:") {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{OIDC_FLOW_COOKIE}={value}; Path={base_path}/sso; Max-Age={max_age}; HttpOnly; \
         SameSite=Lax{secure}"
    )
}

/// The `Set-Cookie` value that clears the flow cookie, when a public URL
/// names its path.
pub(super) fn cleared_flow_cookie(public_url: Option<&str>) -> Vec<String> {
    public_url
        .map(|url| flow_cookie(url, "", Duration::ZERO))
        .into_iter()
        .collect()
}

/// The `Content-Security-Policy` of a page without a script: it loads
/// nothing, posts no form, and is never framed.
const PAGE_POLICY: &str =
    "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// `text` with the characters HTML gives a meaning escaped.
pub(super) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A plain page with `title` and `body` (HTML, already escaped), and a link
/// back to jellyfin-web, under [`PAGE_POLICY`]. The link is relative, from
/// `/sso/oidc/{key}/…`, so a server under a base URL keeps it.
pub(super) fn page(status: u16, title: &str, body: &str) -> SsoReply {
    page_linking_home(status, title, body, "../../../web/")
}

/// [`page`] for a route whose relative link back to jellyfin-web is `home`.
pub(super) fn page_linking_home(status: u16, title: &str, body: &str, home: &str) -> SsoReply {
    SsoReply {
        status,
        location: None,
        html: format!(
            "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>{title}</title></head>\n<body><main><h1>{title}</h1>\n{body}\n\
             <p><a href=\"{home}\">Back to Ferrofin</a></p></main></body></html>\n"
        ),
        set_cookies: Vec::new(),
        content_security_policy: Some(PAGE_POLICY.to_owned()),
    }
}

/// The one page every failed check shows: it says nothing of why (B15).
fn refusal_page(status: u16) -> SsoReply {
    page(
        status,
        "Sign-in failed",
        "<p>Sign-in failed. Try again.</p>",
    )
}

/// Logs `reason` once, at `warn`, with the provider key only.
pub(super) fn log_refusal(key: &str, reason: &'static str) {
    tracing::warn!(oidc_provider = %key, reason, "OIDC sign-in refused");
}

/// Whether the browser may be sent to `url`: an `https` URL, or an `http` one
/// for a provider whose `AllowHttpIssuer` is on (D11), as every URL the server
/// fetches from it.
fn browser_url_allowed(url: &url::Url, allow_http: bool) -> bool {
    url.scheme() == "https" || (allow_http && url.scheme() == "http")
}

/// How the client authenticates at the token endpoint: HTTP Basic, the
/// default, unless the provider lists `client_secret_post` and not
/// `client_secret_basic`.
fn auth_type(metadata: &ProviderMetadata) -> AuthType {
    match metadata.token_endpoint_auth_methods_supported() {
        Some(methods)
            if methods.contains(&CoreClientAuthMethod::ClientSecretPost)
                && !methods.contains(&CoreClientAuthMethod::ClientSecretBasic) =>
        {
            AuthType::RequestBody
        }
        _ => AuthType::BasicAuth,
    }
}

/// The verifier of `provider`'s ID tokens under `metadata`: its issuer, its
/// client id among the audiences, its keys, and the asymmetric algorithms it
/// advertises only, so neither `none` nor an HMAC keyed with the client
/// secret passes. Other audiences beside the client id pass here, and
/// [`SsoService::verify_id_token`] then requires an `azp` naming the client
/// (OpenID Connect Core 1.0, 3.1.3.7, steps 3 to 5).
fn id_token_verifier<'a>(
    provider: &OidcProvider,
    metadata: &ProviderMetadata,
) -> Result<CoreIdTokenVerifier<'a>, Refusal> {
    let algorithms: Vec<_> = metadata
        .id_token_signing_alg_values_supported()
        .iter()
        .filter(|algorithm| algorithm.key_type().is_some() && !algorithm.uses_shared_secret())
        .cloned()
        .collect();
    if algorithms.is_empty() {
        return Err(Refusal::bad("id_token_algorithm"));
    }
    Ok(CoreIdTokenVerifier::new_public_client(
        ClientId::new(provider.client_id.clone()),
        metadata.issuer().clone(),
        metadata.jwks().clone(),
    )
    .set_allowed_algs(algorithms)
    .set_other_audience_verifier_fn(|_| true))
}

/// The reason code of an ID token that fails verification.
fn id_token_reason(e: &ClaimsVerificationError) -> &'static str {
    match e {
        ClaimsVerificationError::InvalidIssuer(_) => "id_token_issuer",
        ClaimsVerificationError::InvalidAudience(_) => "id_token_audience",
        ClaimsVerificationError::Expired(_) => "id_token_expired",
        ClaimsVerificationError::InvalidNonce(_) => "id_token_nonce",
        ClaimsVerificationError::SignatureVerification(_) => "id_token_signature",
        _ => "id_token_invalid",
    }
}

/// Every claim of `id_token`, read from its payload: only ever called once
/// its signature and claims have been verified.
fn verified_payload(id_token: &CoreIdToken) -> Option<Map<String, Value>> {
    let compact = id_token.to_string();
    let payload = compact.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

impl SsoService {
    /// [`SsoManager::start_oidc_sign_in`](ferrofin_traits::sso::SsoManager::start_oidc_sign_in),
    /// for what its verified identity is for, its `purpose`.
    pub(super) async fn start_sign_in(
        &self,
        key: &str,
        client: IpAddr,
        purpose: FlowPurpose,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        let config = self.config().await?;
        let provider = sign_in_provider(&config, key)?;
        let Some(public_url) = self.public_url(&config).ok().flatten() else {
            log_refusal(key, "public_url");
            return Ok(page(
                503,
                "Single Sign-On is not set up",
                "<p>Sign-in needs the server's public URL, which its callback URL is built on. \
                 An administrator sets it as the Public URL on the Single Sign-On settings page, \
                 or as the server's published URL.</p>",
            ));
        };
        let Ok(metadata) = self.metadata.get_or_refresh(&self.http, &provider).await else {
            log_refusal(key, "discovery");
            return Ok(refusal_page(502));
        };
        let redirect_uri = format!("{public_url}/sso/oidc/{key}/callback");
        let redirect = RedirectUrl::new(redirect_uri.clone())
            .map_err(|_| ServiceError::backend("the callback URL is not a URL"))?;
        let oidc = CoreClient::from_provider_metadata(
            (*metadata).clone(),
            ClientId::new(provider.client_id.clone()),
            None,
        )
        .set_redirect_uri(redirect);
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = oidc
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(challenge);
        // `openid` is always sent first.
        for scope in provider
            .scopes
            .split_ascii_whitespace()
            .filter(|scope| *scope != "openid")
        {
            request = request.add_scope(Scope::new(scope.to_owned()));
        }
        let (location, state, nonce) = request.url();
        // The browser goes there: `http` only where the issuer may be (D11).
        if !browser_url_allowed(&location, provider.allow_http_issuer) {
            log_refusal(key, "authorization_endpoint");
            return Ok(refusal_page(502));
        }
        let binding = CsrfToken::new_random_len(32);
        self.flows.insert(
            state.into_secret(),
            PendingFlow {
                provider_key: key.to_owned(),
                issuer: provider.issuer.clone(),
                client_id: provider.client_id.clone(),
                redirect_uri,
                verifier,
                nonce,
                binding_hash: Sha256::digest(binding.secret().as_bytes()).into(),
                created: Instant::now(),
                client,
                purpose,
            },
        );
        tracing::debug!(oidc_provider = %key, "OIDC sign-in started");
        Ok(SsoReply {
            status: 302,
            location: Some(location.into()),
            html: String::new(),
            set_cookies: vec![flow_cookie(&public_url, binding.secret(), FLOW_LIFETIME)],
            content_security_policy: None,
        })
    }

    /// [`SsoManager::finish_oidc_sign_in`](ferrofin_traits::sso::SsoManager::finish_oidc_sign_in).
    pub(super) async fn finish_sign_in(
        &self,
        key: &str,
        callback: OidcCallback,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        let config = self.config().await?;
        let provider = sign_in_provider(&config, key)?;
        let public_url = self.public_url(&config).ok().flatten();
        // What the verified identity is for decides how the flow ends: each
        // purpose but a sign-in has its own arm, ending on its own page with
        // no session.
        let mut reply = match self.verify_callback(&provider, callback).await {
            Ok((FlowPurpose::SignIn, identity, cookie)) => {
                tracing::info!(oidc_provider = %key, "OIDC sign-in verified");
                match self.sign_in_verified(&provider, identity, &cookie).await? {
                    // Keeps the cookie, the code's binding: the exchange
                    // clears it.
                    Ok(code) => return Ok(completion_page(&code, &cookie, public_url.as_deref())),
                    Err(refusal) => refusal.refused(key),
                }
            }
            Ok((FlowPurpose::QuickConnect(code), identity, cookie)) => {
                match self
                    .quick_connect_verified(
                        &provider,
                        identity,
                        code,
                        &cookie,
                        public_url.as_deref(),
                    )
                    .await?
                {
                    // Keeps the cookie, the confirmation's binding.
                    Ok(confirmation) => return Ok(confirmation),
                    Err(refusal) => refusal.refused(key),
                }
            }
            Err(refusal) => refusal.refused(key),
        };
        reply.set_cookies = cleared_flow_cookie(public_url.as_deref());
        Ok(reply)
    }

    /// Hands a verified sign-in to its account, found by its link or created
    /// ([`account`](Self::account)), and keeps it for the exchange under a
    /// new code bound to the flow `cookie`: `Ok(code)`, or the refusal saying
    /// what to ask an administrator. A check that must refuse before any
    /// account exists goes before [`account`](Self::account).
    async fn sign_in_verified(
        &self,
        provider: &OidcProvider,
        identity: VerifiedIdentity,
        cookie: &Secret,
    ) -> Result<Result<String, Refusal>, ServiceError> {
        if let Err(refusal) = roles::check_required_role(provider, &identity) {
            return Ok(Err(refusal));
        }
        let user = match self.account(provider, &identity).await? {
            Ok(user) => user,
            Err(refusal) => return Ok(Err(refusal)),
        };
        Ok(Ok(self.issue_exchange_code(
            &user,
            identity,
            cookie,
            FlowPurpose::SignIn,
        )?))
    }

    /// Runs the callback's checks, in the order of the module's list, and
    /// answers what its verified identity is for, the identity, and the flow
    /// cookie that bound it to the browser.
    pub(super) async fn verify_callback(
        &self,
        provider: &OidcProvider,
        callback: OidcCallback,
    ) -> Result<(FlowPurpose, VerifiedIdentity, Secret), Refusal> {
        let flow = callback
            .state
            .as_deref()
            .and_then(|state| self.flows.take(state))
            .ok_or(Refusal::bad("unknown_state"))?;
        if flow.expired() {
            return Err(Refusal::bad("expired_state"));
        }
        if flow.provider_key != provider.key {
            return Err(Refusal::bad("wrong_provider"));
        }
        // An edit since the start would send the code to another issuer.
        if flow.issuer != provider.issuer || flow.client_id != provider.client_id {
            return Err(Refusal::bad("provider_changed"));
        }
        let cookie = callback.flow_cookie.ok_or(Refusal::bad("missing_cookie"))?;
        if !constant_time_eq(
            &Sha256::digest(cookie.expose().as_bytes()),
            &flow.binding_hash,
        ) {
            return Err(Refusal::bad("wrong_cookie"));
        }
        if callback.error.is_some() {
            return Err(Refusal::bad("provider_error"));
        }
        let metadata = self
            .metadata
            .get_or_refresh(&self.http, provider)
            .await
            .map_err(|_| Refusal::upstream("discovery"))?;
        match &callback.iss {
            Some(iss) if *iss != provider.issuer => return Err(Refusal::bad("wrong_iss")),
            None if metadata
                .additional_metadata()
                .authorization_response_iss_parameter_supported
                == Some(true) =>
            {
                return Err(Refusal::bad("missing_iss"));
            }
            _ => {}
        }
        let code = callback.code.ok_or(Refusal::bad("missing_code"))?;
        let tokens = self
            .exchange_code(
                provider,
                &metadata,
                &flow.redirect_uri,
                flow.verifier,
                &code,
            )
            .await?;
        let id_token = tokens.id_token().ok_or(Refusal::bad("no_id_token"))?;
        let (subject, issuer) = self
            .verify_id_token(provider, Arc::clone(&metadata), id_token, &flow.nonce)
            .await?;
        let mut claims = verified_payload(id_token).ok_or(Refusal::bad("id_token_invalid"))?;
        if !claims.contains_key(&provider.username_claim)
            || (roles::reads_roles(provider) && !claims.contains_key(&provider.role_claim))
        {
            for (name, value) in self
                .userinfo(provider, &metadata, tokens.access_token(), &subject)
                .await?
            {
                claims.entry(name).or_insert(value);
            }
        } else if super::avatar_sync::wants_userinfo(provider, &claims) {
            // Asked for the picture only, which never fails a sign-in (B16).
            match self
                .userinfo(provider, &metadata, tokens.access_token(), &subject)
                .await
            {
                Ok(mut answer) => {
                    if let Some(picture) = answer.remove(super::avatar_sync::PICTURE_CLAIM) {
                        claims.insert(super::avatar_sync::PICTURE_CLAIM.to_owned(), picture);
                    }
                }
                Err(refusal) => tracing::warn!(
                    oidc_provider = %provider.key,
                    reason = refusal.reason,
                    "the userinfo answer for the picture claim is not used"
                ),
            }
        }
        let username = claims
            .get(&provider.username_claim)
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok((
            flow.purpose,
            VerifiedIdentity {
                provider_key: provider.key.clone(),
                issuer,
                subject,
                username,
                claims,
            },
            cookie,
        ))
    }

    /// Exchanges `code` at `provider`'s token endpoint, with the PKCE
    /// `verifier`, the client secret and the `redirect_uri` the
    /// authorization request named.
    async fn exchange_code(
        &self,
        provider: &OidcProvider,
        metadata: &ProviderMetadata,
        redirect_uri: &str,
        verifier: PkceCodeVerifier,
        code: &Secret,
    ) -> Result<CoreTokenResponse, Refusal> {
        let secret = self
            .secrets()
            .await
            .map_err(|_| Refusal {
                reason: "private_data",
                status: 500,
                advice: None,
            })?
            .oidc_client_secrets
            .get(&provider.key)
            .cloned()
            .ok_or(Refusal::bad("client_secret"))?;
        let redirect =
            RedirectUrl::new(redirect_uri.to_owned()).map_err(|_| Refusal::bad("redirect_uri"))?;
        let oidc = CoreClient::from_provider_metadata(
            metadata.clone(),
            ClientId::new(provider.client_id.clone()),
            Some(ClientSecret::new(secret.expose().to_owned())),
        )
        .set_redirect_uri(redirect)
        .set_auth_type(auth_type(metadata));
        let http = self.http.allowing_http(provider.allow_http_issuer);
        oidc.exchange_code(AuthorizationCode::new(code.expose().to_owned()))
            .map_err(|_| Refusal::upstream("token_endpoint"))?
            .set_pkce_verifier(verifier)
            .request_async(&http)
            .await
            .map_err(|e| match e {
                // RFC 6749's error codes are no secret: they tell a wrong
                // client secret from a used code.
                RequestTokenError::ServerResponse(answer) => Refusal::bad(match answer.error() {
                    CoreErrorResponseType::InvalidClient => "token_invalid_client",
                    CoreErrorResponseType::InvalidGrant => "token_invalid_grant",
                    _ => "token_error",
                }),
                _ => Refusal::upstream("token_request"),
            })
    }

    /// Verifies `id_token` against `metadata`, fetching the provider's keys
    /// again once when it names a key they lack, and answers its subject and
    /// issuer.
    async fn verify_id_token(
        &self,
        provider: &OidcProvider,
        mut metadata: Arc<ProviderMetadata>,
        id_token: &CoreIdToken,
        nonce: &Nonce,
    ) -> Result<(String, String), Refusal> {
        let mut refetched = false;
        loop {
            let verifier = id_token_verifier(provider, &metadata)?;
            match id_token.claims(&verifier, nonce) {
                Ok(claims) => {
                    // `openidconnect` leaves `azp` unchecked.
                    let azp = claims.authorized_party().map(|azp| azp.as_str());
                    if azp.is_some_and(|azp| azp != provider.client_id) {
                        return Err(Refusal::bad("id_token_azp"));
                    }
                    if claims.audiences().len() > 1 && azp.is_none() {
                        return Err(Refusal::bad("id_token_audience"));
                    }
                    return Ok((
                        claims.subject().as_str().to_owned(),
                        claims.issuer().as_str().to_owned(),
                    ));
                }
                Err(ClaimsVerificationError::SignatureVerification(
                    SignatureVerificationError::NoMatchingKey,
                )) if !refetched => {
                    refetched = true;
                    metadata = self
                        .metadata
                        .refresh(&self.http, provider)
                        .await
                        .map_err(|_| Refusal::upstream("discovery"))?;
                }
                Err(e) => return Err(Refusal::bad(id_token_reason(&e))),
            }
        }
    }

    /// The provider's userinfo answer for `access_token`, which must name
    /// `subject`; nothing when the provider has no userinfo endpoint.
    async fn userinfo(
        &self,
        provider: &OidcProvider,
        metadata: &ProviderMetadata,
        access_token: &AccessToken,
        subject: &str,
    ) -> Result<Map<String, Value>, Refusal> {
        let Some(endpoint) = metadata.userinfo_endpoint() else {
            return Ok(Map::new());
        };
        let request = openidconnect::http::Request::builder()
            .uri(endpoint.as_str())
            .header(
                openidconnect::http::header::AUTHORIZATION,
                format!("Bearer {}", access_token.secret()),
            )
            .header(openidconnect::http::header::ACCEPT, "application/json")
            .body(Vec::new())
            .map_err(|_| Refusal::upstream("userinfo"))?;
        let answer = self
            .http
            .allowing_http(provider.allow_http_issuer)
            .send(request)
            .await
            .map_err(|_| Refusal::upstream("userinfo"))?;
        if !answer.status().is_success() {
            return Err(Refusal::upstream("userinfo"));
        }
        let claims: Map<String, Value> =
            serde_json::from_slice(answer.body()).map_err(|_| Refusal::upstream("userinfo"))?;
        if claims.get("sub").and_then(Value::as_str) != Some(subject) {
            return Err(Refusal::bad("userinfo_subject"));
        }
        Ok(claims)
    }
}

// The accounts' tests sign in through its `Harness`.
#[cfg(test)]
pub(super) mod tests;
