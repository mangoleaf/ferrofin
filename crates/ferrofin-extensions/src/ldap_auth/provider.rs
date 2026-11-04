//! Sign-in through the directory: the authentication and password-reset
//! provider LDAP-Auth contributes, a port of
//! `LdapAuthenticationProviderPlugin.Authenticate`, `ChangePassword`,
//! `StartForgotPasswordProcess`, `LocateLdapUser`, `ConnectToLdap` and
//! `LdapUtils.SanitizeFilter` (jellyfin-plugin-ldapauth v24, `d11c86a`).
//!
//! A sign-in finds the user's directory entry as the bind DN, finds or
//! relinks the Ferrofin user it belongs to (by LDAP uid through the
//! external-identity store, then by name for a user LDAP-Auth already owns),
//! checks the password by binding as the entry, asks the admin filter whether
//! the user is an administrator, then creates the user or follows a directory
//! rename, giving a user it creates their directory picture
//! (`profile_image`, which also holds the `LdapProfileImageSync` task).
//! The administrator flag goes back to the user manager in
//! [`ProviderAuthenticationResult::administrator`], which applies it and never
//! demotes the last administrator (D5).
//!
//! Accepted divergences from the C# (see `docs/PLUGINS_UPSTREAM.md`):
//! - **An empty password is refused before any connection.** Novell's client
//!   turns it into an anonymous bind, which upstream then refuses; `ldap3`
//!   would send it, and a directory may accept an unauthenticated bind (RFC
//!   4513 section 5.1.2).
//! - **Only the directory refusing the user's own bind is a failed sign-in**
//!   (D4), with no matching entry and creation turned off: each is a `401`,
//!   which the user manager counts toward lockout when the name signed in
//!   with is an account's (creation turned off only arises for a name that is
//!   not). The bind counts as refused only for the result codes that refuse
//!   credentials ([`REFUSED_BIND`]). A directory that cannot be reached,
//!   times out, fails the bind with another code, refuses the configured bind
//!   DN, or fails the user search or the admin filter's (a referred server
//!   failing it, a referral loop and the referral budget included) is a
//!   server error, which the user manager does not count toward lockout.
//!   Upstream answers each with an `AuthenticationException`. A referral that
//!   leads to no server is skipped, so a user only it holds is no matching
//!   entry.
//! - **The last administrator is never demoted by the admin filter** (D5):
//!   the user manager keeps them one and logs a warning, and the sign-in
//!   succeeds. Upstream demotes them.
//! - **An entry without the username attribute fails with a `400` whenever
//!   the name is needed**: to apply the admin filter, or to create or rename
//!   the user. Upstream's `SanitizeFilter` throws on the null name, a `500`,
//!   while the admin filter is on.
//! - **A name that finds another account's entry is refused.** When the name
//!   signed in with is a Ferrofin user's, the entry must belong to that user.
//!   Upstream signs in the user the name named whichever entry the search
//!   returned first, so an entry another person can edit to match the name
//!   would let them sign in as that user. The entry's username attribute is
//!   still trusted, as upstream trusts it: an entry with no uid link yet whose
//!   username attribute names an account LDAP-Auth owns is relinked to it, so
//!   that attribute must be one users cannot edit: where users can write it,
//!   one with no account linked to their uid yet can take any LDAP-owned
//!   account, whatever its rights, with their own password. The uid
//!   attribute is trusted the same way: a user who can write their own can
//!   point it at a linked user's uid, and signs in to that account, renamed
//!   to their own username attribute. Kept as upstream by owner decision
//!   (gl-1188, 2026-11-03: to be revisited if it is proven a security issue).
//!   The username-attribute route was reproduced live on 2026-11-03, the
//!   directory administrator editing the entry standing in for a user; the
//!   uid route is read from the code. Whether that triggers the revisit is
//!   the owner's call.
//! - **A link is written only after the password is checked.** Upstream
//!   records a relink by name before it binds as the user.
//! - **A uid link to an account LDAP-Auth no longer owns is ignored**
//!   (gl-1230), as the lookup by name ignores such an account. Upstream
//!   follows the link whatever the account's `AuthenticationProviderId`, and
//!   the user manager then switches the account back to LDAP-Auth, undoing an
//!   administrator's move to another provider and its local password. The
//!   link is kept, so moving the account back to LDAP-Auth finds it again,
//!   unless a sign-in has meanwhile linked the entry to another account (one
//!   it created, or an LDAP-Auth account named as the entry's username
//!   attribute), which takes the link over.
//! - **An entry without the uid attribute is not linked.** Upstream stores a
//!   null `LdapUid`, which no lookup finds, and in doing so drops the user's
//!   earlier link and every other null-uid link. Such a user is found by name
//!   each time, and keeps the link of the uid they had.
//! - **A blank new password is refused** (D11), before any connection, with
//!   a `400`: a reset sends one. Upstream sends it to the directory, and RFC
//!   3062 then has the directory set a random password nobody learns.
//! - **A password change whose directory cannot be reached, or refuses the
//!   bind DN, is a `500`**, as at sign-in (D4). Upstream answers `401`.
//! - **No PIN is ever this provider's to redeem**: upstream throws
//!   `NotImplementedException`, which would stop the redemption before a
//!   later provider could redeem its own.
//! - **A password change writes only to the user's own entry.** The entry
//!   the user's name finds must belong to them (by its uid link, or by name),
//!   as at sign-in; otherwise the change is a `401` and nothing is written.
//!   Upstream writes to whichever entry matched first, so a person who can
//!   make their own entry match another user's name would have that user's
//!   new password set on their entry. The match trusts the username and uid
//!   attributes, as sign-in does, so where users can write them a new
//!   password can still reach the wrong entry (kept as upstream by owner
//!   decision, gl-1188).
//! - **A new user is created with its provider ids in one transaction**
//!   ([`UserManager::create_external_user`]). Upstream saves them after
//!   creating the user, a moment in which the account has no password and
//!   the default provider takes an empty one.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ferrofin_common::extensions::get_md5;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::users::{
    ForgotPasswordAction, ForgotPasswordResult, PinRedeemResult, UserPolicy,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::library::{ExternalUserSetup, UserManager};
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::remote_images::RemoteImageFetcher;
use ferrofin_traits::security::{
    AuthenticationManager, PasswordResetManager, ProviderAuthenticationResult,
};
use ferrofin_util::string_extensions::{lower_invariant, replace_ignore_ascii_case};
use uuid::Uuid;

