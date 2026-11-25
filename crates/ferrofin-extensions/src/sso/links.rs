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
//!   provider is Single Sign-On's has a password no one knows (D21) and
//!   cannot link itself: an administrator links it.
//! - A self-service link never replaces one: an account linked at the
//!   provider's issuer is refused at the start, and again by the callback's
//!   write, which checks it in its own transaction, so of two flows only one
//!   links, on whichever servers they finish. The admin's link still
//!   replaces the account's link at that issuer.
//! - A link flow ends with the session that started it: the callback's write
//!   links only while that session's token is still one of the account's
//!   devices, so a sign-out by any route before the write ends it.
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
//! - A link change runs with its entries, and a removal with its sign-out
//!   and its entry, in a task of its own that the request waits for, so a
//!   request dropped midway leaves neither half done.
//!
//! The account a user's route acts for comes from the authenticated request,
//! never from a parameter: only the admin routes name one.

use std::collections::HashMap;
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::activity::LogLevel;
use ferrofin_model::secret::Secret;
use ferrofin_model::sso::{OidcAdminLink, OidcLink, OidcLinkProvider, OidcProvider, OidcUserLinks};
use ferrofin_traits::activity::{ActivityLogCreate, ActivityManager};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{
    ConditionalLink, ExternalIdentity, ExternalIdentityKey, LinkBy,
};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::sso::{OidcLinkAt, SsoManager as _, SsoReply};
use futures_util::FutureExt as _;
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

/// Writes the activity entries of a change to the links of the account
/// `user_id` at the provider `provider` (its name), made by `by`: the account
/// itself, an administrator, or nil for an API key. Each of `changes` is a
/// link and whether it was added ([`LINKED_ENTRY`]) or removed
/// ([`UNLINKED_ENTRY`]), written in order; the subject is in the overview
/// only. Best-effort, as the sign-in's entries: the log never fails a link.
async fn write_link_entries(
    users: &dyn UserManager,
    activity: &dyn ActivityManager,
    user_id: Uuid,
    by: Uuid,
    provider: &str,
    changes: &[(&ExternalIdentity, bool)],
) {
    let account = user_name(users, user_id, "account")
        .await
        .unwrap_or_else(|| user_id.to_string());
    let short_overview = if by == user_id {
        format!("By {account}")
    } else if by.is_nil() {
        "By an API key".to_owned()
    } else {
        match user_name(users, by, "administrator").await {
            Some(admin) => format!("By the administrator {admin}"),
            None => "By an administrator".to_owned(),
        }
    };
    for (link, linked) in changes {
        let (kind, name) = if *linked {
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
            overview: Some(format!(
                "Issuer: {}\nSubject: {}",
                link.issuer, link.subject
            )),
            short_overview: Some(short_overview.clone()),
            severity: LogLevel::Information,
            ..ActivityLogCreate::default()
        };
        if let Err(error) = activity.create_entry(entry).await {
            tracing::warn!(%error, activity_type = kind, "could not write the activity entry");
        }
    }
}

