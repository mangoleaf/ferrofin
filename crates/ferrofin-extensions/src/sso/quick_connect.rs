//! Approving a native app's Quick Connect code by signing in through Single
//! Sign-On (B13, D8): the app shows its code, the user approves it in any
//! browser, and no client changes.
//!
//! 1. `GET /sso/quickconnect` ([`SsoService::code_entry_page`]): a form for
//!    the code, with a button for each provider that can sign users in, each
//!    starting `GET /sso/oidc/{provider}/start?quickConnect=CODE`.
//! 2. That start ([`SsoService::start_quick_connect`]) takes only a code as
//!    the server draws them ([`QuickConnectCode`]), and starts a sign-in whose
//!    purpose is [`FlowPurpose::QuickConnect`].
//! 3. Its callback is checked as a sign-in's, refused without the provider's
//!    required role, and the account found or created as for one
//!    ([`SsoService::quick_connect_verified`]). It answers
//!    the confirmation page, which names the code and the account and holds a
//!    one-time confirmation: an exchange code of the browser sign-in's kind,
//!    bound to the flow cookie, which the page keeps as long.
//! 4. Only the page's `POST /sso/quickconnect`
//!    ([`SsoService::confirm_quick_connect`]), with that cookie, uses the
//!    confirmation up, checks the identity is still linked to the account,
//!    applies role sync and the account rules of a password sign-in for the
//!    approving browser's address, and approves the code with
//!    `QuickConnect::authorize_request`, which signs the app's device in
//!    through `authenticate_direct` (device access, the session limit and the
//!    `AuthenticationSucceeded` entry).
//!
//! The confirmation is not optional: a start link carrying a code can be sent
//! to someone else, and approving it as soon as their identity provider let
//! them through would sign the sender's device in as them. They see the code
//! and the account on a Ferrofin page, after signing in, and agree to it, as
//! jellyfin-web has its user type the code.
//!
//! No browser session is opened: the exchange refuses a confirmation. A
//! service built without a Quick Connect manager
//! ([`SsoService::with_quick_connect`]) answers `404` on these routes.

use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::OidcProvider;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::security::QuickConnect;
use ferrofin_traits::sso::{SsoManager as _, SsoReply};

use super::complete::refused;
use super::flows::{EXCHANGE_CODE_LIFETIME, FlowPurpose};
use super::sign_in::{
    Refusal, VerifiedIdentity, cleared_flow_cookie, escape_html, flow_cookie, log_refusal, page,
    page_linking_home, sign_in_provider,
};
use super::{SsoConfig, SsoService, roles};

/// The digits of a Quick Connect code: upstream `QuickConnectManager`'s
/// `CodeLength`, which the server's manager draws its codes with.
pub const CODE_LENGTH: usize = 6;

/// jellyfin-web from `/sso/quickconnect`.
const HOME: &str = "../web/";

/// The confirmation page's `Content-Security-Policy`: as every Single
/// Sign-On page's, but its one form posts to this server.
const CONFIRMATION_POLICY: &str =
    "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// What the start answers for a `quickConnect` that is not a code.
const NOT_A_CODE: &str = "<p>That is not a Quick Connect code: the code is the six digits your \
                          app shows.</p>\n<p><a href=\"../../quickconnect\">Enter the code \
                          again</a></p>";

/// What the approval answers for a code no app is waiting with.
const UNKNOWN_CODE: &str = "<p>No app is waiting with this code: it may have expired. Start \
                            Quick Connect again in your app, and enter its new code.</p>";

/// What the approval answers for a code approved already.
const APPROVED_ALREADY: &str = "<p>This code is approved already. If your app is not signed in, \
                                start Quick Connect again in it.</p>";

/// A Quick Connect code as the server draws them: [`CODE_LENGTH`] ASCII
/// digits. `Debug` leaves the digits out (B15).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct QuickConnectCode([u8; CODE_LENGTH]);

impl QuickConnectCode {
    /// `text`, without surrounding spaces, as a code when it is one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let digits: [u8; CODE_LENGTH] = text.trim().as_bytes().try_into().ok()?;
        digits
            .iter()
            .all(u8::is_ascii_digit)
            .then_some(Self(digits))
    }

    /// The code's digits.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // ASCII digits only, so always UTF-8.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl std::fmt::Debug for QuickConnectCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QuickConnectCode(..)")
    }
}