use super::{
    BoundSearchError, ConnectionOptions, EXTENSION_ID, LdapConfig, LdapConnection, LdapDirectory,
    LdapEntry, LdapError, NO_ATTRIBUTES, PASSWORD_MODIFY_OID, config_error, novell_bind,
    search_as_bind_dn,
};

/// The id LDAP-Auth's users store as their `AuthenticationProviderId` and
/// `PasswordResetProviderId`: upstream's `GetType().FullName`, so the users
/// of an adopted Jellyfin select this provider.
pub const PROVIDER_ID: &str = "Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin";

/// The provider's display name, as `GET /Auth/Providers` lists it.
pub const PROVIDER_NAME: &str = "LDAP-Authentication";

/// The placeholder in `LdapSearchFilter` the sign-in name replaces.
const USERNAME_PLACEHOLDER: &str = "{username}";

/// Upstream's message for a wrong password, and for an empty one.
const INVALID_LOGIN: &str = "Error completing LDAP login. Invalid username or password.";

/// Upstream's message when the search finds no entry.
pub(crate) const NO_MATCH: &str = "Found no LDAP users matching provided username.";

/// The RFC 4511 result codes with which a directory refuses a bind's
/// credentials, rather than failing to serve it: `constraintViolation` (19, a
/// password policy's lockout), `inappropriateAuthentication` (48),
/// `invalidCredentials` (49), `insufficientAccessRights` (50) and
/// `unwillingToPerform` (53). Only these count as a failed sign-in (D4).
const REFUSED_BIND: [u32; 5] = [19, 48, 49, 50, 53];

/// `LdapUtils.SanitizeFilter`: escapes the characters RFC 4515 gives a
/// meaning in a search filter (`\`, `*`, `(`, `)` and NUL), byte for byte as
/// upstream does, so a sign-in name cannot change the filter.
#[must_use]
pub fn sanitize_filter(input: &str) -> String {
    let mut sanitized = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' => sanitized.push_str("\\5c"),
            '*' => sanitized.push_str("\\2a"),
            '(' => sanitized.push_str("\\28"),
            ')' => sanitized.push_str("\\29"),
            '\0' => sanitized.push_str("\\00"),
            other => sanitized.push(other),
        }
    }
    sanitized
}

