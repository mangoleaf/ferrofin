//! Built-in [`AuthenticationManager`] providers.
//!
//! Port of `Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider`
//! and `InvalidAuthProvider`. Each C# `IAuthenticationProvider` (with its
//! `IRequiresResolvedUser` mixin folded in) becomes an `AuthenticationManager`
//! trait impl.
//!
//! Password hashing goes through the shared [`CryptographyProvider`] from
//! `hermit-common` (real PBKDF2, Jellyfin-compatible), reused rather than
//! reimplemented. The C# `AuthenticationException` maps to
//! [`ServiceError::Unauthorized`].

use async_trait::async_trait;
use hermit_common::cryptography::{Constants, CryptoProvider, CryptographyProvider, PasswordHash};
use hermit_db::entities::users::UserEntity;
use hermit_model::users::UserPolicy;

use hermit_traits::error::ServiceError;
use hermit_traits::security::{AuthenticationManager, ProviderAuthenticationResult};

/// The C# fully-qualified type name of the default provider, stored on
/// `User.AuthenticationProviderId` and surfaced by
/// [`UserManager::get_authentication_providers`](hermit_traits::library::UserManager::get_authentication_providers).
pub const DEFAULT_AUTH_PROVIDER_ID: &str =
    "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider";

/// The C# fully-qualified type name of the invalid/fallback provider.
pub const INVALID_AUTH_PROVIDER_ID: &str =
    "Jellyfin.Server.Implementations.Users.InvalidAuthProvider";

/// The default local-users authentication provider (C#
/// `DefaultAuthenticationProvider`).
///
/// Verifies a stored [`PasswordHash`] against the presented password using the
/// shared crypto provider, supporting password-less users. Unlike upstream it
/// does **not** re-hash (migrate) legacy hashes in-place on login: `authenticate`
/// takes the user by shared reference, so a hash upgrade would need a write path
/// the [`AuthenticationManager`] trait does not expose. Migration on next
/// explicit password change still applies. (Flagged as a deliberate port
/// simplification.)
#[derive(Debug, Clone, Default)]
pub struct DefaultAuthenticationProvider {
    crypto: CryptographyProvider,
}

impl DefaultAuthenticationProvider {
    /// Creates the provider over the shared crypto implementation.
    #[must_use]
    pub fn new() -> Self {
        Self {
            crypto: CryptographyProvider::new(),
        }
    }

    /// Whether a stored hash is already at the current default method and
    /// iteration count (C# migration check). Exposed for the `UserManager` to
    /// decide whether to re-hash on an authenticated password touch.
    #[must_use]
    pub fn hash_is_current(&self, hash: &PasswordHash) -> bool {
        if hash.id() != self.crypto.default_hash_method() {
            return false;
        }
        hash.parameters()
            .iter()
            .find(|(k, _)| k == "iterations")
            .and_then(|(_, v)| v.parse::<u32>().ok())
            .is_some_and(|iters| iters == Constants::DEFAULT_ITERATIONS)
    }

    /// Formats a fresh password hash for `new_password`, or `None` for an empty
    /// password (a password-less user). Mirrors C# `ChangePassword`'s hash
    /// production so the `UserManager` can persist it.
    ///
    /// # Errors
    /// Returns [`ServiceError::Backend`] if hashing fails.
    pub fn format_password_hash(&self, new_password: &str) -> Result<Option<String>, ServiceError> {
        if new_password.is_empty() {
            return Ok(None);
        }
        let hash = self
            .crypto
            .create_password_hash(new_password)
            .map_err(|e| ServiceError::Backend(format!("hashing password: {e}")))?;
        Ok(Some(hash.to_string()))
    }
}

