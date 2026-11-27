//! The Ferrofin account a verified OIDC identity signs in to (B6), and the
//! authentication provider the accounts sign-in creates belong to, which
//! refuses every password (B11, D5).
//!
//! An identity is linked to an account by its issuer and subject, in the
//! shared identity store under [`PROVIDER_ID`] (D3), never by name:
//! - linked: that account, whatever its provider now is (a local account a
//!   user links keeps its password, D5);
//! - not linked, with the provider's `CreateUsers` off: refused;
//! - not linked, with `CreateUsers` on: an account named by the username
//!   claim is created under [`PROVIDER_ID`] and linked. A missing claim, a
//!   name Ferrofin refuses and a name an account already has are refused,
//!   and nothing is created (D4).
//!
//! The account is created with [`UserManager::create_external_user`], which
//! gives it its provider inside the transaction that inserts it, never under
//! the default provider first: a default-provider account with no password
//! accepts an empty one, which was the community SSO plugin's bug on Jellyfin
//! 10.11. In the same transaction it gets the hash of a random secret no one
//! keeps as its password (D21), so when its provider later becomes the
//! default one (a save of its user page while the plugin is off does that),
//! no password signs in to it until an administrator sets one.
//!
//! A refusal's page says what to ask an administrator, and its one `warn`
//! line holds its reason code only, never the name or another claim (B15).
//!
//! [`UserManager::create_external_user`]: ferrofin_traits::library::UserManager::create_external_user

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::sso::OidcProvider;
use ferrofin_model::users::UserPolicy;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::ExternalIdentityKey;
use ferrofin_traits::library::ExternalUserSetup;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::security::{AuthenticationManager, ProviderAuthenticationResult};
use uuid::Uuid;

use super::sign_in::{Refusal, VerifiedIdentity};
use super::{SsoService, plugin_enabled};

/// The stored id of the provider the accounts sign-in creates belong to:
/// their `AuthenticationProviderId` and `PasswordResetProviderId`, and the
/// provider id of every OIDC identity link. It is written into user rows for
/// good, so it never changes. It is not the community plugin's
/// `Jellyfin.Plugin.SSO_Auth.Api.SSOController`: accounts an adopted install
/// carries under that id stay with the invalid provider (D9).
pub const PROVIDER_ID: &str = "Ferrofin.SingleSignOn.SsoAuthenticationProvider";

/// The provider's name, as `GET /Auth/Providers` and the user page show it.
pub const PROVIDER_NAME: &str = "Single Sign-On";

/// The answer to a password change for an account of this provider.
const NO_PASSWORD: &str = "This account signs in through Single Sign-On and has no password. An \
                           administrator can give it one by changing its authentication provider \
                           on its profile page.";

/// No account is linked, and the provider creates none.
const NO_ACCOUNT: Refusal = Refusal::account(
    "no_account",
    "No Ferrofin account is linked to your sign-in, and this provider does not create accounts. \
     Ask an administrator to create an account for you and link it to your sign-in.",
);

/// The username claim is missing, or is not a string.
const NO_USERNAME: Refusal = Refusal::account(
    "no_username",
    "The identity provider did not send the name your new Ferrofin account would have. Ask an \
     administrator to check the provider's username claim.",
);

/// The username claim is not a name Ferrofin takes.
const INVALID_USERNAME: Refusal = Refusal::account(
    "invalid_username",
    "Your name at the identity provider cannot be a Ferrofin username, which holds only letters, \
     digits, spaces and the characters - ' . _ @ +, and does not start or end with a space. Ask an \
     administrator to create an account for you and link it to your sign-in.",
);

/// An account already has the name. It says nothing of that account.
const USERNAME_TAKEN: Refusal = Refusal::account(
    "username_taken",
    "A Ferrofin account already has your name, and it is not linked to your sign-in. Ask an \
     administrator to link that account to your sign-in, or to rename one of them.",
);

/// The authentication provider of the accounts sign-in creates: it refuses
/// every password, an empty one included, and every password change, and
/// its refusals never count toward an account's lockout (D20). It is enabled
/// with the plugin; while the plugin is off, the user manager's invalid
/// provider refuses those accounts in its place, so they fail closed.
pub struct SsoAuthProvider {
    plugins: Arc<dyn PluginManager>,
}

impl SsoAuthProvider {
    /// The provider over the plugin manager, which holds the plugin's
    /// enabled flag.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>) -> Self {
        Self { plugins }
    }
}

