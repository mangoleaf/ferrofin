//! Links from an outside identity to a Ferrofin user — the seam an
//! authentication provider records who signs in through it.
//!
//! An outside identity is a provider id, an issuer and a subject
//! ([`ExternalIdentityKey`]). LDAP-Auth's uid map, which upstream keeps in its
//! plugin configuration (`LdapUsers`, jellyfin-plugin-ldapauth v24
//! `Config/PluginConfiguration.cs`), is the provider id, an empty issuer and the
//! LDAP uid; OIDC later uses its issuer and `sub`. One identity belongs to at
//! most one user, a user has at most one link per provider, and a user's links
//! go when the user is deleted. The links live in the Ferrofin-owned
//! `FerrofinExternalIdentities` table (migration 0048). The trait is
//! object-safe and carries an `_assert_object_safe` assertion.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::ServiceError;

/// Which outside identity: the provider that vouches for it, the issuer and the
/// subject within that issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalIdentityKey<'a> {
    /// The authentication provider's stored id
    /// ([`AuthenticationManager::id`](crate::security::AuthenticationManager::id)),
    /// e.g. `Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin`.
    pub provider_id: &'a str,
    /// Who issued the subject: empty for LDAP, the issuer URL for OIDC.
    pub issuer: &'a str,
    /// The identity within the issuer: the LDAP uid (`LdapUid`), or OIDC's
    /// `sub`. Compared byte for byte, as upstream's `StringComparison.Ordinal`.
    /// An empty subject (a directory entry without the uid attribute) is never
    /// found, as upstream's `GetLdapUser` skips an entry whose `LdapUid` is
    /// empty, so two such users can never share an account.
    pub subject: &'a str,
}

/// One stored link (a `FerrofinExternalIdentities` row; upstream's `LdapUser`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    /// The linked user (`LinkedJellyfinUserId`).
    pub user_id: Uuid,
    /// The authentication provider's stored id.
    pub provider_id: String,
    /// Who issued the subject (empty for LDAP).
    pub issuer: String,
    /// The identity within the issuer (`LdapUid`).
    pub subject: String,
    /// A hash of the avatar last taken from the provider, so a sync can tell
    /// when it changed: LDAP-Auth's `ProfileImageHash`, the Base64 MD5 of the
    /// image. `None` when none was recorded; the store keeps upstream's empty
    /// `ProfileImageHash` as `None` too.
    pub avatar_hash: Option<String>,
    /// When the link was made.
    pub date_created: DateTime<Utc>,
}

impl ExternalIdentity {
    /// The identity this link is for.
    #[must_use]
    pub fn key(&self) -> ExternalIdentityKey<'_> {
        ExternalIdentityKey {
            provider_id: &self.provider_id,
            issuer: &self.issuer,
            subject: &self.subject,
        }
    }
}

/// Reads and writes the links between outside identities and users.
#[async_trait]
pub trait ExternalIdentityStore: Send + Sync {
    /// The link of `identity`, if a user has it (upstream's
    /// `UserHelper.GetLdapUser(ldapUid)`). `None` for an empty subject.
    async fn find(
        &self,
        identity: ExternalIdentityKey<'_>,
    ) -> Result<Option<ExternalIdentity>, ServiceError>;

    /// Every link of the provider, oldest first (`GetAllLdapUsers`),
    /// including one stored with an empty subject, which
    /// [`find`](Self::find) never returns.
    async fn list(&self, provider_id: &str) -> Result<Vec<ExternalIdentity>, ServiceError>;

    /// Links `identity` to `user_id` with `avatar_hash`, and returns the new
    /// link. In one transaction it first removes the user's link for that
    /// provider and any link of that identity, so an identity belongs to one
    /// user and a user has one link per provider, and a second link for the
    /// same identity replaces the first (`PluginConfiguration.AddUser`). An
    /// empty subject is stored as upstream stores it, but [`find`](Self::find)
    /// never returns it; an empty `avatar_hash` is stored as `None`.
    ///
    /// # Errors
    /// [`ServiceError::NotFound`] when no user has `user_id`; nothing changes.
    async fn link(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<ExternalIdentity, ServiceError>;

    /// Sets the avatar hash of `identity`'s link, as the profile-image sync
    /// does (`configUser.ProfileImageHash = …`); an empty hash is stored as
    /// `None`. Returns whether a link was there to update.
    async fn set_avatar_hash(
        &self,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<bool, ServiceError>;

    /// Removes the link of `identity`. Returns whether there was one.
    async fn unlink(&self, identity: ExternalIdentityKey<'_>) -> Result<bool, ServiceError>;
}

fn _assert_object_safe(_: &dyn ExternalIdentityStore) {}
