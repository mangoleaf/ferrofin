//! Linking a Ferrofin account and an OIDC identity (B12): a signed-in user
//! links their own account at a provider on the link page, `GET /sso/link`,
//! and removes the link there; an admin lists every link on the settings
//! page, adds one by hand and removes any.
//!
//! - A link flow is a sign-in flow whose purpose is
//!   [`FlowPurpose::Link`](super::flows::FlowPurpose::Link),
//!   naming the account signed in when it starts and the session it started
//!   from. `POST /sso/oidc/{provider}/link` starts it, as an authenticated
//!   request because a top-level navigation cannot carry the access token,
//!   and `SsoService::start_sign_in` sets the flow cookie on its answer for
//!   the link page's browser. Its callback passes every check of a sign-in's
//!   (`verify_callback`), so a forged or replayed one, or one without that
//!   cookie, links nothing; `SsoService::link_verified` then links the
//!   verified identity to the flow's account instead of looking one up, and
//!   answers its own page, with no exchange code and no session.
//! - Linking is a credential change, so it needs more than a session (D22):
//!   the start asks for the account's current password and checks it as a
//!   sign-in does (counted, and paced by the login backoff). An account whose
//!   provider is Single Sign-On's has no password of its own and cannot link
//!   itself: an administrator links it.
//! - A self-service link never replaces one: an account linked at the
//!   provider's issuer is refused at the start, and again at the callback
//!   under the lock every link write holds, so of two flows only one links.
//!   The admin's link still replaces the account's link at that issuer (the
//!   store's rule).
//! - A link flow ends with the session that started it: the callback links
//!   only while that session's token is still one of the account's devices,
//!   so a sign-out by any route before the callback's session check ends it
//!   (one inside the callback is a known window: see `link_identity`).
//! - An identity without the provider's required role is refused, as at a
//!   sign-in (B8). An identity another account has is refused, and nothing
//!   moves.
//! - Linking never changes the account's authentication provider or its
//!   password (D5).
//! - Removing a link signs every session of its account out, and ends every
//!   link flow the account started and has not called back. An account whose
//!   provider is Single Sign-On's cannot sign in without a link, which both
//!   pages say before the removal.
//! - Each link made and removed, by the user or an admin, writes a
//!   `UserIdentityLinked` or `UserIdentityUnlinked` activity entry.
//!
//! The account a user's route acts for comes from the authenticated request,
//! never from a parameter: only the admin routes name one.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrofin_common::cryptography::constant_time_eq;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::activity::LogLevel;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcAdminLink, OidcLink, OidcLinkProvider, OidcProvider, OidcUserLinks};
use ferrofin_traits::activity::ActivityLogCreate;
use ferrofin_traits::devices::DeviceQuery;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentity, ExternalIdentityKey};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::sso::{OidcLinkAt, SsoManager as _, SsoReply};
use sha2::{Digest, Sha256};
use tracing::Instrument as _;
use uuid::Uuid;

use super::accounts::PROVIDER_ID;
use super::flows::FlowPurpose;
use super::roles::check_required_role;
use super::sign_in::{VerifiedIdentity, escape_html, page, sign_in_provider};
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

/// Why an account Single Sign-On's provider owns cannot start a link: it has
/// no password to confirm one with (D22). The link page says it too.
const SINGLE_SIGN_ON_ACCOUNT_REFUSAL: &str = "Your account signs in through Single Sign-On \
     only, so it has no password to confirm a link with: an administrator adds further links on \
     the Single Sign-On settings page.";

/// A start whose password does not sign the account in, as `POST
/// /Users/Password` answers a wrong current password.
const WRONG_PASSWORD: &str = "invalid user or password entered";

/// A start with no password: nothing is checked or counted.
const NO_PASSWORD: &str = "the account's current password is required to link it";

/// A start for an account linked at the provider's issuer already.
const ALREADY_LINKED: &str = "your account is already linked at this provider: remove it first \
     to link another sign-in there";

/// The activity entry type of a link added.
const LINKED_ENTRY: &str = "UserIdentityLinked";

/// The activity entry type of a link removed.
const UNLINKED_ENTRY: &str = "UserIdentityUnlinked";

/// Why an identity was not linked to an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotLinked {
    /// Another account has the identity.
    Taken,
    /// The account no longer exists.
    NoAccount,
    /// The account has a link at the issuer, which a self-service link never
    /// replaces.
    AlreadyLinked,
    /// The session that started the link flow has ended.
    SessionEnded,
}