#[async_trait]
impl AuthenticationManager for SsoAuthProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn id(&self) -> &str {
        PROVIDER_ID
    }

    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        plugin_enabled(self.plugins.as_ref()).await
    }

    async fn authenticate(
        &self,
        _username: &str,
        _password: &str,
        _resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        Err(ServiceError::unauthorized(
            "the account signs in through Single Sign-On only",
        ))
    }

    async fn change_password(
        &self,
        _user: &UserEntity,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::Forbidden(NO_PASSWORD.to_owned()))
    }

    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }

    fn counts_failed_passwords(&self) -> bool {
        false
    }
}

impl SsoService {
    /// The account `identity` signs in to at `provider`: the one its link
    /// names, else, when the provider creates accounts, a new one, linked.
    /// `Ok(Err(refusal))` when there is none to sign in to; `Err` when the
    /// accounts or the links cannot be read or written.
    pub(super) async fn account(
        &self,
        provider: &OidcProvider,
        identity: &VerifiedIdentity,
    ) -> Result<Result<UserEntity, Refusal>, ServiceError> {
        // The store never finds an empty subject, so its account could never
        // be signed in to again.
        if identity.subject.is_empty() {
            return Ok(Err(Refusal::bad("id_token_subject")));
        }
        let key = ExternalIdentityKey {
            provider_id: PROVIDER_ID,
            issuer: &identity.issuer,
            subject: &identity.subject,
        };
        if let Some(user) = self.linked_user(key).await? {
            return Ok(Ok(user));
        }
        if !provider.create_users {
            return Ok(Err(NO_ACCOUNT));
        }
        let Some(name) = identity.username.as_deref() else {
            return Ok(Err(NO_USERNAME));
        };
        // One creation at a time, so a second first sign-in of the identity
        // waits for the first's account and finds it linked.
        let _provisioning = self.provisioning.lock().await;
        if let Some(user) = self.linked_user(key).await? {
            return Ok(Ok(user));
        }
        let setup = ExternalUserSetup {
            authentication_provider_id: PROVIDER_ID.to_owned(),
            password_reset_provider_id: PROVIDER_ID.to_owned(),
            // As a new local account's: role mapping (D6) may narrow it.
            enable_all_folders: true,
            enabled_folders: None,
            // D21: never passwordless, whatever provider it later has.
            unusable_password: true,
        };
        let created = match self.users.create_external_user(name, &setup).await {
            Ok(user) => user,
            Err(error) => {
                // Another server (two pods in a rolling update) may have just
                // created and linked this identity's own account, its name
                // or its write colliding with this one: look the link up
                // again first.
                if let Some(user) = self.linked_user(key).await? {
                    return Ok(Ok(user));
                }
                // A name Ferrofin refuses, or one an account has (compared as
                // the user manager finds names), so no account is ever taken
                // over by its name.
                if !matches!(error, ServiceError::InvalidInput(_)) {
                    return Err(error);
                }
                let taken = self.users.get_user_by_name(name).await?.is_some();
                return Ok(Err(if taken {
                    USERNAME_TAKEN
                } else {
                    INVALID_USERNAME
                }));
            }
        };
        let user_id = Uuid::parse_str(&created.id)
            .map_err(|error| ServiceError::backend(error.to_string()))?;
        if let Err(error) = self.identities.link(user_id, key, None).await {
            // Never matched by name, an unlinked account would only hold the
            // name and refuse this identity at every later sign-in.
            if let Err(removal) = self.users.delete_user(user_id).await {
                tracing::warn!(
                    oidc_provider = %provider.key,
                    user_id = %created.id,
                    error = %removal,
                    "the account of an OIDC identity was created but not linked, and cannot be \
                     removed; an administrator must delete it before that identity signs in"
                );
            }
            return Err(error);
        }
        tracing::info!(
            oidc_provider = %provider.key,
            user_id = %created.id,
            "created and linked the account of an OIDC identity"
        );
        Ok(Ok(created))
    }

    /// The account `key`'s link names, while it exists.
    async fn linked_user(
        &self,
        key: ExternalIdentityKey<'_>,
    ) -> Result<Option<UserEntity>, ServiceError> {
        match self.identities.find(key).await? {
            Some(link) => self.users.get_user_by_id(link.user_id).await,
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests;