/// A page of `/sso/quickconnect`, linking back to jellyfin-web from there.
fn at_home(status: u16, title: &str, body: &str) -> SsoReply {
    page_linking_home(status, title, body, HOME)
}

/// The `403` page of Quick Connect turned off on the server, from a route
/// whose link back to jellyfin-web is `home`.
fn turned_off(home: &str) -> SsoReply {
    page_linking_home(
        403,
        "Quick Connect is off",
        "<p>Quick Connect is turned off on this server. An administrator can turn it on with \
         \u{201c}Enable Quick Connect on this server\u{201d} in the dashboard.</p>",
        home,
    )
}

/// A refused confirmation that names no rule: logged once, as the exchange's
/// refusals are, and the page saying to start again.
fn failed(provider: Option<&str>, reason: &'static str) -> SsoReply {
    let refusal = refused(provider, reason);
    at_home(
        refusal.status,
        "Approval failed",
        &format!(
            "<p>{}</p>\n<p>If your app is signed in already, you can close this page. If not, \
             <a href=\"quickconnect\">enter its code again</a>.</p>",
            escape_html(&refusal.message)
        ),
    )
}

/// The providers of `config` that can sign users in, in its order.
fn sign_in_providers(config: &SsoConfig) -> Vec<OidcProvider> {
    config
        .checked_providers()
        .into_iter()
        .filter_map(|(provider, refused)| {
            (provider.enabled && refused.is_none()).then_some(provider)
        })
        .collect()
}

impl SsoService {
    /// The service with Quick Connect approval (B13), through `quick_connect`:
    /// without it, the Quick Connect routes answer `404`.
    #[must_use]
    pub fn with_quick_connect(mut self, quick_connect: Arc<dyn QuickConnect>) -> Self {
        self.quick_connect = Some(quick_connect);
        self
    }

    /// The Quick Connect manager, or the `404` of a service without one.
    fn quick_connect_manager(&self) -> Result<Arc<dyn QuickConnect>, ServiceError> {
        self.quick_connect
            .clone()
            .ok_or_else(|| ServiceError::not_found("Quick Connect approval is not available"))
    }

