//! Completing an OIDC sign-in (B5, B7): from a verified callback to a
//! session in jellyfin-web, without a token in any URL.
//!
//! 1. The callback answers the completion page ([`completion_page`]). It holds
//!    a one-time exchange code, random, kept for [`EXCHANGE_CODE_LIFETIME`]
//!    and bound to the flow cookie, which the page sets again for that long.
//! 2. Its script (`assets/sso/complete.js`, the one script the page's
//!    `Content-Security-Policy` lets it run) posts the code to
//!    `POST /sso/oidc/exchange` with jellyfin-web's device, keeps the
//!    `AuthenticationResult` it gets where jellyfin-web keeps its sessions,
//!    and replaces itself with `/web`.
//! 3. The exchange ([`SsoService::exchange_sign_in`]) uses the code up, checks
//!    its age and the cookie, and opens the session in
//!    [`SsoService::open_session`]: role sync
//!    ([`roles`](super::roles)), then the account rules of a password
//!    sign-in, then `SessionManager::authenticate_direct`, which applies device
//!    access and the session limit and logs the success
//!    (`AuthenticationSucceeded`). A refusal of a known account writes an
//!    `AuthenticationFailed` activity entry; any other refusal is one log
//!    line. Nothing here touches the account's failed-login counter. A
//!    Quick Connect confirmation ([`quick_connect`](super::quick_connect))
//!    is refused: it opens no session.
//!
//! Neither the code nor the token is ever logged (B15).

use std::sync::LazyLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::activity::LogLevel;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_traits::activity::ActivityLogCreate;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::session::{AuthenticationRequest, AuthenticationResultData};
use ferrofin_traits::sso::{OidcExchangeRefusal, OidcExchangeReply, SsoManager as _, SsoReply};
use openidconnect::CsrfToken;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::SsoService;
use super::flows::{EXCHANGE_CODE_LIFETIME, FlowPurpose, PendingExchange};
use super::sign_in::{
    VerifiedIdentity, cleared_flow_cookie, escape_html, flow_cookie, log_refusal, page,
    sign_in_provider,
};

/// The completion page's script.
const COMPLETE_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/sso/complete.js"
));

/// The completion page's `Content-Security-Policy`: its own script, by its
/// hash, and requests to this server only; nothing else is loaded, no form
/// posted, and the page is never framed.
static COMPLETE_POLICY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "default-src 'none'; script-src 'sha256-{}'; connect-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'",
        STANDARD.encode(Sha256::digest(COMPLETE_SCRIPT))
    )
});

/// What every refusal of an exchange that names no account rule shows.
const FAILED: &str = "Sign-in failed. Try again.";

/// The completion page for the exchange `code`, bound to the flow `cookie`,
/// which it sets again to live as long as the code (when a public URL names
/// its path).
pub(super) fn completion_page(code: &str, cookie: &Secret, public_url: Option<&str>) -> SsoReply {
    let mut reply = page(
        200,
        "Signing in",
        &format!(
            "<p id=\"status\" data-code=\"{code}\" data-version=\"{version}\">Signing you in to \
             Ferrofin…</p>\n<noscript><p>Signing in needs JavaScript.</p></noscript>\n\
             <script>{COMPLETE_SCRIPT}</script>",
            code = escape_html(code),
            version = escape_html(ferrofin_health::RELEASE_VERSION),
        ),
    );
    reply.set_cookies = public_url
        .map(|url| flow_cookie(url, cookie.expose(), EXCHANGE_CODE_LIFETIME))
        .into_iter()
        .collect();
    reply.content_security_policy = Some(COMPLETE_POLICY.clone());
    reply
}

/// A refusal of an exchange for a reason no account rule names: logged once,
/// with the provider when it is known, and the page's [`FAILED`].
pub(super) fn refused(provider: Option<&str>, reason: &'static str) -> OidcExchangeRefusal {
    if let Some(key) = provider {
        log_refusal(key, reason);
    } else {
        tracing::warn!(reason, "OIDC sign-in refused");
    }
    OidcExchangeRefusal {
        status: 400,
        message: FAILED.to_owned(),
    }
}

impl SsoService {
    /// Keeps the sign-in of `user` by `identity` for `purpose` under a new
    /// code, bound to the flow `cookie`, and answers the code.
    pub(super) fn issue_exchange_code(
        &self,
        user: &UserEntity,
        identity: VerifiedIdentity,
        cookie: &Secret,
        purpose: FlowPurpose,
    ) -> Result<String, ServiceError> {
        let user_id =
            Uuid::parse_str(&user.id).map_err(|error| ServiceError::backend(error.to_string()))?;
        let code = CsrfToken::new_random_len(32).into_secret();
        self.exchanges.insert(
            code.clone(),
            PendingExchange {
                binding_hash: Sha256::digest(cookie.expose().as_bytes()).into(),
                user_id,
                identity,
                purpose,
                created: std::time::Instant::now(),
            },
        );
        Ok(code)
    }