#[async_trait]
impl AuthenticationManager for DefaultAuthenticationProvider {
    // The trait fixes the signature to `-> &str`, so the `&'static` the literal
    // would allow is not expressible here.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "Default"
    }

    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }

    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        let user = resolved_user
            .ok_or_else(|| ServiceError::Unauthorized("Invalid username or password".to_owned()))?;

        // Jellyfin supports password-less users: an empty stored password and an
        // empty presented password authenticate.
        let stored = user.password.as_deref().unwrap_or("");
        if stored.is_empty() && password.is_empty() {
            return Ok(ProviderAuthenticationResult {
                username: username.to_owned(),
                display_name: None,
            });
        }

        // A password was presented but none is stored: reject.
        if user.password.is_none() {
            return Err(ServiceError::Unauthorized(
                "Invalid username or password".to_owned(),
            ));
        }

        let hash = PasswordHash::parse(stored)
            .map_err(|e| ServiceError::Backend(format!("parsing stored password hash: {e}")))?;
        let ok = self
            .crypto
            .verify(&hash, password)
            .map_err(|e| ServiceError::Backend(format!("verifying password: {e}")))?;
        if !ok {
            return Err(ServiceError::Unauthorized(
                "Invalid username or password".to_owned(),
            ));
        }

        Ok(ProviderAuthenticationResult {
            username: username.to_owned(),
            display_name: None,
        })
    }

    async fn change_password(
        &self,
        _user: &UserEntity,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        // The trait takes the user by shared reference, so the actual persistence
        // of the new hash is performed by `UserManager::change_password`, which
        // owns the write path (via `format_password_hash`). This method is a
        // no-op success to satisfy the trait; the C# equivalent mutated the
        // passed-in user in place, which our persistence split disallows.
        Ok(())
    }

    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }
}

/// The fallback provider assigned to a user whose configured authentication
/// provider is missing (C# `InvalidAuthProvider`). It is always disabled and
/// rejects every authentication attempt.
#[derive(Debug, Clone, Copy, Default)]
pub struct InvalidAuthProvider;

impl InvalidAuthProvider {
    /// Creates the provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AuthenticationManager for InvalidAuthProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "InvalidOrMissingAuthenticationProvider"
    }

    async fn is_enabled(&self) -> Result<bool, ServiceError> {
        Ok(false)
    }

    async fn authenticate(
        &self,
        _username: &str,
        _password: &str,
        _resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError> {
        Err(ServiceError::Unauthorized(
            "User Account cannot login with this provider. The Normal provider for this user cannot be found"
                .to_owned(),
        ))
    }

    async fn change_password(
        &self,
        _user: &UserEntity,
        _new_password: &str,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    fn new_user_policy(&self) -> Option<UserPolicy> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_user, test_db};
    use uuid::Uuid;

    /// Builds a bare [`UserEntity`] with the given stored password.
    async fn user_with_password(password: Option<String>) -> UserEntity {
        let db = test_db().await;
        let id = Uuid::from_u128(9);
        let mut user = seed_user(&db, id).await;
        user.password = password;
        user
    }

    #[tokio::test]
    async fn passwordless_user_authenticates_with_empty_password() {
        let provider = DefaultAuthenticationProvider::new();
        let user = user_with_password(None).await;
        let result = provider
            .authenticate("u", "", Some(&user))
            .await
            .expect("passwordless auth");
        assert_eq!(result.username, "u");
    }

    #[tokio::test]
    async fn wrong_password_is_unauthorized() {
        let provider = DefaultAuthenticationProvider::new();
        let hash = provider
            .format_password_hash("correct")
            .expect("hash")
            .expect("some hash");
        let user = user_with_password(Some(hash)).await;

        assert!(
            provider
                .authenticate("u", "correct", Some(&user))
                .await
                .is_ok()
        );
        assert!(matches!(
            provider.authenticate("u", "wrong", Some(&user)).await,
            Err(ServiceError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn no_resolved_user_is_unauthorized() {
        let provider = DefaultAuthenticationProvider::new();
        assert!(matches!(
            provider.authenticate("u", "p", None).await,
            Err(ServiceError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn fresh_hash_is_current() {
        let provider = DefaultAuthenticationProvider::new();
        let raw = provider
            .format_password_hash("pw")
            .expect("hash")
            .expect("some");
        let parsed = PasswordHash::parse(&raw).expect("parse");
        assert!(provider.hash_is_current(&parsed));
    }

    #[tokio::test]
    async fn invalid_provider_always_rejects() {
        let provider = InvalidAuthProvider::new();
        assert!(!provider.is_enabled().await.expect("enabled"));
        assert!(matches!(
            provider.authenticate("u", "p", None).await,
            Err(ServiceError::Unauthorized(_))
        ));
    }
}
