//! Linking a Ferrofin account and an OIDC identity (B12): a signed-in user
//! links their own account at a provider on the link page, `GET /sso/link`,
//! and removes the link there; an admin lists every link on the settings
//! page, adds one by hand and removes any.
//!
//! - A link flow is a sign-in flow whose purpose is
//!   [`FlowPurpose::Link`](super::flows::FlowPurpose::Link),
//!   naming the account signed in when it starts. `POST
//!   /sso/oidc/{provider}/link` starts it, as an authenticated request because
//!   a top-level navigation cannot carry the access token, and sets the flow
//!   cookie in the link page's browser. Its callback passes every check of a
//!   sign-in's (`verify_callback`), so a forged or replayed one, or one without
//!   that cookie, links nothing; `SsoService::link_verified` then links the
//!   verified identity to the flow's account instead of looking one up, and
//!   answers its own page, with no exchange code and no session.
//! - An identity without the provider's required role is refused, as at a
//!   sign-in (B8).
//! - An identity another account has is refused, and nothing moves. A
//!   second identity at the same issuer replaces the account's earlier one
//!   (the store's rule).
//! - Linking never changes the account's authentication provider or its
//!   password (D5).
//! - Removing a link signs every session of its account out, and ends every
//!   link flow the account started and has not called back. An account whose
//!   provider is Single Sign-On's cannot sign in without a link, which both
//!   pages say before the removal.
//!
//! The account a user's route acts for comes from the authenticated request,
//! never from a parameter: only the admin routes name one.

use std::collections::HashMap;
use std::sync::LazyLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrofin_model::sso::{OidcAdminLink, OidcLink, OidcLinkProvider, OidcProvider, OidcUserLinks};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentity, ExternalIdentityKey};
use ferrofin_traits::sso::{OidcLinkAt, SsoManager as _, SsoReply};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::accounts::PROVIDER_ID;
use super::roles::check_required_role;
use super::sign_in::{VerifiedIdentity, escape_html, page};
use super::{SsoConfig, SsoService};

/// The link page's script.
const LINK_SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/sso/link.js"));

/// The link page's `Content-Security-Policy`: its own script, by its hash,
/// and requests to this server only; nothing else is loaded, no form posted,
/// and the page is never framed.
static LINK_POLICY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "default-src 'none'; script-src 'sha256-{}'; connect-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'",
        STANDARD.encode(Sha256::digest(LINK_SCRIPT))
    )
});

/// The longest subject an admin can link: OpenID Connect Core 1.0, section 2,
/// "It MUST NOT exceed 255 ASCII characters in length".
const MAX_SUBJECT_LEN: usize = 255;

/// What the link page says before a removal that leaves an account of the
/// Single Sign-On provider without a link (the settings page says it too).
const LAST_LINK_WARNING: &str = "An account that signs in through Single Sign-On only cannot \
     sign in at all once its last link is removed, until an administrator links it again or \
     gives it another authentication provider on its profile page.";

/// Why an identity was not linked to an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotLinked {
    /// Another account has the identity.
    Taken,
    /// The account no longer exists.
    NoAccount,
}

/// The link page: the script finds jellyfin-web's session in the browser.
fn link_page() -> SsoReply {
    SsoReply {
        status: 200,
        location: None,
        html: format!(
            "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>Linked sign-ins</title></head>\n<body><main><h1>Linked sign-ins</h1>\n\
             <p id=\"status\">Loading…</p>\n\
             <noscript><p>This page needs JavaScript.</p></noscript>\n\
             <div id=\"links\" hidden>\n\
             <p>A sign-in at an identity provider that is linked to your Ferrofin account signs \
             in to that account. Linking does not change your password.</p>\n\
             <h2>Your linked sign-ins</h2>\n<p id=\"none\">None yet.</p>\n<ul id=\"linked\"></ul>\n\
             <p>Removing a link signs out every session of your account, this one included.</p>\n\
             <p id=\"warning\" hidden>{warning}</p>\n\
             <h2>Link a sign-in</h2>\n<p id=\"unavailable\">No provider is enabled.</p>\n\
             <ul id=\"providers\"></ul>\n\
             <p>Linking at a provider you are already linked at replaces that link.</p>\n\
             </div>\n<p><a href=\"../web/\">Back to Ferrofin</a></p></main>\n\
             <script>{LINK_SCRIPT}</script></body></html>\n",
            warning = escape_html(LAST_LINK_WARNING),
        ),
        set_cookies: Vec::new(),
        content_security_policy: Some(LINK_POLICY.clone()),
    }
}