/// The filter `LocateLdapUser` searches with for `username`, escaped with
/// [`sanitize_filter`]: `LdapSearchFilter` with every `{username}` replaced
/// (ignoring case), or, when it has none, `(&<filter>(|(<attr>=<name>)…))`
/// over `LdapSearchAttributes` with its spaces removed, split on commas.
#[must_use]
pub fn user_search_filter(config: &LdapConfig, username: &str) -> String {
    let name = sanitize_filter(username);
    let filter = &config.ldap_search_filter;
    if filter.to_ascii_lowercase().contains(USERNAME_PLACEHOLDER) {
        return replace_ignore_ascii_case(filter, USERNAME_PLACEHOLDER, &name);
    }
    let mut built = format!("(&{filter}(|");
    for attribute in config.ldap_search_attributes.replace(' ', "").split(',') {
        let _ = write!(built, "({attribute}={name})");
    }
    built.push_str("))");
    built
}

/// The filter the admin filter searches with at sign-in for the entry whose
/// username attribute is `ldap_username`: `LdapAdminFilter` with every
/// `{username}` replaced (ignoring case) by the name escaped with
/// [`sanitize_filter`], as upstream's `Authenticate` builds it.
#[must_use]
pub fn admin_search_filter(config: &LdapConfig, ldap_username: &str) -> String {
    replace_ignore_ascii_case(
        &config.ldap_admin_filter,
        USERNAME_PLACEHOLDER,
        &sanitize_filter(ldap_username),
    )
}

/// `LocateLdapUser`: searches the base DN's subtree as the configured bind DN
/// with [`user_search_filter`] for `[LdapUsernameAttribute, LdapUidAttribute]`,
/// and `LdapProfileImageAttribute` while profile-image sync is on, following
/// referrals, and returns the first entry, warning when there are more, or
/// `None` when none matches.
pub(crate) async fn locate_ldap_user(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    username: &str,
) -> Result<Option<LdapEntry>, BoundSearchError> {
    let filter = user_search_filter(config, username);
    tracing::debug!(base_dn = ?config.ldap_base_dn, filter = ?filter, "LDAP user search");
    let mut attributes = vec![
        config.ldap_username_attribute.as_str(),
        config.ldap_uid_attribute.as_str(),
    ];
    // An empty name names no attribute an entry could have, so it is not
    // sent: a server may refuse it.
    if config.enable_ldap_profile_image_sync && !config.ldap_profile_image_attribute.is_empty() {
        attributes.push(config.ldap_profile_image_attribute.as_str());
    }
    let mut found = search_as_bind_dn(
        directory,
        config,
        &config.ldap_base_dn,
        &filter,
        &attributes,
    )
    .await?;
    if found.is_empty() {
        tracing::debug!(username = ?username, "no LDAP entry matches the sign-in name");
        return Ok(None);
    }
    if found.len() > 1 {
        tracing::warn!(
            username = ?username,
            "more than one LDAP result matched; using first result only"
        );
    }
    let entry = found.swap_remove(0);
    tracing::debug!(username = ?username, dn = ?entry.dn, "found the LDAP entry");
    Ok(Some(entry))
}

