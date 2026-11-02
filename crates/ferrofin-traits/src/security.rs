//! Security-layer traits — pluggable authentication and Quick Connect.
//!
//! Port of `MediaBrowser.Controller.Authentication.IAuthenticationProvider`
//! (surfaced here as [`AuthenticationManager`]) and
//! `MediaBrowser.Controller.QuickConnect.IQuickConnect` ([`QuickConnect`]).
//!
//! Port rules applied throughout:
//! - The C# `User` argument of `ChangePassword` becomes a [`UserEntity`] row;
//!   identity arguments become [`uuid::Uuid`].
//! - `IAuthenticationProvider` folds in the `IRequiresResolvedUser` /
//!   `IHasNewUserPolicy` mixin interfaces: `authenticate` takes an optional
//!   pre-resolved user, and [`AuthenticationManager::new_user_policy`] exposes
//!   the default policy. `ProviderAuthenticationResult` is ported as
//!   [`ProviderAuthenticationResult`].
//! - The C# `GetType().FullName` a user's `AuthenticationProviderId` stores
//!   becomes [`AuthenticationManager::id`], beside the display
//!   [`name`](AuthenticationManager::name).
//! - [`QuickConnect::get_authorized_request`] returns the
//!   [`AuthenticationResultData`] (session and token) of the C#
//!   `AuthenticationResult`; the API layer adds its `User` and `ServerId`.
//! - `Task<T>` → `async fn -> Result<T, ServiceError>`; the `IsEnabled` property
//!   becomes an `async fn` so implementations may consult live configuration.
//!
//! Both traits are object-safe and carry `_assert_object_safe_*` assertions.

use async_trait::async_trait;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::quick_connect::QuickConnectResult;
use ferrofin_model::security::AuthenticationInfo;
use ferrofin_model::users::UserPolicy;
use uuid::Uuid;

use crate::error::ServiceError;
use crate::options::AuthorizationInfo;
use crate::session::AuthenticationResultData;

/// The result of a successful provider authentication.
///
/// Port of `ProviderAuthenticationResult`: the resolved canonical username plus
/// an optional display name, and an administrator flag the C# result does not
/// carry ([`administrator`](Self::administrator)).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderAuthenticationResult {
    /// The canonical username the credentials resolved to.
    pub username: String,

    /// An optional display name supplied by the provider.
    pub display_name: Option<String>,

    /// Whether the signed-in user should be an administrator, or `None` to
    /// leave their flag as it is. Once this provider has authenticated the
    /// user, before the account checks (disabled, remote access, schedule),
    /// the user manager applies it through
    /// [`UserManager::apply_external_administrator`](crate::library::UserManager::apply_external_administrator),
    /// which never demotes the last administrator: the user stays one and a
    /// warning is logged.
    ///
    /// Upstream's LDAP plugin sets the flag itself, calling
    /// `IUserManager.UpdateUserAsync` from inside `Authenticate`, which works
    /// because upstream's user lock nests. The login lock Ferrofin holds
    /// across a provider does not, so a provider that saved the policy of the
    /// user it is signing in would wait on that lock forever. It reports the
    /// flag here instead.
    pub administrator: Option<bool>,
}

/// A pluggable authentication backend (built-in, LDAP, …).
///
/// Port of `IAuthenticationProvider` with its `IRequiresResolvedUser` and
/// `IHasNewUserPolicy` mixins folded in. Named "manager" per the Wave 4 port
/// grouping; concrete providers register in `ferrofin-core`.
#[async_trait]
pub trait AuthenticationManager: Send + Sync {
    /// The provider's display name, the `Name` `GET /Auth/Providers` lists.
    fn name(&self) -> &str;

    /// The id a user's `AuthenticationProviderId` stores to select this
    /// provider, the `Id` `GET /Auth/Providers` lists (C# `GetType().FullName`).
    ///
    /// Defaults to [`name`](Self::name). A provider whose display name and
    /// stored id differ overrides it: LDAP-Auth shows `LDAP-Authentication`
    /// and stores `Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin`.
    fn id(&self) -> &str {
        self.name()
    }

    /// Whether the provider is currently enabled.
    async fn is_enabled(&self) -> Result<bool, ServiceError>;