/// Every stored provider the routes accept, enabled or not: a link at a
/// disabled provider can still be listed and removed.
fn valid_providers(config: &SsoConfig) -> Vec<OidcProvider> {
    config
        .checked_providers()
        .into_iter()
        .filter_map(|(provider, refused)| refused.is_none().then_some(provider))
        .collect()
}

/// The provider `key` among `providers`.
fn named<'a>(providers: &'a [OidcProvider], key: &str) -> Result<&'a OidcProvider, ServiceError> {
    providers
        .iter()
        .find(|provider| provider.key == key)
        .ok_or_else(|| ServiceError::not_found("no such OIDC provider"))
}

/// The first of `providers` at `issuer`: the one a link there is shown under.
fn at_issuer<'a>(providers: &'a [OidcProvider], issuer: &str) -> Option<&'a OidcProvider> {
    providers.iter().find(|provider| provider.issuer == issuer)
}

/// Whether `user`'s authentication provider is Single Sign-On's.
fn single_sign_on_account(user: &ferrofin_db::entities::users::UserEntity) -> bool {
    user.authentication_provider_id == PROVIDER_ID
}

/// `link` as an admin sees it, with its account.
fn admin_link(
    link: ExternalIdentity,
    user_name: String,
    single_sign_on_account: bool,
    providers: &[OidcProvider],
) -> OidcAdminLink {
    let provider = at_issuer(providers, &link.issuer);
    OidcAdminLink {
        user_id: link.user_id,
        user_name,
        single_sign_on_account,
        provider: provider.map(|provider| provider.key.clone()),
        provider_name: provider.map(|provider| provider.name.clone()),
        issuer: link.issuer,
        subject: link.subject,
        date_created: link.date_created,
    }
}

/// Checks a subject an admin types: what an ID token's `sub` can be.
fn validate_subject(subject: &str) -> Result<(), ServiceError> {
    if subject.is_empty()
        || subject.chars().count() > MAX_SUBJECT_LEN
        || subject.chars().any(char::is_control)
        || subject.trim() != subject
    {
        return Err(ServiceError::invalid_input(format!(
            "the subject must be 1 to {MAX_SUBJECT_LEN} characters, without control characters, \
             and not start or end with a space"
        )));
    }
    Ok(())
}