    /// [`SsoManager::exchange_oidc_sign_in`](ferrofin_traits::sso::SsoManager::exchange_oidc_sign_in).
    pub(super) async fn exchange_sign_in(
        &self,
        code: &Secret,
        flow_cookie: Option<&Secret>,
        request: AuthenticationRequest,
    ) -> Result<OidcExchangeReply, ServiceError> {
        self.ensure_enabled().await?;
        let config = self.config().await?;
        let outcome = match self.redeem(code, flow_cookie) {
            // A Quick Connect confirmation opens no browser session.
            Ok(pending) if pending.purpose != FlowPurpose::SignIn => Err(refused(
                Some(&pending.identity.provider_key),
                "wrong_purpose",
            )),
            Ok(pending) => match sign_in_provider(&config, &pending.identity.provider_key) {
                Ok(provider) => self.open_session(&provider, pending, request).await?,
                // Disabled or removed since the callback.
                Err(_) => Err(refused(
                    Some(&pending.identity.provider_key),
                    "provider_unavailable",
                )),
            },
            Err(reason) => Err(refused(None, reason)),
        };
        Ok(OidcExchangeReply {
            outcome,
            set_cookies: cleared_flow_cookie(self.public_url(&config).ok().flatten().as_deref()),
        })
    }

    /// Takes the exchange `code` names, once, while it is younger than
    /// [`EXCHANGE_CODE_LIFETIME`] and `flow_cookie` is the one it is bound
    /// to; else the reason it is refused.
    pub(super) fn redeem(
        &self,
        code: &Secret,
        flow_cookie: Option<&Secret>,
    ) -> Result<PendingExchange, &'static str> {
        let pending = self.exchanges.take(code.expose()).ok_or("unknown_code")?;
        if pending.expired() {
            return Err("expired_code");
        }
        let cookie = flow_cookie.ok_or("missing_cookie")?;
        if !ferrofin_common::cryptography::constant_time_eq(
            &Sha256::digest(cookie.expose().as_bytes()),
            &pending.binding_hash,
        ) {
            return Err("wrong_cookie");
        }
        Ok(pending)
    }

    /// Opens the session of a redeemed sign-in at `provider` with `request`'s
    /// device and address, or answers why not.
    ///
    /// Just before the session is minted, the account passes the rules of a
    /// password sign-in; whatever else must hold of the account, or change
    /// on it, at each sign-in belongs beside them. Just after it the sign-in
    /// is complete; work that must never delay or fail it belongs there,
    /// started without being awaited.
    async fn open_session(
        &self,
        provider: &OidcProvider,
        pending: PendingExchange,
        mut request: AuthenticationRequest,
    ) -> Result<Result<AuthenticationResultData, OidcExchangeRefusal>, ServiceError> {
        let key = provider.key.as_str();
        let Some(user) = self.users.get_user_by_id(pending.user_id).await? else {
            return Ok(Err(refused(Some(key), "no_account")));
        };
        let remote_endpoint = request.remote_endpoint.clone().unwrap_or_default();
        // Role sync first, as a password sign-in applies its provider's
        // administrator flag before the account rules.
        self.sync_roles(provider, &pending.identity, pending.user_id)
            .await?;
        // Disabled, remote access, parental schedule: `authenticate_direct`
        // leaves them out. The refusal is logged by the user manager.
        if let Err(error) = self
            .users
            .ensure_sign_in_allowed(&user, &remote_endpoint)
            .await
        {
            return self
                .account_refused(&user, &remote_endpoint, error)
                .await
                .map(Err);
        }
        request.user_id = Some(pending.user_id);
        request.username = None;
        request.password = None;
        match self.sessions.authenticate_direct(&request).await {
            Ok(session) => {
                tracing::info!(oidc_provider = %key, user_id = %user.id, "OIDC sign-in completed");
                // Not awaited: the picture never holds the sign-in (B16).
                self.start_avatar_sync(provider, &pending.identity, pending.user_id);
                Ok(Ok(session))
            }
            // A device the account may not use, or its session limit: logged
            // by the session manager.
            Err(error @ ServiceError::Forbidden(_)) => self
                .account_refused(&user, &remote_endpoint, error)
                .await
                .map(Err),
            // The page always sends its device.
            Err(ServiceError::InvalidInput(_)) => Ok(Err(refused(Some(key), "no_device"))),
            // Deleted since the callback.
            Err(ServiceError::Unauthorized(_)) => Ok(Err(refused(Some(key), "no_account"))),
            Err(error) => Err(error),
        }
    }

    /// The refusal of `user` by a sign-in rule (`error`, a
    /// [`ServiceError::Forbidden`]; any other is passed on): an
    /// `AuthenticationFailed` activity entry as a refused password sign-in
    /// writes, and the rule's message for the page.
    pub(super) async fn account_refused(
        &self,
        user: &UserEntity,
        remote_endpoint: &str,
        error: ServiceError,
    ) -> Result<OidcExchangeRefusal, ServiceError> {
        let ServiceError::Forbidden(message) = error else {
            return Err(error);
        };
        let entry = ActivityLogCreate {
            name: format!("Failed login attempt from {}", user.username),
            type_: "AuthenticationFailed".to_owned(),
            severity: LogLevel::Error,
            short_overview: Some(format!("IP address: {remote_endpoint}")),
            ..ActivityLogCreate::default()
        };
        // Best-effort, as the password handler's: the feed never fails a
        // sign-in.
        if let Err(error) = self.activity.create_entry(entry).await {
            tracing::warn!(%error, activity_type = "AuthenticationFailed", "could not write the activity entry");
        }
        Ok(OidcExchangeRefusal {
            status: 403,
            message,
        })
    }
}

#[cfg(test)]
mod tests;