/// Whose link write it is, and what it may do.
#[derive(Clone, Copy)]
enum Linking<'a> {
    /// A link flow's callback: it links only while the session whose token
    /// hashes to this is the account's, and never replaces a link.
    Flow(&'a [u8; 32]),
    /// An administrator: it replaces the account's link at the issuer.
    Admin,
}

/// What a link write changed.
enum Change {
    /// Nothing: the account had the identity already.
    Kept,
    /// The link is new.
    Added,
    /// The link replaced the account's earlier one at the issuer (the
    /// admin's route only).
    Replaced(ExternalIdentity),
}

/// What a link flow keeps of the session that started it: the SHA-256 of its
/// access token, which the request authenticated with as stored.
fn session_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
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
             <h2>Link a sign-in</h2>\n<p id=\"sso-only\" hidden>{sso_only}</p>\n\
             <div id=\"linking\">\n<p id=\"unavailable\">No provider is enabled.</p>\n\
             <div id=\"password-entry\">\n\
             <p>Linking asks for the password you sign in to Ferrofin with. An account without \
             a password sets one on its profile first.</p>\n\
             <p><label for=\"password\">Your password</label> \
             <input id=\"password\" type=\"password\" autocomplete=\"current-password\"></p>\n\
             </div>\n<ul id=\"providers\"></ul>\n\
             <p>Linking never replaces a link: to link another sign-in at a provider you are \
             linked at, remove that link first.</p>\n\
             </div>\n</div>\n<p><a href=\"../web/\">Back to Ferrofin</a></p></main>\n\
             <script>{LINK_SCRIPT}</script></body></html>\n",
            warning = escape_html(LAST_LINK_WARNING),
            sso_only = escape_html(SINGLE_SIGN_ON_ACCOUNT_REFUSAL),
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