impl SsoService {
    /// [`SsoManager::oidc_link_page`](ferrofin_traits::sso::SsoManager::oidc_link_page).
    pub(super) async fn link_page(&self) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        Ok(link_page())
    }

    /// [`SsoManager::oidc_links`](ferrofin_traits::sso::SsoManager::oidc_links).
    pub(super) async fn user_links(&self, user_id: Uuid) -> Result<OidcUserLinks, ServiceError> {
        self.ensure_enabled().await?;
        let user = self
            .users
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {user_id}")))?;
        let providers = valid_providers(&self.config().await?);
        let links = self
            .identities
            .list_for_user(PROVIDER_ID, user_id)
            .await?
            .into_iter()
            .map(|link| {
                let provider = at_issuer(&providers, &link.issuer);
                OidcLink {
                    provider: provider.map(|provider| provider.key.clone()),
                    provider_name: provider.map(|provider| provider.name.clone()),
                    issuer: link.issuer,
                    date_created: link.date_created,
                }
            })
            .collect();
        Ok(OidcUserLinks {
            single_sign_on_account: single_sign_on_account(&user),
            links,
            providers: providers
                .into_iter()
                .filter(|provider| provider.enabled)
                .map(|provider| OidcLinkProvider {
                    key: provider.key,
                    name: provider.name,
                })
                .collect(),
        })
    }

    /// The callback of a link flow whose checks all passed: links `identity`
    /// to the flow's account `user_id`, and answers the page saying so, or
    /// why not. No session is opened.
    pub(super) async fn link_verified(
        &self,
        provider: &OidcProvider,
        user_id: Uuid,
        identity: VerifiedIdentity,
    ) -> Result<SsoReply, ServiceError> {
        let key = provider.key.as_str();
        let refused = |reason: &'static str, status, advice: &str| {
            tracing::warn!(oidc_provider = %key, reason, "OIDC link refused");
            page(
                status,
                "Link refused",
                &format!("<p>{}</p>", escape_html(advice)),
            )
        };
        // The provider's required role applies to a link as to a sign-in
        // (B8): an identity without it links nothing and replaces nothing.
        if let Err(refusal) = check_required_role(provider, &identity) {
            return Ok(refused(
                refusal.reason,
                refusal.status,
                refusal.advice.unwrap_or("Linking failed. Try again."),
            ));
        }
        // The store never finds an empty subject: such a link would sign
        // nobody in.
        if identity.subject.is_empty() {
            return Ok(refused(
                "id_token_subject",
                400,
                "Linking failed. Try again.",
            ));
        }
        let identity = ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &identity.issuer,
            subject: &identity.subject,
        };
        Ok(match self.link_identity(user_id, identity).await? {
            Ok(_) => {
                tracing::info!(oidc_provider = %key, user_id = %user_id, "OIDC identity linked");
                page(
                    200,
                    "Sign-in linked",
                    &format!(
                        "<p>Your Ferrofin account is now linked to your sign-in at {}, which \
                         signs in to it from now on.</p>\n\
                         <p><a href=\"../../link\">Back to your linked sign-ins</a></p>",
                        escape_html(&provider.name)
                    ),
                )
            }
            Err(NotLinked::Taken) => refused(
                "linked_elsewhere",
                409,
                "This sign-in is already linked to another Ferrofin account. Ask an administrator \
                 to remove that link first.",
            ),
            // Deleted since the flow started.
            Err(NotLinked::NoAccount) => refused("no_account", 400, "Linking failed. Try again."),
        })
    }

    /// Whether `identity` is still linked to `user_id`: a sign-in verified
    /// before its link was removed, or moved, signs nothing in. Callers hold
    /// the provisioning lock from this check until their session is minted.
    pub(super) async fn still_linked(
        &self,
        identity: &VerifiedIdentity,
        user_id: Uuid,
    ) -> Result<bool, ServiceError> {
        let key = ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &identity.issuer,
            subject: &identity.subject,
        };
        Ok(self
            .identities
            .find(key)
            .await?
            .is_some_and(|link| link.user_id == user_id))
    }

    /// Links `identity` to `user_id`, unless another account has it: then
    /// nothing changes. A link it already has is kept as it is.
    async fn link_identity(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
    ) -> Result<Result<ExternalIdentity, NotLinked>, ServiceError> {
        // Held as provisioning holds it, so the lookup and the link are one
        // step on this server.
        // shortcut: two servers (pods in a rolling update) linking one identity
        // to two accounts at the same instant can both pass `find`, and the
        // store's `link` then moves it to the second; only the person who
        // signed in as that identity can start either. Upgrade to a store
        // method that links unless taken, in one transaction, if that matters.
        let _linking = self.provisioning.lock().await;
        match self.identities.find(identity).await? {
            Some(link) if link.user_id == user_id => return Ok(Ok(link)),
            Some(_) => return Ok(Err(NotLinked::Taken)),
            None => {}
        }
        match self.identities.link(user_id, identity, None).await {
            Ok(link) => Ok(Ok(link)),
            Err(ServiceError::NotFound(_)) => Ok(Err(NotLinked::NoAccount)),
            Err(error) => Err(error),
        }
    }

    /// [`SsoManager::all_oidc_links`](ferrofin_traits::sso::SsoManager::all_oidc_links).
    pub(super) async fn all_links(&self) -> Result<Vec<OidcAdminLink>, ServiceError> {
        self.ensure_enabled().await?;
        let providers = valid_providers(&self.config().await?);
        let users: HashMap<Uuid, (String, bool)> = self
            .users
            .get_users()
            .await?
            .into_iter()
            .filter_map(|user| {
                let id = Uuid::parse_str(&user.id).ok()?;
                let sso_only = single_sign_on_account(&user);
                Some((id, (user.username, sso_only)))
            })
            .collect();
        Ok(self
            .identities
            .list(PROVIDER_ID)
            .await?
            .into_iter()
            // The table's foreign key removes a deleted account's links.
            .filter_map(|link| {
                let (name, sso_only) = users.get(&link.user_id)?.clone();
                Some(admin_link(link, name, sso_only, &providers))
            })
            .collect())
    }

    /// [`SsoManager::add_oidc_link`](ferrofin_traits::sso::SsoManager::add_oidc_link).
    pub(super) async fn add_link(
        &self,
        user_id: Uuid,
        key: &str,
        subject: &str,
    ) -> Result<OidcAdminLink, ServiceError> {
        self.ensure_enabled().await?;
        if user_id.is_nil() {
            return Err(ServiceError::invalid_input("the account is required"));
        }
        let providers = valid_providers(&self.config().await?);
        let issuer = named(&providers, key)?.issuer.clone();
        validate_subject(subject)?;
        let user = self
            .users
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {user_id}")))?;
        let identity = ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &issuer,
            subject,
        };
        let link = match self.link_identity(user_id, identity).await? {
            Ok(link) => link,
            Err(NotLinked::Taken) => {
                return Err(ServiceError::Conflict(
                    "another account is linked to this identity: remove that link first".to_owned(),
                ));
            }
            Err(NotLinked::NoAccount) => {
                return Err(ServiceError::not_found(format!("user {user_id}")));
            }
        };
        tracing::info!(oidc_provider = %key, user_id = %user_id, "OIDC identity linked by an administrator");
        let sso_only = single_sign_on_account(&user);
        Ok(admin_link(link, user.username, sso_only, &providers))
    }

    /// [`SsoManager::remove_oidc_link`](ferrofin_traits::sso::SsoManager::remove_oidc_link).
    pub(super) async fn remove_link(
        &self,
        user_id: Uuid,
        at: OidcLinkAt<'_>,
    ) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        let providers = valid_providers(&self.config().await?);
        let issuer = match at {
            OidcLinkAt::Provider(key) => named(&providers, key)?.issuer.as_str(),
            OidcLinkAt::Issuer(issuer) => issuer,
        };
        let removed = {
            let _linking = self.provisioning.lock().await;
            let link = self
                .identities
                .list_for_user(PROVIDER_ID, user_id)
                .await?
                .into_iter()
                .find(|link| link.issuer == issuer)
                .ok_or_else(|| ServiceError::not_found("no such OIDC link"))?;
            self.identities.unlink(link.key()).await?
        };
        if !removed {
            return Err(ServiceError::not_found("no such OIDC link"));
        }
        // The identity no longer signs in to the account: neither does any
        // session it, or anyone, opened (B12). The link is removed first,
        // under the lock an exchange or a Quick Connect approval holds from
        // its lookup of the link to its session: a sign-in it finished whose
        // code or confirmation is not used yet is refused there, and one
        // already minted is signed out here. A failed
        // sign-out is an error for a removal that stands; the dashboard's
        // Devices page signs the sessions out too.
        let revoked = self.sessions.revoke_user_tokens(user_id, "").await;
        // After the sign-out, so no session of the account is left to start
        // another: a link flow started from one would otherwise link
        // whoever signs in at its callback.
        // shortcut: a callback whose flow was already taken still links, the
        // drop reaches this process's flows only (another pod's live out their
        // lifetime), and a sign-out by another route (the Devices page, a
        // password change) ends no link flow; bind a link flow to its session
        // if that matters.
        self.flows.drop_link_flows_of(user_id);
        revoked?;
        tracing::info!(user_id = %user_id, "OIDC identity unlinked and the account's sessions signed out");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
