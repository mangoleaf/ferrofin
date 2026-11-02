//! LDAP-Auth manager trait — the DI seam for the `/Ldap/*` routes.
//!
//! Port of the public surface of `jellyfin-plugin-ldapauth`'s `LdapController`
//! (v24, `d11c86a`). The concrete implementation lives with the extension
//! (`ferrofin-extensions::ldap_auth`) so every piece of the plugin is grouped
//! there; `ferrofin-api` handlers reach it only through this object-safe trait.

use async_trait::async_trait;
use ferrofin_model::ldap::{ServerConnectionInfo, ServerTestResponse};

use crate::error::ServiceError;

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
}

fn _assert_object_safe_ldap_manager(_: &dyn LdapManager) {}
