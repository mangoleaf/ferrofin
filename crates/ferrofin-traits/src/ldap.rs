//! LDAP-Auth manager trait — the DI seam for the `/Ldap/*` routes.
//!
//! Port of the public surface of `jellyfin-plugin-ldapauth`'s `LdapController`
//! (v24, `d11c86a`). The concrete implementation lives with the extension
//! (`ferrofin-extensions::ldap_auth`) so every piece of the plugin is grouped
//! there; `ferrofin-api` handlers reach it only through this object-safe trait.

use async_trait::async_trait;
use ferrofin_model::ldap::{
    LdapFilterResponse, LdapTestErrorResponse, ServerConnectionInfo, ServerTestResponse,
    UserFilterInfo, UserSearchAttributes, UserSearchResponse,
};

use crate::error::ServiceError;

/// How a settings-page directory test ended: its result, or one of the
/// plugin's own error answers, which carry an [`LdapTestErrorResponse`] where
/// Ferrofin's errors carry their own body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LdapTestAnswer<T> {
    /// `200` with the result.
    Ok(T),
    /// `400`: the directory refused the search (`LdapController` catching an
    /// `LdapException`).
    BadRequest(LdapTestErrorResponse),
    /// `401`: the directory could not be reached or refused the configured
    /// bind DN, or the user search failed or found no one (`LdapController`
    /// catching an `AuthenticationException`).
    Unauthorized(LdapTestErrorResponse),
}

/// The LDAP-Auth plugin's settings-page operations.
///
/// Every method self-gates on the plugin's enabled flag, reporting
/// [`ServiceError::NotFound`] while it is disabled: the observable behaviour
/// of a Jellyfin server whose disabled plugin's controller is not registered.
#[async_trait]
pub trait LdapManager: Send + Sync {
    /// Succeeds while the plugin is enabled, and is
    /// [`ServiceError::NotFound`] otherwise, so a route can answer `404`
    /// before it looks at the request body.
    async fn ensure_enabled(&self) -> Result<(), ServiceError>;

    /// Saves `connection` into the plugin configuration, then connects, binds
    /// and searches the base DN with it, reporting each step.
    ///
    /// Port of `LdapController.TestServerBind` and
    /// `LdapAuthenticationProviderPlugin.TestServerBind`. A failed step is
    /// part of the answer, never an error: the result is `Err` only when the
    /// plugin is disabled or its configuration cannot be read or saved.
    async fn test_server_bind(
        &self,
        connection: ServerConnectionInfo,
    ) -> Result<ServerTestResponse, ServiceError>;

    /// Saves `filters` into the plugin configuration, then counts the entries
    /// the user filter and (while it is on) the admin filter find under the
    /// base DN.
    ///
    /// Port of `LdapController.TestLdapFilters` and
    /// `LdapAuthenticationProviderPlugin.GetFilteredUsers`. The result is `Err`
    /// only when the plugin is disabled or its configuration cannot be read or
    /// saved; a directory that fails is one of the [`LdapTestAnswer`] errors.
    async fn test_ldap_filters(
        &self,
        filters: UserFilterInfo,
    ) -> Result<LdapTestAnswer<LdapFilterResponse>, ServiceError>;

    /// Saves the search attributes into the plugin configuration, then, when a
    /// name is given, finds the entry it signs in as.
    ///
    /// Port of `LdapController.LdapUserSearch` and
    /// `LdapAuthenticationProviderPlugin.LocateLdapUser`. The result is `Err`
    /// only when the plugin is disabled or its configuration cannot be read or
    /// saved; a directory that fails, or finds no entry, is
    /// [`LdapTestAnswer::Unauthorized`].
    async fn ldap_user_search(
        &self,
        search: UserSearchAttributes,
    ) -> Result<LdapTestAnswer<UserSearchResponse>, ServiceError>;
}

fn _assert_object_safe_ldap_manager(_: &dyn LdapManager) {}