/// The first value of `entry`'s attribute `name` as text (Novell's
/// `LdapAttribute.StringValue`), or `None` with upstream's warning when the
/// entry lacks it.
fn attribute_text(entry: &LdapEntry, name: &str) -> Option<String> {
    let value = entry
        .attribute(name)
        .and_then(|attribute| attribute.values.first());
    if value.is_none() {
        tracing::warn!(attribute = ?name, dn = ?entry.dn, "LDAP attribute not found for user");
    }
    value.map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// A directory failure that is not the directory refusing the user's
/// password: a server error, which the user manager does not count toward
/// the user's lockout (D4).
fn outage(step: &str, error: &LdapError) -> ServiceError {
    ServiceError::backend(format!("LDAP sign-in could not {step}: {error}"))
}

/// A directory failure during a password change: a server error, as at
/// sign-in (D4).
fn change_failure(step: &str, error: &LdapError) -> ServiceError {
    ServiceError::backend(format!("LDAP password change could not {step}: {error}"))
}

/// The error `failure` ([`outage`] or [`change_failure`]) makes for a search
/// as the configured bind DN that failed at the connection, the bind, or the
/// search, which the caller calls `search_step`.
fn search_failure(
    error: &BoundSearchError,
    search_step: &str,
    failure: fn(&str, &LdapError) -> ServiceError,
) -> ServiceError {
    match error {
        BoundSearchError::Connect(error) => failure("connect", error),
        BoundSearchError::Bind(error) => failure("bind as the configured bind DN", error),
        BoundSearchError::Search(error) => failure(search_step, error),
    }
}

/// The `401` of a password change whose name finds an entry that belongs to
/// another account.
const OTHER_ACCOUNTS_ENTRY: &str =
    "The LDAP entry this user's name finds belongs to another account; no password was changed.";

/// Upstream's `ChangePassword` sends a blank password as it is, and RFC 3062
/// then has the directory set a random one nobody learns (D11).
const BLANK_PASSWORD: &str =
    "An LDAP user's password cannot be set blank; the directory was not changed.";

/// The write half of `ChangePassword`, on `connection`: bound as the
/// configured bind DN, the entry `dn` gets `new_password` through the RFC 3062
/// password modify operation when the root DSE lists it, or else by replacing
/// `LdapPasswordAttribute`; with neither, upstream's `InvalidOperationException`
/// (a `500`).
async fn set_password(
    connection: &mut dyn LdapConnection,
    config: &LdapConfig,
    dn: &str,
    new_password: &str,
) -> Result<(), ServiceError> {
    novell_bind(
        connection,
        &config.ldap_bind_user,
        config.ldap_bind_password.expose(),
    )
    .await
    .map_err(|error| change_failure("bind as the configured bind DN", &error))?;
    let extensions = connection
        .supported_extensions()
        .await
        .map_err(|error| change_failure("read the root DSE", &error))?;
    let written = if extensions.iter().any(|oid| oid == PASSWORD_MODIFY_OID) {
        tracing::debug!(dn = ?dn, "setting the password with the password modify operation");
        connection.password_modify(dn, new_password).await
    } else if !config.ldap_password_attribute.is_empty() {
        tracing::debug!(
            dn = ?dn,
            attribute = ?config.ldap_password_attribute,
            "setting the password attribute"
        );
        connection
            .modify_replace(dn, &config.ldap_password_attribute, &[new_password])
            .await
    } else {
        return Err(ServiceError::backend("Password attribute is not set"));
    };
    written.map_err(|error| change_failure("set the password", &error))
}

/// `PasswordResetUrl` with `$userId` (the id in .NET's lowercase `Guid` form)
/// and `$userName` (the entered name lowercased as .NET's `ToLowerInvariant`
/// does) replaced, ignoring case, as upstream's `StartForgotPasswordProcess`
/// fills it. Both go in as they are, unencoded, as upstream puts them.
#[must_use]
pub fn password_reset_url(template: &str, user_id: Uuid, entered_username: &str) -> String {
    let url = replace_ignore_ascii_case(template, "$userId", &user_id.to_string());
    replace_ignore_ascii_case(&url, "$userName", &lower_invariant(entered_username))
}

/// The LDAP-Auth authentication provider. Reads the plugin's enabled flag
/// and configuration at each call, so enabling, disabling or reconfiguring
/// the plugin needs no restart.
pub struct LdapAuthProvider {
    plugins: Arc<dyn PluginManager>,
    users: Arc<dyn UserManager>,
    identities: Arc<dyn ExternalIdentityStore>,
    directory: Arc<dyn LdapDirectory>,
    /// Fetches a picture the directory gives as a URL.
    images: Arc<dyn RemoteImageFetcher>,
    /// Whether the configuration that cannot be read was already warned
    /// about, so a broken file warns once rather than at every sign-in.
    warned_unreadable: AtomicBool,
}

/// The Ferrofin user a directory entry belongs to.
struct FoundUser {
    user: UserEntity,
    /// Found by name rather than by uid, so the uid is to be linked.
    needs_link: bool,
}

impl LdapAuthProvider {
    /// A provider over the plugin manager (the enabled flag and the
    /// configuration), the user accounts it finds and creates, the store of
    /// LDAP uid links, the directory it connects through, and the fetcher of
    /// pictures the directory gives as URLs.
    #[must_use]
    pub fn new(
        plugins: Arc<dyn PluginManager>,
        users: Arc<dyn UserManager>,
        identities: Arc<dyn ExternalIdentityStore>,
        directory: Arc<dyn LdapDirectory>,
        images: Arc<dyn RemoteImageFetcher>,
    ) -> Self {
        Self {
            plugins,
            users,
            identities,
            directory,
            images,
            warned_unreadable: AtomicBool::new(false),
        }
    }

    /// The configuration, when the plugin is enabled and names a server
    /// (D3); `None` otherwise. A configuration that does not parse counts as
    /// none, as upstream starts the plugin from its defaults, which here name
    /// no server; it is warned about once until it parses again. A file the
    /// plugin manager cannot read at all is its error.
    async fn usable_config(&self) -> Result<Option<LdapConfig>, ServiceError> {
        if !self
            .plugins
            .get_plugin(EXTENSION_ID)
            .await?
            .is_some_and(|plugin| plugin.enabled)
        {
            return Ok(None);
        }
        let bytes = self.plugins.get_plugin_configuration(EXTENSION_ID).await?;
        let config: LdapConfig = match serde_json::from_slice(&bytes) {
            Ok(config) => {
                self.warned_unreadable.store(false, Ordering::Relaxed);
                config
            }
            Err(error) => {
                if self.warned_unreadable.swap(true, Ordering::Relaxed) {
                    tracing::debug!(
                        error = %config_error(&error),
                        "the LDAP-Auth configuration cannot be read"
                    );
                } else {
                    tracing::warn!(
                        error = %config_error(&error),
                        "the LDAP-Auth configuration cannot be read; sign-in through the \
                         directory is off until it is saved again"
                    );
                }
                return Ok(None);
            }
        };
        Ok((!config.ldap_server.trim().is_empty()).then_some(config))
    }

    /// The link key of an LDAP uid: this provider, no issuer, the uid.
    pub(super) fn identity(uid: &str) -> ExternalIdentityKey<'_> {
        ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: "",
            subject: uid,
        }
    }

    /// `LocateLdapUser` ([`locate_ldap_user`]) at sign-in: no matching entry
    /// is upstream's `401`, and a directory that fails is a server error.
    async fn locate_user(
        &self,
        config: &LdapConfig,
        username: &str,
    ) -> Result<LdapEntry, ServiceError> {
        locate_ldap_user(self.directory.as_ref(), config, username)
            .await
            .map_err(|error| search_failure(&error, "search the directory", outage))?
            .ok_or_else(|| ServiceError::unauthorized(NO_MATCH))
    }

    /// Whether the admin filter makes the user whose entry is `entry` an
    /// administrator: upstream's admin block in `Authenticate`, or `None`
    /// while the filter is off (see [`LdapConfig::admin_filter_enabled`]).
    ///
    /// It searches as the configured bind DN, on a connection of its own,
    /// [`LdapConfig::admin_base_dn`]'s subtree with [`admin_search_filter`],
    /// following referrals.
    /// With `EnableLdapAdminFilterMemberUid` any entry found makes the user an
    /// administrator; otherwise one of them must be the user's own entry (the
    /// DNs compared ordinally).
    async fn administrator(
        &self,
        config: &LdapConfig,
        entry: &LdapEntry,
        ldap_username: Option<&str>,
    ) -> Result<Option<bool>, ServiceError> {
        if !config.admin_filter_enabled() {
            return Ok(None);
        }
        // Upstream's `SanitizeFilter` throws on a null name: see the module's
        // divergences.
        let name = ldap_username.ok_or_else(|| no_username(config))?;
        let filter = admin_search_filter(config, name);
        tracing::debug!(base_dn = ?config.admin_base_dn(), filter = ?filter, "LDAP admin filter search");
        let found = search_as_bind_dn(
            self.directory.as_ref(),
            config,
            config.admin_base_dn(),
            &filter,
            &[NO_ATTRIBUTES],
        )
        .await
        .map_err(|error| search_failure(&error, "apply the admin filter", outage))?;
        let administrator = if config.enable_ldap_admin_filter_member_uid {
            !found.is_empty()
        } else {
            found.iter().any(|admin| admin.dn == entry.dn)
        };
        tracing::debug!(dn = ?entry.dn, administrator, "applied the LDAP admin filter");
        Ok(Some(administrator))
    }

    /// `ConnectToLdap(userDn, password)`: a connection of its own, bound as
    /// the entry with the password the user gave. Only the directory refusing
    /// that bind is an authentication failure.
    async fn bind_as_user(
        &self,
        config: &LdapConfig,
        dn: &str,
        password: &str,
    ) -> Result<(), ServiceError> {
        // Novell's bind makes an empty password anonymous, which succeeds, and
        // upstream refuses it as not `Bound`. `authenticate` refuses it before
        // connecting; this keeps the check where upstream has it.
        if password.is_empty() {
            return Err(ServiceError::unauthorized(INVALID_LOGIN));
        }
        let options = ConnectionOptions::from_config(config);
        let mut connection = self
            .directory
            .connect(&options)
            .await
            .map_err(|error| outage("connect", &error))?;
        let bound = novell_bind(connection.as_mut(), dn, password).await;
        connection.unbind().await;
        match bound {
            Ok(()) => Ok(()),
            Err(LdapError::Result { code, .. }) if REFUSED_BIND.contains(&code) => {
                tracing::debug!(dn = ?dn, code, "the directory refused the user's password");
                Err(ServiceError::unauthorized(INVALID_LOGIN))
            }
            Err(error) => Err(outage("check the password", &error)),
        }
    }

    /// Links `user_id` to the entry's uid (`PluginConfiguration.AddUser`),
    /// with no avatar hash until a picture is stored. An entry without the uid
    /// attribute is not linked: see the module's divergences.
    async fn link(&self, user_id: &str, uid: &str) -> Result<(), ServiceError> {
        if uid.is_empty() {
            return Ok(());
        }
        let user_id =
            Uuid::parse_str(user_id).map_err(|error| ServiceError::backend(error.to_string()))?;
        self.identities
            .link(user_id, Self::identity(uid), None)
            .await
            .map(|_| ())
    }

    /// The Ferrofin user the entry belongs to, upstream's two lookups: the one
    /// its uid is linked to, else the user named as the entry's username
    /// attribute, which then needs linking. Both take only a user LDAP-Auth
    /// owns (`AuthenticationProviderId`): a local account with that name is
    /// not taken, and a link to an account since moved to another provider is
    /// ignored but kept (see the module's divergences).
    async fn find_user(
        &self,
        uid: &str,
        ldap_username: Option<&str>,
    ) -> Result<Option<FoundUser>, ServiceError> {
        if let Some(link) = self.identities.find(Self::identity(uid)).await?
            && let Some(user) = self.users.get_user_by_id(link.user_id).await?
            && owned(&user)
        {
            return Ok(Some(FoundUser {
                user,
                needs_link: false,
            }));
        }
        let Some(name) = ldap_username else {
            return Ok(None);
        };
        Ok(self
            .users
            .get_user_by_name(name)
            .await?
            .filter(owned)
            .map(|user| FoundUser {
                user,
                needs_link: true,
            }))
    }
}