    /// Authenticates a username/password pair.
    ///
    /// `resolved_user` carries the already-loaded [`UserEntity`] when the caller
    /// resolved it first (folding in `IRequiresResolvedUser`); providers that do
    /// their own lookup may ignore it.
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        resolved_user: Option<&UserEntity>,
    ) -> Result<ProviderAuthenticationResult, ServiceError>;

    /// Changes a user's password.
    async fn change_password(
        &self,
        user: &UserEntity,
        new_password: &str,
    ) -> Result<(), ServiceError>;

    /// The default policy to apply to users newly created by this provider, if
    /// it dictates one (folding in `IHasNewUserPolicy`).
    fn new_user_policy(&self) -> Option<UserPolicy>;
}

fn _assert_object_safe_authentication_manager(_: &dyn AuthenticationManager) {}

/// Drives the Quick Connect pairing flow.
///
/// Port of `IQuickConnect`. The `AuthorizationInfo` initiator argument is the
/// ported [`AuthorizationInfo`] context.
#[async_trait]
pub trait QuickConnect: Send + Sync {
    /// Whether Quick Connect is currently enabled.
    async fn is_enabled(&self) -> Result<bool, ServiceError>;

    /// Initiates a new Quick Connect request for the given initiator.
    async fn try_connect(
        &self,
        authorization_info: &AuthorizationInfo,
    ) -> Result<QuickConnectResult, ServiceError>;

    /// Checks the status of an in-flight request by its secret.
    async fn check_request_status(&self, secret: &str) -> Result<QuickConnectResult, ServiceError>;

    /// Authorizes a request to connect as the given user, by its short code.
    async fn authorize_request(&self, user_id: Uuid, code: &str) -> Result<bool, ServiceError>;

    /// Gets the authentication result (session and access token) opened when
    /// the request's code was authorized (C# `GetAuthorizedRequest`). The
    /// secret exchange returns it as is and never authenticates again.
    async fn get_authorized_request(
        &self,
        secret: &str,
    ) -> Result<AuthenticationResultData, ServiceError>;
}

fn _assert_object_safe_quick_connect(_: &dyn QuickConnect) {}

/// Manages long-lived server API keys.
///
/// Port of the API-key surface of
/// `MediaBrowser.Controller.Security.IAuthenticationManager` (the
/// `GetApiKeys`/`CreateApiKey`/`DeleteApiKey` triple used by `ApiKeyController`).
/// The C# interface's authorization-provider concerns are handled separately by
/// [`AuthenticationManager`]; this trait is the persistence-backed key store.
#[async_trait]
pub trait ApiKeyManager: Send + Sync {
    /// Lists every stored API key as an [`AuthenticationInfo`].
    ///
    /// Mirrors `GetApiKeys`: each key becomes an [`AuthenticationInfo`] whose
    /// `AppName`/`AccessToken`/`DateCreated` come from the key row and whose
    /// device fields are empty.
    async fn get_api_keys(&self) -> Result<Vec<AuthenticationInfo>, ServiceError>;

    /// Creates a new API key named `name`, generating its access token.
    ///
    /// Mirrors `CreateApiKey`: the token is a fresh 32-hex-digit GUID and the
    /// created/last-activity timestamps are set to now.
    async fn create_api_key(&self, name: &str) -> Result<(), ServiceError>;

    /// Deletes the API key whose access token is `access_token` (a no-op when no
    /// key matches). Mirrors `DeleteApiKey`.
    async fn delete_api_key(&self, access_token: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_api_key_manager(_: &dyn ApiKeyManager) {}

#[cfg(test)]
mod tests {
    use super::ProviderAuthenticationResult;

    #[test]
    fn provider_result_default_is_empty() {
        let r = ProviderAuthenticationResult::default();
        assert_eq!(r.username, "");
        assert_eq!(r.display_name, None);
        assert_eq!(r.administrator, None, "no flag leaves the user's alone");
    }

    #[test]
    fn provider_result_carries_fields() {
        let r = ProviderAuthenticationResult {
            username: "alice".to_owned(),
            display_name: Some("Alice".to_owned()),
            administrator: Some(true),
        };
        assert_eq!(r.username, "alice");
        assert_eq!(r.display_name.as_deref(), Some("Alice"));
        assert_eq!(r.administrator, Some(true));
    }
}
