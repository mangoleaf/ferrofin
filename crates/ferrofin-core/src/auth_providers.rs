//! Built-in [`AuthenticationManager`] providers.
//!
//! Port of `Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider`
//! and `InvalidAuthProvider`. Each C# `IAuthenticationProvider` (with its
//! `IRequiresResolvedUser` mixin folded in) becomes an `AuthenticationManager`
//! trait impl.
//!
//! Password hashing goes through the shared [`CryptographyProvider`] from
//! `ferrofin-common` (real PBKDF2, Jellyfin-compatible), reused rather than
//! reimplemented. The C# `AuthenticationException` maps to
//! [`ServiceError::Unauthorized`].

use async_trait::async_trait;
use ferrofin_common::cryptography::{
    Constants, CryptoProvider, CryptographyProvider, PasswordHash,
};
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::users::UserPolicy;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::security::{AuthenticationManager, ProviderAuthenticationResult};

/// The C# fully-qualified type name of the default provider, stored on
/// `User.AuthenticationProviderId` and surfaced by
/// [`UserManager::get_authentication_providers`](ferrofin_traits::library::UserManager::get_authentication_providers).
pub const DEFAULT_AUTH_PROVIDER_ID: &str =
    "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider";

/// The C# fully-qualified type name of the invalid/fallback provider.
pub const INVALID_AUTH_PROVIDER_ID: &str =
    "Jellyfin.Server.Implementations.Users.InvalidAuthProvider";

/// The default local-users authentication provider (C#
/// `DefaultAuthenticationProvider`).
///
/// Verifies a stored [`PasswordHash`] against the presented password using the
/// shared crypto provider, supporting password-less users. Every PBKDF2 run
/// (verify and hash) goes to tokio's blocking pool: 210,000 rounds must not hold
/// an async worker. Upstream's "migrate old hashes to the new default" step
/// needs a write, which the [`AuthenticationManager`] trait does not expose, so
/// the `UserManager` performs it after a successful login through
/// [`upgraded_hash`](Self::upgraded_hash).
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
    /// iteration count (C# migration check).
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
    pub async fn format_password_hash(
        &self,
        new_password: &str,
    ) -> Result<Option<String>, ServiceError> {
        if new_password.is_empty() {
            return Ok(None);
        }
        let crypto = self.crypto;
        let new_password = new_password.to_owned();
        let hash = blocking(move || crypto.create_password_hash(&new_password))
            .await?
            .map_err(|e| ServiceError::Backend(format!("hashing password: {e}")))?;
        Ok(Some(hash.to_string()))
    }

    /// The hash to store after `password` verified against `stored`: a fresh
    /// default hash when `stored` is not [current](Self::hash_is_current), else
    /// `None`. Port of the "Migrate old hashes to the new default" step of C#
    /// `DefaultAuthenticationProvider.Authenticate`.
    ///
    /// # Errors
    /// Returns [`ServiceError::Backend`] if `stored` does not parse or hashing
    /// fails.
    pub async fn upgraded_hash(
        &self,
        stored: &str,
        password: &str,
    ) -> Result<Option<String>, ServiceError> {
        if stored.is_empty() {
            return Ok(None);
        }
        let hash = PasswordHash::parse(stored)
            .map_err(|e| ServiceError::Backend(format!("parsing stored password hash: {e}")))?;
        if self.hash_is_current(&hash) {
            return Ok(None);
        }
        self.format_password_hash(password).await
    }
}

/// Runs PBKDF2 work on tokio's blocking pool, so a login never holds an async
/// worker for a whole hash. Dropping the caller (a client that disconnects
/// mid-login) does not abort the work: the hash runs to completion and its
/// result is discarded.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ServiceError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ServiceError::Backend(format!("password hashing task failed: {e}")))
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
        let crypto = self.crypto;
        let password = password.to_owned();
        let ok = blocking(move || crypto.verify(&hash, &password))
            .await?
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
            .await
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

    /// PBKDF2 runs on the blocking pool: on a single-threaded runtime, a task
    /// beside a login keeps running while the hash is computed. Inline, the
    /// login's first poll would hold the thread for the whole hash.
    #[tokio::test(flavor = "current_thread")]
    async fn verify_does_not_block_the_runtime() {
        // PBKDF2-HMAC-SHA512, 20,000 iterations, of "pw": slow enough to observe.
        const HASH: &str = "$PBKDF2-SHA512$iterations=20000$0A1B2C3D4E5F60718293A4B5C6D7E8F9$\
            F4AFA1D71E05BE8B12BB2A758CEDFF65276341F9E06248F5813EB9800AB06F34\
            9C6E217DFD6285C1CBD9896C42DBF0221E4EFD81330A176C0F912D7C6FC6524C";
        let provider = DefaultAuthenticationProvider::new();
        let user = user_with_password(Some(HASH.to_owned())).await;
        let mut ticks = 0u32;
        let ticker = async {
            loop {
                tokio::task::yield_now().await;
                ticks += 1;
            }
        };
        tokio::select! {
            biased;
            result = provider.authenticate("u", "pw", Some(&user)) => assert!(result.is_ok()),
            _ = ticker => unreachable!("the ticker never ends"),
        }
        assert!(ticks > 0, "the runtime was blocked for the whole hash");
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
            .await
            .expect("hash")
            .expect("some");
        let parsed = PasswordHash::parse(&raw).expect("parse");
        assert!(provider.hash_is_current(&parsed));
        // A current hash is never re-hashed on login, nor a password-less user.
        assert_eq!(
            provider.upgraded_hash(&raw, "pw").await.expect("current"),
            None
        );
        assert_eq!(provider.upgraded_hash("", "").await.expect("empty"), None);
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