/// Whether LDAP-Auth owns `user`: their `AuthenticationProviderId` is its id,
/// ignoring case as upstream's `OrdinalIgnoreCase` compare does.
fn owned(user: &UserEntity) -> bool {
    user.authentication_provider_id
        .eq_ignore_ascii_case(PROVIDER_ID)
}

/// The error for a directory entry without the username attribute: upstream
/// creates or renames the user with a null name, which throws an
/// `ArgumentException`, a `400`.
fn no_username(config: &LdapConfig) -> ServiceError {
    ServiceError::invalid_input(format!(
        "the LDAP entry has no {} attribute to name the user by",
        config.ldap_username_attribute
    ))
}

#[async_trait]
impl AuthenticationManager for LdapAuthProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn id(&self) -> &str {
        PROVIDER_ID
    }

    /// Enabled while the plugin is and a server is configured (D3).
    /// Upstream's `IsEnabled` is always `true`.
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(self.usable_config().await?.is_some())
    }

    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        // Before any connection: see the module's divergences.
        if password.is_empty() {
            return Err(ServiceError::unauthorized(INVALID_LOGIN));
        }
        let Some(config) = self.usable_config().await? else {
            return Err(ServiceError::unauthorized(INVALID_LOGIN));
        };

        let entry = self.locate_user(&config, username).await?;
        let uid = attribute_text(&entry, &config.ldap_uid_attribute).unwrap_or_default();
        let ldap_username = attribute_text(&entry, &config.ldap_username_attribute);
        tracing::debug!(ldap_uid = ?uid, ldap_username = ?ldap_username, "read the LDAP entry");

        let found = self.find_user(&uid, ldap_username.as_deref()).await?;
        if let Some(resolved) = resolved_user
            && found
                .as_ref()
                .is_none_or(|found| found.user.id != resolved.id)
        {
            // The manager signs in `resolved` whatever this provider found.
            tracing::debug!(
                user_id = %resolved.id,
                dn = ?entry.dn,
                "the LDAP entry the name found belongs to another account"
            );
            return Err(ServiceError::unauthorized(INVALID_LOGIN));
        }

        self.bind_as_user(&config, &entry.dn, password).await?;
        let administrator = self
            .administrator(&config, &entry, ldap_username.as_deref())
            .await?;

        let Some(FoundUser { user, needs_link }) = found else {
            if !config.create_users_from_ldap {
                // The manager logs this as an unknown name; the directory
                // took the password, which an administrator needs to know.
                tracing::warn!(
                    ldap_username = ?ldap_username,
                    dn = ?entry.dn,
                    "the directory accepted the password, but automatic user creation is off \
                     and no user is linked to the entry"
                );
                return Err(ServiceError::unauthorized(format!(
                    "Automatic User Creation is disabled and there is no Jellyfin user for \
                     authorized Uid: {}",
                    ldap_username.unwrap_or_default()
                )));
            }
            let name = ldap_username.ok_or_else(|| no_username(&config))?;
            tracing::debug!(username = ?name, "creating the LDAP user");
            let setup = ExternalUserSetup {
                authentication_provider_id: PROVIDER_ID.to_owned(),
                password_reset_provider_id: PROVIDER_ID.to_owned(),
                enable_all_folders: config.enable_all_folders,
                enabled_folders: (!config.enable_all_folders)
                    .then(|| config.enabled_folders.clone()),
            };
            let created = self.users.create_external_user(&name, &setup).await?;
            // Linked before the picture, which may be a download, so a
            // concurrent sign-in finds the user by their uid meanwhile.
            self.link(&created.id, &uid).await?;
            if let Some(avatar_hash) = self.first_picture(&config, &entry, &created).await
                && !uid.is_empty()
                && let Err(error) = self
                    .identities
                    .set_avatar_hash(Self::identity(&uid), Some(&avatar_hash))
                    .await
            {
                // The account and its picture exist; the task stores the
                // picture again, with its hash, on its next run.
                tracing::warn!(
                    user_id = %created.id,
                    %error,
                    "the new LDAP user's picture hash cannot be recorded"
                );
            }
            return Ok(ProviderAuthenticationResult {
                username: name,
                // Upstream sets the flag on every user it creates, `false`
                // while the filter is off.
                administrator: Some(administrator.unwrap_or(false)),
                ..ProviderAuthenticationResult::default()
            });
        };

        if needs_link {
            // A relink by name records no picture, as `AddUser(…, string.Empty)`.
            self.link(&user.id, &uid).await?;
        }
        let name = ldap_username.ok_or_else(|| no_username(&config))?;
        if user.username != name {
            tracing::debug!(user_id = %user.id, username = ?name, "following the directory's rename");
            let user_id = Uuid::parse_str(&user.id)
                .map_err(|error| ServiceError::backend(error.to_string()))?;
            // Takes no login lock, so it is safe under the one the manager
            // holds for this sign-in, and neither does what it announces
            // (`UserUpdated` to the user's sessions, then the device check):
            // the lock does not nest. Upstream's plugin renames here too,
            // under `AuthenticateUser`'s lock, which nests there.
            self.users
                .rename_user(user_id, &user.username, &name)
                .await?;
        }
        Ok(ProviderAuthenticationResult {
            username: name,
            // The manager applies it: a provider saving the policy here would
            // wait forever on the login lock the manager holds.
            administrator,
            ..ProviderAuthenticationResult::default()
        })
    }

    /// `ChangePassword`: finds the user's entry by their name as sign-in does
    /// (`LocateLdapUser`), then, on a connection of its own bound as the bind
    /// DN, writes the new password to it ([`set_password`]). The bind DN must
    /// be allowed to set users' passwords.
    ///
    /// Refused before any connection while `AllowPassChange` is off
    /// (upstream's `NotImplementedException`, a `500`), and for a blank new
    /// password, which is what a reset sends (D11): a `400`. A name with no
    /// entry is upstream's `401`, and so is one whose entry belongs to another
    /// account (see the module's divergences). A directory that cannot be reached, or
    /// refuses the bind DN, is a `500`, as at sign-in (D4); upstream answers
    /// `401`.
    async fn change_password(
        &self,
        user: &UserEntity,
        new_password: &str,
    ) -> Result<(), ServiceError> {
        let Some(config) = self
            .usable_config()
            .await?
            .filter(|config| config.allow_pass_change)
        else {
            return Err(ServiceError::backend(
                "password changes are turned off in the LDAP-Auth settings (AllowPassChange)",
            ));
        };
        if new_password.trim().is_empty() {
            return Err(ServiceError::invalid_input(BLANK_PASSWORD));
        }
        let entry = locate_ldap_user(self.directory.as_ref(), &config, &user.username)
            .await
            .map_err(|error| search_failure(&error, "find the user", change_failure))?
            .ok_or_else(|| ServiceError::unauthorized(NO_MATCH))?;
        // Only the user's own entry: see the module's divergences.
        let uid = attribute_text(&entry, &config.ldap_uid_attribute).unwrap_or_default();
        let ldap_username = attribute_text(&entry, &config.ldap_username_attribute);
        if self
            .find_user(&uid, ldap_username.as_deref())
            .await?
            .is_none_or(|found| found.user.id != user.id)
        {
            tracing::debug!(
                user_id = %user.id,
                dn = ?entry.dn,
                "the LDAP entry the user's name found belongs to another account"
            );
            return Err(ServiceError::unauthorized(OTHER_ACCOUNTS_ENTRY));
        }
        let options = ConnectionOptions::from_config(&config);
        let mut connection = self
            .directory
            .connect(&options)
            .await
            .map_err(|error| change_failure("connect", &error))?;
        let changed = set_password(connection.as_mut(), &config, &entry.dn, new_password).await;
        connection.unbind().await;
        changed?;
        tracing::info!(user_id = %user.id, dn = ?entry.dn, "changed the user's password in the directory");
        Ok(())
    }

    /// Upstream's plugin has no `IHasNewUserPolicy`: it sets up the users it
    /// creates itself.
    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }
}