    /// [`SsoManager::quick_connect_page`](ferrofin_traits::sso::SsoManager::quick_connect_page).
    pub(super) async fn code_entry_page(&self) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        let quick_connect = self.quick_connect_manager()?;
        let providers = sign_in_providers(&self.config().await?);
        // B1: with no provider to sign in with, there is no such page.
        if providers.is_empty() {
            return Err(ServiceError::not_found(
                "no OIDC provider can sign users in",
            ));
        }
        if !quick_connect.is_enabled().await? {
            return Ok(turned_off(HOME));
        }
        let mut buttons = String::new();
        for provider in &providers {
            // The key passed `validate_key`: lowercase letters, digits and
            // `-`. Writing to a `String` cannot fail.
            let _ = writeln!(
                buttons,
                "<p><button type=\"submit\" formaction=\"oidc/{key}/start\">Sign in with \
                 {name}</button></p>",
                key = provider.key,
                name = escape_html(&provider.name),
            );
        }
        let mut reply = at_home(
            200,
            "Sign in an app with Quick Connect",
            &format!(
                "<p>Enter the code your app shows, then sign in. You confirm the code before \
                 the app is signed in.</p>\n<form method=\"get\">\n<p><label \
                 for=\"code\">Code</label> <input id=\"code\" name=\"quickConnect\" required \
                 inputmode=\"numeric\" pattern=\"[0-9]{{{CODE_LENGTH}}}\" \
                 maxlength=\"{CODE_LENGTH}\" autocomplete=\"off\" autofocus></p>\n{buttons}</form>"
            ),
        );
        // A button's start sends the browser on to the identity provider, and
        // browsers hold a form's redirects to its `form-action` too: `https`,
        // or `http` where a provider's issuer may be (D11).
        let http = if providers.iter().any(|p| p.allow_http_issuer) {
            " http:"
        } else {
            ""
        };
        reply.content_security_policy = Some(format!(
            "default-src 'none'; base-uri 'none'; form-action 'self' https:{http}; \
             frame-ancestors 'none'"
        ));
        Ok(reply)
    }

    /// [`SsoManager::start_oidc_quick_connect`](ferrofin_traits::sso::SsoManager::start_oidc_quick_connect).
    pub(super) async fn start_quick_connect(
        &self,
        key: &str,
        client: IpAddr,
        code: &str,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        let quick_connect = self.quick_connect_manager()?;
        // An unknown provider is no such route, whatever the code.
        sign_in_provider(&self.config().await?, key)?;
        // User input: only a code goes on, and it is never shown back.
        let Some(code) = QuickConnectCode::parse(code) else {
            log_refusal(key, "quick_connect_code");
            return Ok(page(400, "Not a Quick Connect code", NOT_A_CODE));
        };
        if !quick_connect.is_enabled().await? {
            log_refusal(key, "quick_connect_off");
            return Ok(turned_off("../../../web/"));
        }
        self.start_sign_in(key, client, FlowPurpose::QuickConnect(code))
            .await
    }

    /// Finds or creates the account a verified Quick Connect sign-in at
    /// `provider` approves the `code` as, and answers the confirmation page:
    /// it names the code and the account, and holds a one-time confirmation
    /// bound to the flow `cookie`, which it sets again for as long (when a
    /// `public_url` names its path). `Ok(Err(refusal))` when there is no
    /// account to approve as.
    pub(super) async fn quick_connect_verified(
        &self,
        provider: &OidcProvider,
        identity: VerifiedIdentity,
        code: QuickConnectCode,
        cookie: &Secret,
        public_url: Option<&str>,
    ) -> Result<Result<SsoReply, Refusal>, ServiceError> {
        // Before any account is looked up or created, as for a sign-in (B8).
        if let Err(refusal) = roles::check_required_role(provider, &identity) {
            return Ok(Err(refusal));
        }
        let user = match self.account(provider, &identity).await? {
            Ok(user) => user,
            Err(refusal) => return Ok(Err(refusal)),
        };
        tracing::info!(oidc_provider = %provider.key, "OIDC Quick Connect sign-in verified");
        let confirmation =
            self.issue_exchange_code(&user, identity, cookie, FlowPurpose::QuickConnect(code))?;
        let mut reply = page(
            200,
            "Sign in your app?",
            &format!(
                "<p>Sign in the device showing code <strong>{code}</strong> as \
                 <strong>{name}</strong>?</p>\n<p>Confirm only if you started Quick Connect on \
                 that device yourself and it shows this code. This page works for one \
                 minute.</p>\n<form method=\"post\" \
                 action=\"../../quickconnect\"><input type=\"hidden\" name=\"confirmation\" \
                 value=\"{confirmation}\"><button type=\"submit\">Sign in the \
                 device</button></form>",
                code = escape_html(code.as_str()),
                name = escape_html(&user.username),
                confirmation = escape_html(&confirmation),
            ),
        );
        reply.set_cookies = public_url
            .map(|url| flow_cookie(url, cookie.expose(), EXCHANGE_CODE_LIFETIME))
            .into_iter()
            .collect();
        reply.content_security_policy = Some(CONFIRMATION_POLICY.to_owned());
        Ok(Ok(reply))
    }

    /// [`SsoManager::confirm_oidc_quick_connect`](ferrofin_traits::sso::SsoManager::confirm_oidc_quick_connect).
    pub(super) async fn confirm_quick_connect(
        &self,
        confirmation: Option<&Secret>,
        flow_cookie: Option<&Secret>,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        let quick_connect = self.quick_connect_manager()?;
        let config = self.config().await?;
        let mut reply = self
            .approve(
                &config,
                quick_connect.as_ref(),
                confirmation,
                flow_cookie,
                client,
            )
            .await?;
        reply.set_cookies = cleared_flow_cookie(self.public_url(&config).ok().flatten().as_deref());
        Ok(reply)
    }

    /// Uses `confirmation` up and approves its code through `quick_connect`
    /// as its account, while its identity is still linked to it, from the
    /// browser at `client`: the page saying the app is signed in, or why
    /// not.
    async fn approve(
        &self,
        config: &SsoConfig,
        quick_connect: &dyn QuickConnect,
        confirmation: Option<&Secret>,
        flow_cookie: Option<&Secret>,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        let redeemed = confirmation
            .ok_or("missing_confirmation")
            .and_then(|confirmation| self.redeem(confirmation, flow_cookie));
        let pending = match redeemed {
            Ok(pending) => pending,
            Err(reason) => return Ok(failed(None, reason)),
        };
        let key = pending.identity.provider_key.as_str();
        // A browser sign-in's code is no confirmation.
        let FlowPurpose::QuickConnect(code) = pending.purpose else {
            return Ok(failed(Some(key), "wrong_purpose"));
        };
        if !quick_connect.is_enabled().await? {
            log_refusal(key, "quick_connect_off");
            return Ok(turned_off(HOME));
        }
        // Disabled or removed since the callback.
        let Ok(provider) = sign_in_provider(config, key) else {
            return Ok(failed(Some(key), "provider_unavailable"));
        };
        let Some(user) = self.users.get_user_by_id(pending.user_id).await? else {
            return Ok(failed(Some(key), "no_account"));
        };
        // Unlinked or linked elsewhere since the callback (B12), as at a
        // browser exchange: held until the code is approved, so a removal,
        // which unlinks under this lock and then signs the account out,
        // either signs the app out or refuses it.
        let _linked = self.provisioning.lock().await;
        if !self
            .still_linked(&pending.identity, pending.user_id)
            .await?
        {
            return Ok(failed(Some(key), "unlinked"));
        }
        // Approving a code signs the account in on the app's device, so it
        // gets what a browser sign-in applies: role sync first, as
        // `open_session` runs it, so no route skips an administrator flag or
        // access the identity provider has taken away.
        self.sync_roles(&provider, &pending.identity, pending.user_id)
            .await?;
        // Disabled, remote access, parental schedule, for the approving
        // browser, as for a browser sign-in: Quick Connect does not expose
        // the app's address, and upstream's approval judges none. Logged by
        // the user manager.
        let remote_endpoint = client.to_string();
        if let Err(error) = self
            .users
            .ensure_sign_in_allowed(&user, &remote_endpoint)
            .await
        {
            return self.rule_refused(&user, &remote_endpoint, error).await;
        }
        match quick_connect
            .authorize_request(pending.user_id, code.as_str())
            .await
        {
            Ok(_) => {
                tracing::info!(oidc_provider = %key, user_id = %user.id, "OIDC Quick Connect approval completed");
                // Not awaited: the picture never holds the sign-in (B16).
                self.start_avatar_sync(&provider, &pending.identity, pending.user_id);
                Ok(at_home(
                    200,
                    "Your app is signed in",
                    "<p>Your app is signed in. You can close this page.</p>",
                ))
            }
            Err(ServiceError::NotFound(_)) => {
                log_refusal(key, "quick_connect_unknown_code");
                Ok(at_home(400, "Approval failed", UNKNOWN_CODE))
            }
            Err(ServiceError::InvalidInput(_)) => {
                log_refusal(key, "quick_connect_approved_already");
                Ok(at_home(400, "Approval failed", APPROVED_ALREADY))
            }
            // A device the account may not use, or its session limit: logged
            // by the session manager.
            Err(error @ ServiceError::Forbidden(_)) => {
                self.rule_refused(&user, &remote_endpoint, error).await
            }
            // Deleted since the lookup, or Quick Connect turned off since the
            // check above.
            Err(ServiceError::Unauthorized(_)) => Ok(failed(Some(key), "quick_connect_refused")),
            Err(error) => Err(error),
        }
    }

    /// The page of `user` refused by a sign-in rule (`error`, a
    /// [`ServiceError::Forbidden`]; any other is passed on), which writes the
    /// `AuthenticationFailed` activity entry a refused password sign-in does.
    async fn rule_refused(
        &self,
        user: &UserEntity,
        remote_endpoint: &str,
        error: ServiceError,
    ) -> Result<SsoReply, ServiceError> {
        let refusal = self.account_refused(user, remote_endpoint, error).await?;
        Ok(at_home(
            refusal.status,
            "Approval refused",
            &format!("<p>{}</p>", escape_html(&refusal.message)),
        ))
    }
}

#[cfg(test)]
mod tests;