/// Runs `work`, the `task` changing the links of the account `user_id` with
/// what goes with it, in a task of its own, and waits for it: a request
/// dropped midway leaves the task to finish. A failure reaches the request,
/// whose error boundary logs it; once the request is gone, the task logs it
/// itself (LOGGING.md, rules 2 and 7), but not a link that was not there
/// to remove. A panic (which the panic hook logs too) is such a failure.
async fn in_own_task<T: Send + 'static>(
    task: &'static str,
    user_id: Uuid,
    work: impl Future<Output = Result<T, ServiceError>> + Send + 'static,
) -> Result<T, ServiceError> {
    let (answer, answered) = tokio::sync::oneshot::channel();
    let run = async move {
        let outcome = AssertUnwindSafe(work)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err(ServiceError::backend("the link change panicked")));
        if let Err(Err(error)) = answer.send(outcome)
            && !matches!(error, ServiceError::NotFound(_))
        {
            tracing::error!(%error, task, user_id = %user_id, "a link change failed after its request was dropped");
        }
    };
    tokio::spawn(run.instrument(tracing::Span::current()));
    answered
        .await
        .unwrap_or_else(|_| Err(ServiceError::backend("the link change did not finish")))
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
        let linked = self
            .link_identity(
                user_id,
                identity,
                LinkBy::Session(*session),
                user_id,
                &provider.name,
            )
            .await;
        Ok(match linked {
            Ok(outcome @ (ConditionalLink::Linked { .. } | ConditionalLink::Kept(_))) => {
                if matches!(outcome, ConditionalLink::Linked { .. }) {
                    tracing::info!(oidc_provider = %key, user_id = %user_id, "OIDC identity linked");
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
            Ok(ConditionalLink::Taken) => refused(
                "linked_elsewhere",
                409,
                "This sign-in is already linked to another Ferrofin account. Ask an administrator \
                 to remove that link first.",
            ),
            // Linked at the issuer since the start, by another flow or an
            // administrator.
            Ok(ConditionalLink::AlreadyLinked) => refused(
                "already_linked",
                409,
                "Your Ferrofin account is already linked to a sign-in at this provider, and a \
                 link is never replaced here. Remove it first on your linked sign-ins page, then \
                 link this one.",
            ),
            // A link flow ends with its session (D22): one signed out since
            // the start links nothing, whoever signed in at the provider.
            Ok(ConditionalLink::SessionEnded) => refused(
                "session_ended",
                400,
                "The Ferrofin session that started this link has ended, so nothing was linked. \
                 Sign in to Ferrofin again, then start the link again.",
            ),
            // Deleted since the flow started.
            Err(ServiceError::NotFound(_)) => {
                refused("no_account", 400, "Linking failed. Try again.")
            }
            Err(error) => return Err(error),
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

    /// Links `identity` to `user_id` as `by` may, and writes the change's
    /// activity entries, made by `actor` at the provider named `provider`, in
    /// a task of its own ([`in_own_task`]). The rules are checked by the
    /// store, inside its write
    /// ([`link_unless_taken`](ferrofin_traits::external_identities::ExternalIdentityStore::link_unless_taken)),
    /// so they hold whatever another server links or a sign-out ends
    /// meanwhile.
    async fn link_identity(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        by: LinkBy,
        actor: Uuid,
        provider: &str,
    ) -> Result<ConditionalLink, ServiceError> {
        let (identities, provisioning) =
            (Arc::clone(&self.identities), Arc::clone(&self.provisioning));
        let (users, activity) = (Arc::clone(&self.users), Arc::clone(&self.activity));
        let (provider_id, issuer, subject) = (
            identity.provider_id.to_owned(),
            identity.issuer.to_owned(),
            identity.subject.to_owned(),
        );
        let provider = provider.to_owned();
        in_own_task("oidc_link", user_id, async move {
            let identity = ExternalIdentityKey {
                provider_id: &provider_id,
                issuer: &issuer,
                subject: &subject,
            };
            let outcome = {
                // Held as provisioning holds it, and as a removal holds it
                // until its sign-out, so none links on this server between a
                // removal's unlink and its sign-out.
                let _linking = provisioning.lock().await;
                identities.link_unless_taken(user_id, identity, by).await?
            };
            if let ConditionalLink::Linked { link, replaced } = &outcome {
                let changes: Vec<(&ExternalIdentity, bool)> = replaced
                    .iter()
                    .map(|earlier| (earlier, false))
                    .chain([(link, true)])
                    .collect();
                write_link_entries(
                    users.as_ref(),
                    activity.as_ref(),
                    user_id,
                    actor,
                    &provider,
                    &changes,
                )
                .await;
            }
            Ok(outcome)
        })
        .await
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
            .link_identity(user_id, identity, LinkBy::Administrator, by, &provider_name)
            .await?
        {
            ConditionalLink::Linked { link, .. } | ConditionalLink::Kept(link) => link,
            ConditionalLink::Taken => {
                return Err(ServiceError::Conflict(
                    "another account is linked to this identity: remove that link first".to_owned(),
                ));
            }
            // A session's refusals, never an administrator's.
            ConditionalLink::AlreadyLinked | ConditionalLink::SessionEnded => {
                return Err(ServiceError::backend(
                    "the store refused an administrator's link as a session's",
                ));
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
        let provider = at_issuer(&providers, issuer).map_or(issuer, |provider| &provider.name);
        let (issuer, provider) = (issuer.to_owned(), provider.to_owned());
        let (identities, sessions) = (Arc::clone(&self.identities), Arc::clone(&self.sessions));
        let (provisioning, flows) = (Arc::clone(&self.provisioning), Arc::clone(&self.flows));
        let (users, activity) = (Arc::clone(&self.users), Arc::clone(&self.activity));
        in_own_task("oidc_unlink", user_id, async move {
            let revoked = {
                let _linking = provisioning.lock().await;
                let link = identities
                    .list_for_user(PROVIDER_ID, user_id)
                    .await?
                    .into_iter()
                    .find(|link| link.issuer == issuer)
                    .ok_or_else(|| ServiceError::not_found("no such OIDC link"))?;
                if !identities.unlink(link.key()).await? {
                    return Err(ServiceError::not_found("no such OIDC link"));
                }
                // The identity no longer signs in to the account: neither
                // does any session it, or anyone, opened (B12). The link is
                // removed first, under the lock an exchange or a Quick Connect
                // approval holds from its lookup of the link to its session: a
                // sign-in it finished whose code or confirmation is not used
                // yet is refused there, and one already minted is signed out
                // here. The sign-out is under the lock too, which a link
                // change holds for its write, so none links in between. A
                // failed sign-out is an error for a removal that stands; the
                // dashboard's Devices page signs the sessions out too.
                // shortcut: the lock is this server's: on another server
                // sharing the database (a rolling update's second pod), a link
                // flow called back between the unlink and the sign-out of its
                // session still links, its session then signed out, and an
                // exchange or a Quick Connect approval that looked the link up
                // before the unlink can mint its session after the sign-out.
                // The flow needed the account's password, which links again
                // after a removal anyway. The upgrade is a sign-out before the
                // unlink as well as after it, and a minted session signed out
                // again when its link is gone by then.
                // The entry is written beside the sign-out, which may fail or
                // take a while: the removal stands either way.
                let removed = [(&link, false)];
                let ((), revoked) = tokio::join!(
                    write_link_entries(
                        users.as_ref(),
                        activity.as_ref(),
                        user_id,
                        by,
                        &provider,
                        &removed,
                    ),
                    sessions.revoke_user_tokens(user_id, "")
                );
                revoked
            };
            // After the sign-out, so no session of the account is left to
            // start another. Every link flow is tied to its session too, which
            // the sign-out ended, so this drop only frees the flows early.
            flows.drop_link_flows_of(user_id);
            revoked?;
            tracing::info!(user_id = %user_id, "OIDC identity unlinked and the account's sessions signed out");
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests;