/// LDAP-Auth's password-reset provider: upstream's class is both providers,
/// and the users it creates store its id as their `PasswordResetProviderId`
/// too. A forgotten password is reset at the directory's own page.
#[async_trait]
impl PasswordResetManager for LdapAuthProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn id(&self) -> &str {
        PROVIDER_ID
    }

    /// Enabled while the plugin is and a server is configured, as for sign-in
    /// (D3). Upstream's is always enabled.
    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(self.usable_config().await?.is_some())
    }

    /// `StartForgotPasswordProcess`: a `PinCode` answer whose `PinFile` is
    /// `PasswordResetUrl`, filled in by [`password_reset_url`] with the user's
    /// id, or the MD5 of the entered name (`GetMD5`) when it names no account.
    /// With no URL configured, upstream's `NotImplementedException` (a `500`).
    async fn start_forgot_password_process(
        &self,
        user: Option<&UserEntity>,
        entered_username: &str,
        _is_in_network: bool,
    ) -> Result<ForgotPasswordResult, ServiceError> {
        let template = self
            .usable_config()
            .await?
            .and_then(|config| config.password_reset_url)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                ServiceError::backend(
                    "no password reset page is set in the LDAP-Auth settings (PasswordResetUrl)",
                )
            })?;
        let user_id = match user {
            Some(user) => Uuid::parse_str(&user.id)
                .map_err(|error| ServiceError::backend(error.to_string()))?,
            None => get_md5(entered_username),
        };
        Ok(ForgotPasswordResult {
            action: ForgotPasswordAction::PinCode,
            pin_file: Some(password_reset_url(&template, user_id, entered_username)),
            pin_expiration_date: None,
        })
    }

    /// This provider issues no PINs, so none is its to redeem. Upstream throws
    /// `NotImplementedException`, which would stop another provider redeeming
    /// its own.
    async fn redeem_password_reset_pin(&self, _pin: &str) -> Result<PinRedeemResult, ServiceError> {
        Ok(PinRedeemResult::default())
    }
}

pub(crate) mod profile_image;

#[cfg(test)]
mod sign_in_tests;