/// Whether `user`'s authentication provider is Single Sign-On's, matched
/// as the user manager matches provider ids, ignoring ASCII case.
fn single_sign_on_account(user: &UserEntity) -> bool {
    user.authentication_provider_id
        .eq_ignore_ascii_case(PROVIDER_ID)
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

/// The name of the user `id` for a link's activity entry, where it is the
/// `role` ("account" or "administrator"); `None` when there is none, or when
/// the lookup fails, which degrades the entry to an id or a role.
async fn user_name(users: &dyn UserManager, id: Uuid, role: &'static str) -> Option<String> {
    match users.get_user_by_id(id).await {
        Ok(user) => user.map(|user| user.username),
        Err(error) => {
            tracing::warn!(%error, user_id = %id, role, "a link's activity entry could not read a user");
            None
        }
    }
}

/// Waits for the entries [`SsoService::spawn_link_entries`] started for the
/// account `user_id`; a task that panicked is logged, as LOGGING.md rule 7
/// asks of a spawned task.
async fn entries_written(written: tokio::task::JoinHandle<()>, user_id: Uuid) {
    if let Err(error) = written.await {
        tracing::error!(
            %error,
            task = "oidc_link_entries",
            user_id = %user_id,
            "the link's activity entries were not all written"
        );
    }
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
        let links: Vec<OidcLink> = self
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
        let providers = providers
            .into_iter()
            .filter(|provider| provider.enabled)
            .map(|provider| OidcLinkProvider {
                linked: links.iter().any(|link| link.issuer == provider.issuer),
                key: provider.key,
                name: provider.name,
            })
            .collect();
        Ok(OidcUserLinks {
            single_sign_on_account: single_sign_on_account(&user),
            links,
            providers,
        })
    }

    /// [`SsoManager::start_oidc_link`](ferrofin_traits::sso::SsoManager::start_oidc_link):
    /// every refusal comes before a flow exists, so none sets a cookie. The
    /// flow cookie is set where every flow's is, by
    /// [`start_sign_in`](Self::start_sign_in).
    pub(super) async fn start_link(
        &self,
        key: &str,
        user_id: Uuid,
        password: &Secret,
        session_token: &Secret,
        client: IpAddr,
    ) -> Result<SsoReply, ServiceError> {
        self.ensure_enabled().await?;
        let provider = sign_in_provider(&self.config().await?, key)?;
        let user = self
            .users
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {user_id}")))?;
        let refused = |reason: &'static str| {
            tracing::warn!(oidc_provider = %key, user_id = %user_id, reason, "OIDC link refused");
        };
        if single_sign_on_account(&user) {
            refused("single_sign_on_account");
            return Err(ServiceError::Forbidden(
                SINGLE_SIGN_ON_ACCOUNT_REFUSAL.to_owned(),
            ));
        }
        if self.linked_at(user_id, &provider.issuer).await? {
            refused("already_linked");
            return Err(ServiceError::Conflict(ALREADY_LINKED.to_owned()));
        }
        if password.expose().is_empty() {
            return Err(ServiceError::invalid_input(NO_PASSWORD));
        }
        // As `POST /Users/Password` checks a current password: through the
        // account's own provider, counted and paced by the login backoff as
        // a sign-in is (which logs the failure), and no sign-in is stamped.
        match self
            .users
            .authenticate_user(
                &user.username,
                password.expose(),
                &client.to_string(),
                false,
            )
            .await
        {
            Ok(Some(_)) => {}
            // A wrong password, the backoff's refusal and an account rule
            // all answer as a wrong password.
            Ok(None) | Err(ServiceError::Unauthorized(_) | ServiceError::Forbidden(_)) => {
                return Err(ServiceError::Forbidden(WRONG_PASSWORD.to_owned()));
            }
            Err(error) => return Err(error),
        }
        let session = session_hash(session_token.expose());
        self.start_sign_in(key, client, FlowPurpose::Link { user_id, session })
            .await
    }

    /// Whether `user_id` has a link at `issuer`.
    async fn linked_at(&self, user_id: Uuid, issuer: &str) -> Result<bool, ServiceError> {
        Ok(self
            .identities
            .list_for_user(PROVIDER_ID, user_id)
            .await?
            .iter()
            .any(|link| link.issuer == issuer))
    }

    /// Whether the session whose token hashes to `session` is still one of
    /// `user_id`'s: a sign-out by any route deletes its device's token (the
    /// user's, the Devices page's, a password change's, a link removal's).
    async fn session_live(&self, user_id: Uuid, session: &[u8; 32]) -> Result<bool, ServiceError> {
        let devices = self
            .devices
            .get_devices(&DeviceQuery {
                user_id: Some(user_id),
                ..DeviceQuery::default()
            })
            .await?;
        Ok(devices
            .items
            .iter()
            .any(|device| constant_time_eq(&session_hash(&device.access_token), session)))
    }

    /// The callback of a link flow whose checks all passed: links `identity`
    /// to the flow's account `user_id` while the flow's `session` is live,
    /// and answers the page saying so, or why not. No session is opened.
    pub(super) async fn link_verified(
        &self,
        provider: &OidcProvider,
        user_id: Uuid,
        session: &[u8; 32],
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
        Ok(
            match self
                .link_identity(user_id, identity, Linking::Flow(session))
                .await?
            {
                Ok((link, change)) => {
                    if matches!(change, Change::Added) {
                        tracing::info!(oidc_provider = %key, user_id = %user_id, "OIDC identity linked");
                        let written = self.spawn_link_entries(
                            user_id,
                            user_id,
                            &provider.name,
                            &[(&link, true)],
                        );
                        entries_written(written, user_id).await;
                    }
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
                // Linked at the issuer since the start, by another flow or an
                // administrator.
                Err(NotLinked::AlreadyLinked) => refused(
                    "already_linked",
                    409,
                    "Your Ferrofin account is already linked to a sign-in at this provider, and a \
                 link is never replaced here. Remove it first on your linked sign-ins page, then \
                 link this one.",
                ),
                // A link flow ends with its session (D22): one signed out since
                // the start links nothing, whoever signed in at the provider.
                Err(NotLinked::SessionEnded) => refused(
                    "session_ended",
                    400,
                    "The Ferrofin session that started this link has ended, so nothing was linked. \
                 Sign in to Ferrofin again, then start the link again.",
                ),
                // Deleted since the flow started.
                Err(NotLinked::NoAccount) => {
                    refused("no_account", 400, "Linking failed. Try again.")
                }
            },
        )
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

    /// Links `identity` to `user_id`, unless another account has it, or, for
    /// a link flow, its session has ended or the account has a link at the
    /// issuer already: then nothing changes. A link it already has is kept as
    /// it is.
    async fn link_identity(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        linking: Linking<'_>,
    ) -> Result<Result<(ExternalIdentity, Change), NotLinked>, ServiceError> {
        // Held as provisioning holds it, and as a removal holds it until its
        // sign-out, so the checks and the link are one step on this server: of
        // two flows of one account one links, and none links once a removal
        // has signed its session out.
        // shortcut: two servers (pods in a rolling update) linking at the same
        // instant can both pass the checks: one identity can move to the second
        // account, and two flows of one account can both link, the second
        // replacing the first (the store's rule). Each needs a flow started
        // with the account's password, or an administrator's link, in the same
        // moment. A sign-out by a route that takes no such lock (the user's,
        // the Devices page, a password change) can also land between the
        // session check and the write, a few awaits apart, and the link then
        // stands. The upgrade for both is a store method that links unless the
        // identity is taken, the account is linked at the issuer or the
        // session's device row is gone, in one transaction.
        let _linking = self.provisioning.lock().await;
        // Before the write, so a check that fails, or a request dropped
        // midway, leaves nothing written.
        if let Linking::Flow(session) = linking
            && !self.session_live(user_id, session).await?
        {
            return Ok(Err(NotLinked::SessionEnded));
        }
        match self.identities.find(identity).await? {
            Some(link) if link.user_id == user_id => return Ok(Ok((link, Change::Kept))),
            Some(_) => return Ok(Err(NotLinked::Taken)),
            None => {}
        }
        let earlier = self
            .identities
            .list_for_user(PROVIDER_ID, user_id)
            .await?
            .into_iter()
            .find(|link| link.issuer == identity.issuer);
        if earlier.is_some() && matches!(linking, Linking::Flow(_)) {
            return Ok(Err(NotLinked::AlreadyLinked));
        }
        match self.identities.link(user_id, identity, None).await {
            Ok(link) => Ok(Ok((link, earlier.map_or(Change::Added, Change::Replaced)))),
            Err(ServiceError::NotFound(_)) => Ok(Err(NotLinked::NoAccount)),
            Err(error) => Err(error),
        }
    }

    /// Starts writing the activity entries of a change to the links of the
    /// account `user_id` at the provider `provider` (its name), made by `by`:
    /// the account itself, an administrator, or nil for an API key. Each of
    /// `changes` is a link and whether it was added ([`LINKED_ENTRY`]) or
    /// removed ([`UNLINKED_ENTRY`]), written in order; the subject is in the
    /// overview only. A task of its own writes them, started before any
    /// await of the caller's, so a request dropped once the change is made
    /// still leaves them; the caller awaits it last ([`entries_written`]).
    /// Best-effort, as the sign-in's entries: the log never fails a link.
    // shortcut: a request dropped during the change's own commit (one await)
    // can leave the change without its entries; the upgrade is to run the
    // change itself in the task.
    fn spawn_link_entries(
        &self,
        user_id: Uuid,
        by: Uuid,
        provider: &str,
        changes: &[(&ExternalIdentity, bool)],
    ) -> tokio::task::JoinHandle<()> {
        let (users, activity) = (Arc::clone(&self.users), Arc::clone(&self.activity));
        let provider = provider.to_owned();
        let changes: Vec<(String, bool)> = changes
            .iter()
            .map(|(link, linked)| {
                (
                    format!("Issuer: {}\nSubject: {}", link.issuer, link.subject),
                    *linked,
                )
            })
            .collect();
        let write = async move {
            let account = user_name(users.as_ref(), user_id, "account")
                .await
                .unwrap_or_else(|| user_id.to_string());
            let short_overview = if by == user_id {
                format!("By {account}")
            } else if by.is_nil() {
                "By an API key".to_owned()
            } else {
                match user_name(users.as_ref(), by, "administrator").await {
                    Some(admin) => format!("By the administrator {admin}"),
                    None => "By an administrator".to_owned(),
                }
            };
            for (overview, linked) in changes {
                let (kind, name) = if linked {
                    (
                        LINKED_ENTRY,
                        format!("A sign-in at {provider} was linked to {account}"),
                    )
                } else {
                    (
                        UNLINKED_ENTRY,
                        format!("A sign-in at {provider} was unlinked from {account}"),
                    )
                };
                let entry = ActivityLogCreate {
                    name,
                    type_: kind.to_owned(),
                    user_id: Some(user_id),
                    overview: Some(overview),
                    short_overview: Some(short_overview.clone()),
                    severity: LogLevel::Information,
                    ..ActivityLogCreate::default()
                };
                if let Err(error) = activity.create_entry(entry).await {
                    tracing::warn!(%error, activity_type = kind, "could not write the activity entry");
                }
            }
        };
        tokio::spawn(write.instrument(tracing::Span::current()))
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
        by: Uuid,
        user_id: Uuid,
        key: &str,
        subject: &str,
    ) -> Result<OidcAdminLink, ServiceError> {
        self.ensure_enabled().await?;
        if user_id.is_nil() {
            return Err(ServiceError::invalid_input("the account is required"));
        }
        let providers = valid_providers(&self.config().await?);
        let provider = named(&providers, key)?;
        let (issuer, provider_name) = (provider.issuer.clone(), provider.name.clone());
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
        // The admin's route replaces the account's link at the issuer.
        let link = match self
            .link_identity(user_id, identity, Linking::Admin)
            .await?
        {
            Ok((link, change)) => {
                // One task for both entries of a replacement, in order.
                let changes: &[(&ExternalIdentity, bool)] = match &change {
                    Change::Kept => &[],
                    Change::Added => &[(&link, true)],
                    Change::Replaced(earlier) => &[(earlier, false), (&link, true)],
                };
                if !changes.is_empty() {
                    let written = self.spawn_link_entries(user_id, by, &provider_name, changes);
                    entries_written(written, user_id).await;
                }
                link
            }
            Err(NotLinked::Taken) => {
                return Err(ServiceError::Conflict(
                    "another account is linked to this identity: remove that link first".to_owned(),
                ));
            }
            // `AlreadyLinked` and `SessionEnded` answer link flows only.
            Err(NotLinked::NoAccount | NotLinked::AlreadyLinked | NotLinked::SessionEnded) => {
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
        by: Uuid,
        user_id: Uuid,
        at: OidcLinkAt<'_>,
    ) -> Result<(), ServiceError> {
        self.ensure_enabled().await?;
        let providers = valid_providers(&self.config().await?);
        let issuer = match at {
            OidcLinkAt::Provider(key) => named(&providers, key)?.issuer.as_str(),
            OidcLinkAt::Issuer(issuer) => issuer,
        };
        let (revoked, written) = {
            let _linking = self.provisioning.lock().await;
            let link = self
                .identities
                .list_for_user(PROVIDER_ID, user_id)
                .await?
                .into_iter()
                .find(|link| link.issuer == issuer)
                .ok_or_else(|| ServiceError::not_found("no such OIDC link"))?;
            if !self.identities.unlink(link.key()).await? {
                return Err(ServiceError::not_found("no such OIDC link"));
            }
            // Started with no await between it and the unlink, nor between
            // the unlink and the sign-out below, and awaited after both.
            // shortcut: a request dropped during the unlink's own commit, or
            // part-way through the sign-out (one device at a time), leaves
            // some of the account's sessions signed in, and a retry finds no
            // link to remove; the upgrade is to run the unlink, the sign-out
            // and the entry in one task of their own.
            let provider = at_issuer(&providers, issuer).map_or(issuer, |provider| &provider.name);
            let written = self.spawn_link_entries(user_id, by, provider, &[(&link, false)]);
            // The identity no longer signs in to the account: neither does
            // any session it, or anyone, opened (B12). The link is removed
            // first, under the lock an exchange or a Quick Connect approval
            // holds from its lookup of the link to its session: a sign-in it
            // finished whose code or confirmation is not used yet is refused
            // there, and one already minted is signed out here. The sign-out
            // is under the lock too, which a link flow's callback holds from
            // its session check to its link, so none links in between. A
            // failed sign-out is an error for a removal that stands; the
            // dashboard's Devices page signs the sessions out too.
            (self.sessions.revoke_user_tokens(user_id, "").await, written)
        };
        // After the sign-out, so no session of the account is left to start
        // another. Every link flow is tied to its session too, which the
        // sign-out ended, so this drop only frees the flows early.
        self.flows.drop_link_flows_of(user_id);
        entries_written(written, user_id).await;
        revoked?;
        tracing::info!(user_id = %user_id, "OIDC identity unlinked and the account's sessions signed out");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
