//! Sign-in through the directory: the authentication provider LDAP-Auth
//! contributes, a port of `LdapAuthenticationProviderPlugin.Authenticate`,
//! `LocateLdapUser`, `ConnectToLdap` and `LdapUtils.SanitizeFilter`
//! (jellyfin-plugin-ldapauth v24, `d11c86a`).
//!
//! A sign-in finds the user's directory entry as the bind DN, finds or
//! relinks the Ferrofin user it belongs to (by LDAP uid through the
//! external-identity store, then by name for a user LDAP-Auth already owns),
//! checks the password by binding as the entry, asks the admin filter whether
//! the user is an administrator, then creates the user or follows a directory
//! rename. The administrator flag goes back to the user manager in
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
//!   DN, or fails the user search or the admin filter's (a referral that
//!   cannot be followed included) is a server error, which the user manager
//!   does not count toward lockout. Upstream answers each with an
//!   `AuthenticationException`.
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
//!   that attribute must be one users cannot edit. TODO(gl-1188): the owner
//!   decides how to close that.
//! - **A link is written only after the password is checked.** Upstream
//!   records a relink by name before it binds as the user.
//! - **An entry without the uid attribute is not linked.** Upstream stores a
//!   null `LdapUid`, which no lookup finds, and in doing so drops the user's
//!   earlier link and every other null-uid link. Such a user is found by name
//!   each time, and keeps the link of the uid they had.
//! - **A new user is created with its provider ids in one transaction**
//!   ([`UserManager::create_external_user`]). Upstream saves them after
//!   creating the user, a moment in which the account has no password and
//!   the default provider takes an empty one.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::library::{ExternalUserSetup, UserManager};
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::security::{AuthenticationManager, ProviderAuthenticationResult};
use ferrofin_util::string_extensions::replace_ignore_ascii_case;
use uuid::Uuid;

use super::{
    BoundSearchError, ConnectionOptions, EXTENSION_ID, LdapConfig, LdapDirectory, LdapEntry,
    LdapError, NO_ATTRIBUTES, config_error, novell_bind, search_as_bind_dn,
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
/// following referrals, and returns the first entry, warning when there are
/// more, or `None` when none matches.
pub(crate) async fn locate_ldap_user(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    username: &str,
) -> Result<Option<LdapEntry>, BoundSearchError> {
    let filter = user_search_filter(config, username);
    tracing::debug!(base_dn = ?config.ldap_base_dn, filter = ?filter, "LDAP user search");
    let mut found = search_as_bind_dn(
        directory,
        config,
        &config.ldap_base_dn,
        &filter,
        &[
            config.ldap_username_attribute.as_str(),
            config.ldap_uid_attribute.as_str(),
        ],
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

/// The [`outage`] for a search as the configured bind DN that failed at the
/// connection, the bind, or the search the sign-in calls `search_step`.
fn search_outage(search_step: &str, error: &BoundSearchError) -> ServiceError {
    match error {
        BoundSearchError::Connect(error) => outage("connect", error),
        BoundSearchError::Bind(error) => outage("bind as the configured bind DN", error),
        BoundSearchError::Search(error) => outage(search_step, error),
    }
}

/// The LDAP-Auth authentication provider. Reads the plugin's enabled flag
/// and configuration at each call, so enabling, disabling or reconfiguring
/// the plugin needs no restart.
pub struct LdapAuthProvider {
    plugins: Arc<dyn PluginManager>,
    users: Arc<dyn UserManager>,
    identities: Arc<dyn ExternalIdentityStore>,
    directory: Arc<dyn LdapDirectory>,
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
    /// LDAP uid links, and the directory it connects through.
    #[must_use]
    pub fn new(
        plugins: Arc<dyn PluginManager>,
        users: Arc<dyn UserManager>,
        identities: Arc<dyn ExternalIdentityStore>,
        directory: Arc<dyn LdapDirectory>,
    ) -> Self {
        Self {
            plugins,
            users,
            identities,
            directory,
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
    fn identity(uid: &str) -> ExternalIdentityKey<'_> {
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
            .map_err(|error| search_outage("search the directory", &error))?
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
        .map_err(|error| search_outage("apply the admin filter", &error))?;
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

    /// Links `user_id` to the entry's uid (`PluginConfiguration.AddUser`). An
    /// entry without the uid attribute is not linked: see the module's
    /// divergences.
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
    /// attribute when LDAP-Auth owns them (`AuthenticationProviderId`), which
    /// then needs linking. A local account with that name is not taken.
    async fn find_user(
        &self,
        uid: &str,
        ldap_username: Option<&str>,
    ) -> Result<Option<FoundUser>, ServiceError> {
        if let Some(link) = self.identities.find(Self::identity(uid)).await?
            && let Some(user) = self.users.get_user_by_id(link.user_id).await?
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
            .filter(|user| {
                user.authentication_provider_id
                    .eq_ignore_ascii_case(PROVIDER_ID)
            })
            .map(|user| FoundUser {
                user,
                needs_link: true,
            }))
    }
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
            self.link(&created.id, &uid).await?;
            return Ok(ProviderAuthenticationResult {
                username: name,
                // Upstream sets the flag on every user it creates, `false`
                // while the filter is off.
                administrator: Some(administrator.unwrap_or(false)),
                ..ProviderAuthenticationResult::default()
            });
        };

        if needs_link {
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

    /// `ChangePassword`: refused with upstream's `NotImplementedException` (a
    /// `500`) while `AllowPassChange` is off.
    async fn change_password(
        &self,
        _user: &UserEntity,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        let allowed = self
            .usable_config()
            .await?
            .is_some_and(|config| config.allow_pass_change);
        if !allowed {
            return Err(ServiceError::backend(
                "password changes are turned off in the LDAP-Auth settings (AllowPassChange)",
            ));
        }
        // TODO(gl-1146): write the password to the directory with the RFC 3062
        // password modify operation, falling back to `LdapPasswordAttribute`,
        // and refuse a blank one (D11).
        Err(ServiceError::backend(
            "changing an LDAP user's password in the directory is not ported yet",
        ))
    }

    /// Upstream's plugin has no `IHasNewUserPolicy`: it sets up the users it
    /// creates itself.
    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }
}

#[cfg(test)]
mod sign_in_tests;
