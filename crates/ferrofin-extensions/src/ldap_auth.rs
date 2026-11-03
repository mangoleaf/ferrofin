//! The **LDAP-Auth** extension — a compiled-in port of
//! [`jellyfin-plugin-ldapauth`](https://github.com/jellyfin/jellyfin-plugin-ldapauth)
//! (GUID `958aad66-3784-4d2a-b89a-a7b6fab6e25c`), faithful to v24 (`d11c86a`).
//!
//! Ported surface so far, grouped here so the whole plugin lives in one place:
//! - [`LdapAuthExtension`] — the `/Plugins` presentation, the plugin
//!   configuration ([`LdapConfig`], upstream `PluginConfiguration`) and the
//!   vendored upstream settings page;
//! - [`LdapDirectory`] and [`LdapConnection`] — the directory seam (connect
//!   with upstream's TLS options, simple bind, search, modify, the RFC 3062
//!   password modify and the root DSE's supported extensions), implemented with
//!   `ldap3` by [`directory::Ldap3Directory`];
//! - [`LdapAuthService`] — `POST /Ldap/TestServerBind`,
//!   `POST /Ldap/TestLdapFilters` and `POST /Ldap/LdapUserSearch` (the three
//!   `LdapController` actions), which stay in `ferrofin-api` as thin handlers
//!   over the [`LdapManager`] trait;
//! - [`provider::LdapAuthProvider`] — sign-in
//!   (`LdapAuthenticationProviderPlugin.Authenticate`) with its admin filter,
//!   password changes (`ChangePassword`) and forgotten passwords
//!   (`StartForgotPasswordProcess`): the authentication and password-reset
//!   provider the extension contributes, listed by `GET /Auth/Providers` and
//!   `GET /Auth/PasswordResetProviders` while the plugin is enabled and names a
//!   server;
//! - `provider::profile_image` — users' pictures from the directory
//!   (`LdapProfileImageFormat`'s binary, Base64 or URL values): a new user's
//!   at their first sign-in, and the `LdapProfileImageSync` task
//!   (`LdapProfileImageSyncTask`) that keeps every LDAP user's current, a URL
//!   fetched through the server's image download limits;
//! - [`adoption`] — the one-time import of an adopted Jellyfin's
//!   `LDAP-Auth.xml`, its uid map included, which turns the plugin on when a
//!   user's saved authentication provider is LDAP-Auth's (B7, D7);
//! - `referrals` — following the referrals the user search (at sign-in, in
//!   a password change and on the settings page), the filter test and the
//!   admin filter's search meet, with the bind credentials, as upstream's
//!   `GetSearchConstraints` and `LdapAuthHandler` do. The user's own bind and
//!   a password write stay on the configured server.
//!
//! Accepted divergences from the C# (see `docs/PLUGINS_UPSTREAM.md`):
//! - The plugin is **disabled by default**, and `LdapServer` seeds empty
//!   instead of `ldap-server.contoso.com`, so an unconfigured server is
//!   detectable (D3). Upstream's provider is always enabled and would try the
//!   placeholder host on every unknown sign-in. An adopted `LDAP-Auth.xml`
//!   still naming that placeholder is imported with the server empty.
//! - The configuration has **no `LdapUsers`**: which LDAP uid belongs to which
//!   user is kept in Ferrofin's external-identity table (D12), so
//!   `GET /Plugins/{id}/Configuration` does not return it. Upstream's settings
//!   page never reads or writes it.
//! - An adopted `LDAP-Auth.xml` is **imported once and never written back**
//!   ([`adoption`]), and the importer trims each value, so a bind password
//!   with leading or trailing spaces comes across without them.
//! - **Timeouts** (D6): connecting gives up after [`CONNECT_TIMEOUT`] and each
//!   operation after [`OPERATION_TIMEOUT`]; upstream sets none.
//! - A custom root CA is the **only** trust anchor: the server's certificate
//!   must chain to a certificate in that file, or be one of them (a
//!   self-signed server certificate given as the root CA), and be valid for
//!   the server's name. Upstream's chain policy also sets
//!   `AllowUnknownCertificateAuthority`, with which .NET accepts a chain ending
//!   at a root that is not in the file.
//! - A stored `LdapProfileImageFormat` that names no format reads as
//!   `Default`; .NET's deserializer would fail the whole configuration.
//! - A client certificate or key file that cannot be read fails the test's
//!   Connect step with the reason. Upstream reads them before its `try`, so the
//!   request fails with a `500`.
//! - The `Error` text is Ferrofin's own: the RFC 4511 result name and the
//!   server's diagnostic for a refused operation, or the transport's message.
//!   Upstream reports the Novell client's exception message.
//! - The connection test's base search, the filter test's searches and the
//!   admin filter's search at sign-in ask for no attributes (`1.1`): they read
//!   only the entries' DNs, which the server sends either way.
//! - The filter test keeps the saved `EnableLdapAdminFilterMemberUid` when
//!   the body leaves it out. Upstream saves the missing member as `false`, and
//!   its own settings page never sends it, so pressing the test there turns
//!   member-uid mode off until the page is saved again (a Jellyfin bug not
//!   ported).
//! - The filter test's `400` reports the directory client's own wording for a
//!   filter that does not parse (`Filter Error`). Upstream pulls Novell's
//!   parser message out of the exception's text.
//! - A followed referral keeps the configured TLS, its chain stops after
//!   [`REFERRAL_HOP_LIMIT`] referrals, a search chases at most
//!   [`REFERRAL_BUDGET`] in all, one that leads to no server is skipped with a
//!   warning wherever it arrives, and its URL is read as RFC 4516 writes it:
//!   see `referrals`. Upstream's Novell client connects to a referred server
//!   in plain LDAP, never stops a referral loop, sets no total, and drops a
//!   referral it cannot follow when it comes after the last entry or is a
//!   `referral` result.

pub mod adoption;
pub mod directory;
pub mod provider;
mod referrals;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_core::{PluginConfigPage, ScheduledTask};
use ferrofin_model::ldap::{
    LdapFilterResponse, LdapTestErrorResponse, ServerConnectionInfo, ServerTestResponse,
    UserFilterInfo, UserSearchAttributes, UserSearchResponse,
};
use ferrofin_model::secret::Secret;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::ldap::{LdapManager, LdapTestAnswer};
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use ferrofin_traits::security::{AuthenticationManager, PasswordResetManager};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::{Extension, ExtensionContext};

/// The LDAP-Auth plugin's stable id: the **upstream plugin's GUID**, so the
/// vendored settings page (which calls `getPluginConfiguration` with this exact
/// id) and an adopted Jellyfin's plugin state recognise it.
pub const EXTENSION_ID: Uuid = Uuid::from_u128(0x958a_ad66_3784_4d2a_b89a_a7b6_fab6_e25c);

/// How long opening a connection may take: the TCP connect and, for LDAPS,
/// the TLS handshake (D6).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long each directory operation may wait for an answer (D6). StartTLS
/// counts as one. A search's timer restarts with every entry the server sends,
/// so a long answer that keeps arriving is not cut off.
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);

/// The most referrals a search follows in a row (owner decision, Novell's
/// default `HopLimit`): a referred server that refers the search on once more
/// fails it, which is what stops a referral loop.
pub const REFERRAL_HOP_LIMIT: usize = 10;

/// The most referrals one search chases in all, followed or skipped (owner
/// decision, gl-1208, 2026-11-03): the one past it fails the search. Each
/// referral costs the D6 timeouts again ([`CONNECT_TIMEOUT`] and
/// [`OPERATION_TIMEOUT`], for each of its URLs), and the hop limit bounds
/// only a chain, not how many referrals each answer carries, so this bounds
/// how many referrals one search chases, and with them most of the time it
/// can take (a referral's URLs are not counted). Novell's client sets no
/// total.
pub const REFERRAL_BUDGET: usize = 10;

/// The `referral` result code (RFC 4511 section 4.1.9): the server holds
/// none of the operation's target and names where to send it.
const REFERRAL: u32 = 10;

/// What upstream's Novell client searches for when the filter is empty, as the
/// connection test's base search does.
const EMPTY_FILTER: &str = "(objectclass=*)";

/// The attribute list that asks for no attributes (RFC 4511 section 4.5.1.8):
/// the base search only counts entries.
const NO_ATTRIBUTES: &str = "1.1";

/// A step the connection test has started but not finished. The settings page
/// reads the first step still showing it as the one that failed.
const TESTING: &str = "Testing...";
/// A step that succeeded.
const SUCCESS: &str = "Success";
/// The bind step when no bind password was given.
const ANONYMOUS: &str = "Anonymous";

/// The admin filter that turns it off, which the settings page saves for an
/// empty box. An empty filter is off too.
pub const ADMIN_FILTER_DISABLED: &str = "_disabled_";

/// The `401` message for a directory that could not be reached or refused the
/// configured bind DN: upstream `ConnectToLdap()`'s.
const CONNECT_FAILED: &str = "Failed to Connect or Bind to server.";
/// The `401` message for a user search the directory failed: upstream
/// `LocateLdapUser`'s.
const USER_FILTER_FAILED: &str = "Error completing LDAP login while applying user filter.";
/// What the filter test's `400` puts before the reason, by the filter that
/// failed.
const USER_FILTER_LABEL: &str = "User Filter: ";
const ADMIN_FILTER_LABEL: &str = "Admin Filter: ";

/// The format of the directory attribute holding a user's picture (upstream
/// `ProfileImageFormat`), stored by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum ProfileImageFormat {
    /// Work it out from the value.
    #[default]
    Default,
    /// The attribute holds the image bytes.
    Binary,
    /// The attribute holds the image as Base64 text.
    Base64,
    /// The attribute holds a URL to fetch the image from.
    Url,
}

impl<'de> Deserialize<'de> for ProfileImageFormat {
    /// Reads a name ignoring case, or the enum's number, as `JsonDefaults`'
    /// `JsonStringEnumConverter` does. Anything else, `null` included, reads
    /// as [`ProfileImageFormat::Default`], so one bad value cannot make the
    /// whole configuration unreadable.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(name) => match name.to_ascii_lowercase().as_str() {
                "binary" => Self::Binary,
                "base64" => Self::Base64,
                "url" => Self::Url,
                _ => Self::Default,
            },
            serde_json::Value::Number(number) => match number.as_u64() {
                Some(1) => Self::Binary,
                Some(2) => Self::Base64,
                Some(3) => Self::Url,
                _ => Self::Default,
            },
            _ => Self::Default,
        })
    }
}

/// Reads a JSON `null` as the type's empty value (`""`, `false`, `0`), the way
/// upstream's C# properties behave for every check that reads them
/// (`string.IsNullOrEmpty`). A key that is absent keeps the container's
/// default, upstream's constructor value.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The plugin configuration — upstream `PluginConfiguration`, stored as
/// `{plugins}/958aad66-…/config.json` and edited by the settings page through
/// `GET`/`POST /Plugins/{id}/Configuration`.
///
/// Every member upstream has is here, PascalCase, except `LdapUsers` (D12).
/// The defaults are upstream's constructor values, except
/// [`ldap_server`](Self::ldap_server), which is empty (D3). The bind password
/// is kept in clear, as upstream's XML keeps it: only an administrator can read
/// the configuration. `Debug` leaves it out.
#[allow(clippy::struct_excessive_bools)] // one field per upstream property
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LdapConfig {
    /// The directory server's host name or address; empty until configured.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_server: String,
    /// The directory server's port.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_port: i32,
    /// Connect over TLS from the start (LDAPS).
    #[serde(deserialize_with = "null_as_empty")]
    pub use_ssl: bool,
    /// Upgrade the connection with StartTLS.
    #[serde(deserialize_with = "null_as_empty")]
    pub use_start_tls: bool,
    /// Accept any server certificate.
    #[serde(deserialize_with = "null_as_empty")]
    pub skip_ssl_verify: bool,
    /// The DN the server binds as to search the directory.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_bind_user: String,
    /// The bind DN's password; an empty one binds anonymously.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_bind_password: Secret,
    /// The DN user searches start from.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_base_dn: String,
    /// The filter a directory entry must match to sign in.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_search_filter: String,
    /// The DN administrator searches start from; empty uses the base DN.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_admin_base_dn: String,
    /// The filter that makes a user an administrator.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_admin_filter: String,
    /// Whether any entry the admin filter finds makes the user an
    /// administrator (a `memberUid` filter), not only the user's own entry.
    #[serde(deserialize_with = "null_as_empty")]
    pub enable_ldap_admin_filter_member_uid: bool,
    /// The comma-separated attributes a sign-in name is looked up by.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_search_attributes: String,
    /// A PEM file holding the client certificate to present.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_client_cert_path: String,
    /// A PEM file holding the client certificate's private key.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_client_key_path: String,
    /// A PEM file holding the root CA the server's certificate must chain to.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_root_ca_path: String,
    /// Whether a first sign-in creates the Ferrofin user.
    #[serde(deserialize_with = "null_as_empty")]
    pub create_users_from_ldap: bool,
    /// Whether a password change is written to the directory.
    #[serde(deserialize_with = "null_as_empty")]
    pub allow_pass_change: bool,
    /// The attribute holding a user's stable LDAP uid.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_uid_attribute: String,
    /// The attribute holding a user's Ferrofin name.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_username_attribute: String,
    /// The attribute a password change writes when the server lacks the
    /// password-modify operation.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_password_attribute: String,
    /// Whether users' pictures come from the directory.
    #[serde(deserialize_with = "null_as_empty")]
    pub enable_ldap_profile_image_sync: bool,
    /// Whether a picture the directory no longer has is removed.
    #[serde(deserialize_with = "null_as_empty")]
    pub remove_images_not_in_ldap: bool,
    /// The attribute holding a user's picture.
    #[serde(deserialize_with = "null_as_empty")]
    pub ldap_profile_image_attribute: String,
    /// The format of [`ldap_profile_image_attribute`](Self::ldap_profile_image_attribute).
    pub ldap_profile_image_format: ProfileImageFormat,
    /// Whether a created user may see every library.
    #[serde(deserialize_with = "null_as_empty")]
    pub enable_all_folders: bool,
    /// The libraries a created user may see otherwise.
    #[serde(deserialize_with = "null_as_empty")]
    pub enabled_folders: Vec<String>,
    /// Where a user is sent to reset a forgotten password; upstream leaves it
    /// null. Stored as `null` rather than left out, so the plugin manager's
    /// schema (the seed configuration) has the key.
    pub password_reset_url: Option<String>,
}

impl Default for LdapConfig {
    /// Upstream's `PluginConfiguration()` values, with an empty server (D3).
    fn default() -> Self {
        Self {
            ldap_server: String::new(),
            ldap_port: 389,
            use_ssl: true,
            use_start_tls: false,
            skip_ssl_verify: false,
            ldap_bind_user: "CN=BindUser,DC=contoso,DC=com".to_owned(),
            ldap_bind_password: Secret::new("password"),
            ldap_base_dn: "o=domains,dc=contoso,dc=com".to_owned(),
            ldap_search_filter: "(memberOf=CN=JellyfinUsers,DC=contoso,DC=com)".to_owned(),
            ldap_admin_base_dn: String::new(),
            ldap_admin_filter: "(enabledService=JellyfinAdministrator)".to_owned(),
            enable_ldap_admin_filter_member_uid: false,
            ldap_search_attributes: "uid, cn, mail, displayName".to_owned(),
            ldap_client_cert_path: String::new(),
            ldap_client_key_path: String::new(),
            ldap_root_ca_path: String::new(),
            create_users_from_ldap: true,
            allow_pass_change: false,
            ldap_uid_attribute: "uid".to_owned(),
            ldap_username_attribute: "cn".to_owned(),
            ldap_password_attribute: "userPassword".to_owned(),
            enable_ldap_profile_image_sync: false,
            remove_images_not_in_ldap: false,
            ldap_profile_image_attribute: "jpegphoto".to_owned(),
            ldap_profile_image_format: ProfileImageFormat::Default,
            enable_all_folders: false,
            enabled_folders: Vec::new(),
            password_reset_url: None,
        }
    }
}

impl LdapConfig {
    /// Writes the connection test's settings into the configuration, as
    /// `LdapController.TestServerBind` does before testing. A member the body
    /// left out takes upstream `ServerConnectionInfo`'s default.
    pub fn apply_connection(&mut self, connection: ServerConnectionInfo) {
        self.ldap_server = connection.ldap_server.unwrap_or_default();
        self.ldap_port = connection.ldap_port;
        self.use_ssl = connection.use_ssl;
        self.use_start_tls = connection.use_start_tls;
        self.skip_ssl_verify = connection.skip_ssl_verify;
        self.allow_pass_change = connection.allow_pass_change;
        self.ldap_bind_user = connection.ldap_bind_user.unwrap_or_default();
        self.ldap_bind_password = connection.ldap_bind_password.unwrap_or_default();
        self.ldap_base_dn = connection.ldap_base_dn.unwrap_or_default();
        self.password_reset_url = connection.password_reset_url;
        self.ldap_client_cert_path = connection.ldap_client_cert_path.unwrap_or_default();
        self.ldap_client_key_path = connection.ldap_client_key_path.unwrap_or_default();
        self.ldap_root_ca_path = connection.ldap_root_ca_path.unwrap_or_default();
    }

    /// Writes the filter test's filters into the configuration, as
    /// `LdapController.TestLdapFilters` does before testing. A filter the body
    /// left out is saved empty; a member-uid flag it left out keeps the saved
    /// one (see the module's divergences).
    pub fn apply_filters(&mut self, filters: UserFilterInfo) {
        self.ldap_search_filter = filters.ldap_search_filter.unwrap_or_default();
        self.ldap_admin_filter = filters.ldap_admin_filter.unwrap_or_default();
        if let Some(member_uid) = filters.enable_ldap_admin_filter_member_uid {
            self.enable_ldap_admin_filter_member_uid = member_uid;
        }
    }

    /// Whether the admin filter is on: set, and not [`ADMIN_FILTER_DISABLED`]
    /// (upstream's `!string.IsNullOrEmpty(AdminFilter) && AdminFilter !=
    /// "_disabled_"`, an ordinal compare).
    #[must_use]
    pub fn admin_filter_enabled(&self) -> bool {
        !self.ldap_admin_filter.is_empty() && self.ldap_admin_filter != ADMIN_FILTER_DISABLED
    }

    /// Where the admin filter searches at sign-in: `LdapAdminBaseDn`, or the
    /// base DN while that is empty.
    #[must_use]
    pub fn admin_base_dn(&self) -> &str {
        if self.ldap_admin_base_dn.is_empty() {
            &self.ldap_base_dn
        } else {
            &self.ldap_admin_base_dn
        }
    }
}

/// How a TLS connection checks the server's certificate (upstream
/// `GetConnectionOptions`' certificate callbacks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateVerification {
    /// The system's trusted roots and the server's name: .NET's default.
    System,
    /// Only the roots in this PEM file, and the server's name
    /// (`LdapClient_UserDefinedServerCertValidationDelegate`).
    CustomRoot(PathBuf),
    /// Any certificate (`LdapClient_IgnoreCertDelegate`).
    None,
}

/// The client certificate a TLS connection presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCertificate {
    /// A PEM file holding the certificate, leaf first.
    pub certificate: PathBuf,
    /// A PEM file holding its private key.
    pub key: PathBuf,
}

/// Where and how to connect to the directory: upstream `GetConnectionOptions`
/// plus the server and port `ConnectToLdap` passes to `Connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOptions {
    /// The server's host name or address.
    pub server: String,
    /// The server's port, as configured; one outside `0..=65535` fails to
    /// connect.
    pub port: i32,
    /// Connect over TLS from the start (LDAPS).
    pub use_ssl: bool,
    /// Upgrade with StartTLS once connected.
    pub use_start_tls: bool,
    /// How the server's certificate is checked.
    pub verification: CertificateVerification,
    /// The client certificate to present, when both of its files are set.
    pub client_certificate: Option<ClientCertificate>,
}

impl ConnectionOptions {
    /// The options `config` describes. `SkipSslVerify` wins over a root CA,
    /// and a client certificate needs both files, as upstream's
    /// `GetConnectionOptions` has it.
    #[must_use]
    pub fn from_config(config: &LdapConfig) -> Self {
        let verification = if config.skip_ssl_verify {
            CertificateVerification::None
        } else if config.ldap_root_ca_path.is_empty() {
            CertificateVerification::System
        } else {
            CertificateVerification::CustomRoot(PathBuf::from(&config.ldap_root_ca_path))
        };
        let client_certificate = (!config.ldap_client_cert_path.is_empty()
            && !config.ldap_client_key_path.is_empty())
        .then(|| ClientCertificate {
            certificate: PathBuf::from(&config.ldap_client_cert_path),
            key: PathBuf::from(&config.ldap_client_key_path),
        });
        Self {
            server: config.ldap_server.clone(),
            port: config.ldap_port,
            use_ssl: config.use_ssl,
            use_start_tls: config.use_start_tls,
            verification,
            client_certificate,
        }
    }
}

/// A search's scope (RFC 4511 section 4.5.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// The base entry alone.
    Base,
    /// The entries directly below the base.
    OneLevel,
    /// The base and everything below it.
    Subtree,
}

/// One attribute of a directory entry, with its raw values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapAttribute {
    /// The attribute's name, as the server spelled it.
    pub name: String,
    /// Its values, as the server sent them (text is UTF-8).
    pub values: Vec<Vec<u8>>,
}

/// A directory entry a search returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapEntry {
    /// The entry's DN.
    pub dn: String,
    /// The attributes the search asked for that the entry has.
    pub attributes: Vec<LdapAttribute>,
}

impl LdapEntry {
    /// The attribute named `name`, ignoring case, as LDAP attribute
    /// descriptions (and Novell's `LdapAttributeSet`) do.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&LdapAttribute> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
    }
}

/// What a search found on one server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchOutcome {
    /// The entries, in the order the server sent them.
    pub entries: Vec<LdapEntry>,
    /// The search references (continuations) the server sent, in order: each
    /// names, by its URLs, a part of the tree another server holds, and its
    /// URLs are alternatives of each other (RFC 4511 section 4.5.3). The
    /// connection does not follow them.
    pub references: Vec<Vec<String>>,
}

/// The OID of the RFC 3062 password modify extended operation, which a root
/// DSE lists in `supportedExtension` when the server offers it (Novell's
/// `LdapKnownOids.Extensions.ModifyPassword`).
pub const PASSWORD_MODIFY_OID: &str = "1.3.6.1.4.1.4203.1.11.1";

/// The LDAP result code a client reports for a filter it cannot parse
/// (`filterError`, RFC 1823), as Novell's `LdapLocalException` does.
pub const FILTER_ERROR: u32 = 87;

/// Why a directory call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LdapError {
    /// The connection could not be opened: the name did not resolve, the TCP
    /// connect failed or took longer than [`CONNECT_TIMEOUT`], the LDAPS
    /// handshake failed, or a certificate file could not be read.
    #[error("{0}")]
    Connect(String),
    /// The connection opened but StartTLS failed: the server refused it or the
    /// TLS handshake after it failed.
    #[error("{0}")]
    StartTls(String),
    /// An operation ended with a result code other than success, from the
    /// server or, for a filter that does not parse, from the client
    /// ([`FILTER_ERROR`]).
    #[error("{}", describe_result(*code, message))]
    Result {
        /// The LDAP result code.
        code: u32,
        /// The server's diagnostic message, often empty.
        message: String,
    },
    /// The server did not answer within [`OPERATION_TIMEOUT`].
    #[error("the directory did not answer within {} seconds", OPERATION_TIMEOUT.as_secs())]
    Timeout,
    /// A search sent search references (continuations to other servers)
    /// where they are not followed: the connection test's base search, as
    /// upstream's. The message is Novell's.
    #[error("Search result reference received, and referral following is off")]
    Referral(Vec<String>),
    /// The server referred the whole search elsewhere (a `referral` result,
    /// code 10), to any one of `urls`. It reads as that result.
    #[error("{}", describe_result(REFERRAL, message))]
    Referred {
        /// The URLs the referral names, alternatives of each other.
        urls: Vec<String>,
        /// The server's diagnostic message, often empty.
        message: String,
    },
    /// A referral could not be followed: the server it named failed the
    /// search, which fails it; or, in the warning for a skipped referral, a
    /// URL of it could not be connected to and bound at.
    #[error("the referral to {server} failed: {reason}")]
    ReferralFailed {
        /// The referred server, `host:port`.
        server: String,
        /// Why it failed.
        reason: Box<LdapError>,
    },
    /// A referral names no URL, which fails the search; or, in the warning
    /// for a skipped referral, a URL of it cannot be followed (it is not an
    /// `ldap://` or `ldaps://` URL, or does not parse). The reason, not the
    /// URL, which may carry a password.
    #[error("the directory referred the search to a URL that cannot be followed: {0}")]
    ReferralUrl(String),
    /// A referred server referred the search on once more after
    /// [`REFERRAL_HOP_LIMIT`] referrals in a row.
    #[error(
        "the search was referred more than {} times in a row, the last time by {server}: the \
         directory may refer in a loop",
        REFERRAL_HOP_LIMIT
    )]
    ReferralHopLimit {
        /// The server that sent the referral past the limit, `host:port`.
        server: String,
    },
    /// A search met more than [`REFERRAL_BUDGET`] referrals in all.
    #[error(
        "the search was referred more than {} times in all, the last time by {server}: no \
         search follows more referrals than that",
        REFERRAL_BUDGET
    )]
    ReferralBudget {
        /// The server that sent the referral past the budget, `host:port`.
        server: String,
    },
    /// The connection failed during an operation.
    #[error("{0}")]
    Connection(String),
}

/// RFC 4511's name for a result code, spelled as Novell's client reports it.
fn result_name(code: u32) -> Option<&'static str> {
    Some(match code {
        0 => "Success",
        1 => "Operations Error",
        2 => "Protocol Error",
        3 => "Time Limit Exceeded",
        4 => "Size Limit Exceeded",
        5 => "Compare False",
        6 => "Compare True",
        7 => "Authentication Method Not Supported",
        8 => "Strong Authentication Required",
        REFERRAL => "Referral",
        11 => "Administrative Limit Exceeded",
        12 => "Unavailable Critical Extension",
        13 => "Confidentiality Required",
        14 => "SASL Bind In Progress",
        16 => "No Such Attribute",
        17 => "Undefined Attribute Type",
        18 => "Inappropriate Matching",
        19 => "Constraint Violation",
        20 => "Attribute Or Value Exists",
        21 => "Invalid Attribute Syntax",
        32 => "No Such Object",
        33 => "Alias Problem",
        34 => "Invalid DN Syntax",
        36 => "Alias Dereferencing Problem",
        48 => "Inappropriate Authentication",
        49 => "Invalid Credentials",
        50 => "Insufficient Access Rights",
        51 => "Busy",
        52 => "Unavailable",
        53 => "Unwilling To Perform",
        54 => "Loop Detect",
        64 => "Naming Violation",
        65 => "Object Class Violation",
        66 => "Not Allowed On Non-leaf",
        67 => "Not Allowed On RDN",
        68 => "Entry Already Exists",
        69 => "Object Class Modifications Prohibited",
        71 => "Affects Multiple DSAs",
        80 => "Other",
        FILTER_ERROR => "Filter Error",
        _ => return None,
    })
}

/// A result code and the server's diagnostic as one message:
/// `Invalid Credentials`, or `Invalid Credentials: <diagnostic>`.
fn describe_result(code: u32, message: &str) -> String {
    let name = result_name(code).map_or_else(|| format!("LDAP result {code}"), str::to_owned);
    if message.trim().is_empty() {
        name
    } else {
        format!("{name}: {}", message.trim())
    }
}

/// Opens connections to a directory server: the seam the extension's
/// directory work goes through, faked in unit tests.
#[async_trait]
pub trait LdapDirectory: Send + Sync {
    /// Connects as `options` say: TCP (and the TLS handshake for LDAPS) within
    /// [`CONNECT_TIMEOUT`], then StartTLS when asked for. The error says which
    /// of the two failed ([`LdapError::Connect`] or [`LdapError::StartTls`]).
    async fn connect(
        &self,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn LdapConnection>, LdapError>;
}

fn _assert_object_safe_ldap_directory(_: &dyn LdapDirectory) {}

/// An open directory connection. Every operation gives up after
/// [`OPERATION_TIMEOUT`] without an answer.
#[async_trait]
pub trait LdapConnection: Send {
    /// A simple bind. An empty `password` is sent as it is, which a server may
    /// treat as an unauthenticated bind and accept (RFC 4513 section 5.1.2):
    /// callers decide what an empty password means before they get here.
    async fn simple_bind(&mut self, dn: &str, password: &str) -> Result<(), LdapError>;

    /// Searches `base` with `filter`, returning the `attributes` asked for
    /// (none named returns every user attribute) and the search references the
    /// server sent, which it does not follow. A `referral` result naming URLs
    /// is an [`LdapError::Referred`]; any other result than success is an
    /// [`LdapError::Result`].
    async fn search(
        &mut self,
        base: &str,
        scope: SearchScope,
        filter: &str,
        attributes: &[&str],
    ) -> Result<SearchOutcome, LdapError>;

    /// Replaces every value of `attribute` on the entry `dn` with `values`.
    async fn modify_replace(
        &mut self,
        dn: &str,
        attribute: &str,
        values: &[&str],
    ) -> Result<(), LdapError>;

    /// Sets the password of the entry `user_dn` with the RFC 3062 password
    /// modify extended operation, without the old password.
    async fn password_modify(&mut self, user_dn: &str, new_password: &str)
    -> Result<(), LdapError>;

    /// The extended operations the server's root DSE lists
    /// (`supportedExtension`), by OID.
    async fn supported_extensions(&mut self) -> Result<Vec<String>, LdapError>;

    /// Ends the session. Failures are not reported: the connection is being
    /// dropped either way.
    async fn unbind(&mut self);
}

fn _assert_object_safe_ldap_connection(_: &dyn LdapConnection) {}

/// The connection test — port of
/// `LdapAuthenticationProviderPlugin.TestServerBind`: connect (and StartTLS
/// when configured), bind with the configured bind DN, then count the entries
/// under the base DN. The first step that fails keeps reading `Testing...`
/// and [`ServerTestResponse::error`] says why; a later step is absent.
pub async fn test_server_bind(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
) -> ServerTestResponse {
    let mut response = ServerTestResponse::default();
    if let Err(error) = run_server_test(directory, config, &mut response).await {
        tracing::warn!(error = ?error.to_string(), "LDAP connection test failed");
        response.error = Some(error.to_string());
    }
    response
}

/// The steps of [`test_server_bind`], recording each in `response` as it
/// starts and ends.
async fn run_server_test(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    response: &mut ServerTestResponse,
) -> Result<(), LdapError> {
    let options = ConnectionOptions::from_config(config);
    response.connect = Some(TESTING.to_owned());
    let mut connection = match directory.connect(&options).await {
        Ok(connection) => connection,
        Err(error @ LdapError::StartTls(_)) => {
            response.connect = Some(SUCCESS.to_owned());
            response.start_tls = Some(TESTING.to_owned());
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    response.connect = Some(SUCCESS.to_owned());
    if options.use_start_tls {
        response.start_tls = Some(SUCCESS.to_owned());
    }
    let outcome = bind_and_count(connection.as_mut(), config, response).await;
    connection.unbind().await;
    outcome
}

/// Why stored bytes are not a configuration, without the value that failed:
/// a type mismatch's message quotes the value (``invalid type: integer
/// `12345` ``), which for `LdapBindPassword` is the bind password (LOGGING.md,
/// rule 5). Only the kind of error is kept, with its position in the
/// configuration as the plugin manager serves it (compact JSON, so not the
/// file's own lines).
pub(crate) fn config_error(error: &serde_json::Error) -> String {
    format!(
        "{:?} error at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

/// A bind as Novell's `LdapConnection.Bind(dn, password)` sends it: the DN
/// trimmed, and an empty password an anonymous bind with an empty DN, after
/// which Novell reports `Bound` false.
pub(crate) async fn novell_bind(
    connection: &mut dyn LdapConnection,
    dn: &str,
    password: &str,
) -> Result<(), LdapError> {
    if password.is_empty() {
        connection.simple_bind("", "").await
    } else {
        let dn = dn.trim();
        tracing::debug!(bind_dn = ?dn, "LDAP bind");
        connection.simple_bind(dn, password).await
    }
}

/// Why a search as the configured bind DN failed, by the step that failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BoundSearchError {
    /// The connection could not be opened, or StartTLS failed.
    #[error("{0}")]
    Connect(LdapError),
    /// The directory refused the configured bind DN.
    #[error("{0}")]
    Bind(LdapError),
    /// The search failed.
    #[error("{0}")]
    Search(LdapError),
}

/// `ConnectToLdap()` and a search on it with upstream's `GetSearchConstraints`,
/// as the plugin searches: a connection of its own, bound as the configured
/// bind DN, searching `base`'s subtree with `filter` for `attributes`, and
/// closed again. The referrals the search meets are then followed with the
/// bind credentials (`referrals`), and the entries of every server that
/// answered are returned, the configured server's first.
pub(crate) async fn search_as_bind_dn(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    base: &str,
    filter: &str,
    attributes: &[&str],
) -> Result<Vec<LdapEntry>, BoundSearchError> {
    let options = ConnectionOptions::from_config(config);
    let mut connection = directory
        .connect(&options)
        .await
        .map_err(BoundSearchError::Connect)?;
    let answer = bind_and_search(connection.as_mut(), config, base, filter, attributes).await;
    connection.unbind().await;
    let search = referrals::Search {
        base: base.to_owned(),
        scope: SearchScope::Subtree,
        filter: filter.to_owned(),
    };
    referrals::follow(directory, config, &options, &search, attributes, answer?)
        .await
        .map_err(BoundSearchError::Search)
}

/// The bind and the search of [`search_as_bind_dn`]: what the search
/// answered, a referral included, once the bind succeeded.
async fn bind_and_search(
    connection: &mut dyn LdapConnection,
    config: &LdapConfig,
    base: &str,
    filter: &str,
    attributes: &[&str],
) -> Result<Result<SearchOutcome, LdapError>, BoundSearchError> {
    novell_bind(
        connection,
        &config.ldap_bind_user,
        config.ldap_bind_password.expose(),
    )
    .await
    .map_err(BoundSearchError::Bind)?;
    Ok(connection
        .search(base, SearchScope::Subtree, filter, attributes)
        .await)
}

/// The bind and base-search steps of the connection test.
async fn bind_and_count(
    connection: &mut dyn LdapConnection,
    config: &LdapConfig,
    response: &mut ServerTestResponse,
) -> Result<(), LdapError> {
    response.bind = Some(TESTING.to_owned());
    let password = config.ldap_bind_password.expose();
    novell_bind(connection, &config.ldap_bind_user, password).await?;
    response.bind = Some(
        if password.is_empty() {
            ANONYMOUS
        } else {
            SUCCESS
        }
        .to_owned(),
    );

    response.base_search = Some(TESTING.to_owned());
    let found = connection
        .search(
            &config.ldap_base_dn,
            SearchScope::Subtree,
            EMPTY_FILTER,
            &[NO_ATTRIBUTES],
        )
        .await?;
    // Upstream's base search sets no search constraints, so it does not
    // follow referrals, and Novell's result iterator throws on the first
    // search reference it reaches.
    if !found.references.is_empty() {
        return Err(LdapError::Referral(
            found.references.into_iter().flatten().collect(),
        ));
    }
    response.base_search = Some(format!("Found {} Entities", found.entries.len()));
    Ok(())
}

/// `GetFilteredUsers`: the DNs of the entries `filter` finds in the base DN's
/// subtree, searching as the configured bind DN and following referrals. An
/// empty filter is Novell's `(objectclass=*)`.
async fn filtered_users(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    filter: &str,
) -> Result<HashSet<String>, BoundSearchError> {
    let filter = if filter.is_empty() {
        EMPTY_FILTER
    } else {
        filter
    };
    tracing::debug!(base_dn = ?config.ldap_base_dn, filter = ?filter, "LDAP filter search");
    let found = search_as_bind_dn(
        directory,
        config,
        &config.ldap_base_dn,
        filter,
        &[NO_ATTRIBUTES],
    )
    .await?;
    Ok(found.into_iter().map(|entry| entry.dn).collect())
}

/// The filter test — port of `LdapController.TestLdapFilters` once the body is
/// saved: the distinct entries `LdapSearchFilter` finds, then, while the admin
/// filter is on, those `LdapAdminFilter` finds, and whether the second are all
/// among the first.
///
/// As upstream, both filters are used as written (a `{username}` in them is
/// not replaced) and both search the base DN, the admin filter included. A
/// directory that cannot be reached, or refuses the bind DN, is a `401` with
/// upstream's message; a search the directory fails, the filter's included, is
/// a `400` naming the filter.
pub async fn test_ldap_filters(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
) -> LdapTestAnswer<LdapFilterResponse> {
    let users = match filtered_users(directory, config, &config.ldap_search_filter).await {
        Ok(users) => users,
        Err(error) => return filter_test_failure("user", USER_FILTER_LABEL, &error),
    };
    let admins = if config.admin_filter_enabled() {
        match filtered_users(directory, config, &config.ldap_admin_filter).await {
            Ok(admins) => admins,
            Err(error) => return filter_test_failure("admin", ADMIN_FILTER_LABEL, &error),
        }
    } else {
        HashSet::new()
    };
    LdapTestAnswer::Ok(LdapFilterResponse {
        users: users.len(),
        admins: admins.len(),
        is_subset: admins.is_subset(&users),
    })
}

/// The filter test's answer when the search of the `filter` filter (`user` or
/// `admin`) failed; `label` begins a `400`'s message.
fn filter_test_failure<T>(
    filter: &str,
    label: &str,
    error: &BoundSearchError,
) -> LdapTestAnswer<T> {
    tracing::warn!(
        filter,
        error = ?error.to_string(),
        "LDAP filter test failed"
    );
    match error {
        BoundSearchError::Connect(_) | BoundSearchError::Bind(_) => {
            LdapTestAnswer::Unauthorized(LdapTestErrorResponse::new(CONNECT_FAILED))
        }
        BoundSearchError::Search(error) => {
            LdapTestAnswer::BadRequest(LdapTestErrorResponse::new(format!("{label}{error}")))
        }
    }
}

/// The user search — port of `LdapController.LdapUserSearch` once the body is
/// saved, for a name that is not empty: the DN of the entry `username` signs
/// in as (`LocateLdapUser`, the search sign-in runs).
///
/// Every failure is a `401` with upstream's message: a directory that cannot
/// be reached or refuses the bind DN, a search the directory fails, and no
/// matching entry.
pub async fn ldap_user_search(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    username: &str,
) -> LdapTestAnswer<UserSearchResponse> {
    let message = match provider::locate_ldap_user(directory, config, username).await {
        Ok(Some(entry)) => {
            return LdapTestAnswer::Ok(UserSearchResponse {
                located_dn: Some(entry.dn),
            });
        }
        Ok(None) => provider::NO_MATCH,
        Err(error) => {
            tracing::warn!(error = ?error.to_string(), "LDAP user search failed");
            match error {
                BoundSearchError::Connect(_) | BoundSearchError::Bind(_) => CONNECT_FAILED,
                BoundSearchError::Search(_) => USER_FILTER_FAILED,
            }
        }
    };
    LdapTestAnswer::Unauthorized(LdapTestErrorResponse::new(message))
}

/// The LDAP-Auth plugin's service: reads and saves its configuration through
/// the plugin manager and runs the settings page's directory tests. Shared by
/// the `/Ldap/*` routes through [`LdapManager`].
pub struct LdapAuthService {
    plugins: Arc<dyn PluginManager>,
    directory: Arc<dyn LdapDirectory>,
    /// Serialises the routes' read-modify-write of the configuration, so two
    /// tests saved at once cannot lose each other's fields.
    config_lock: tokio::sync::Mutex<()>,
}

impl LdapAuthService {
    /// A service over the plugin manager (the enabled flag and the
    /// configuration) and the directory it connects through.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>, directory: Arc<dyn LdapDirectory>) -> Self {
        Self {
            plugins,
            directory,
            config_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The plugin's current configuration.
    ///
    /// # Errors
    ///
    /// When the plugin manager cannot read it, or what it holds is not a
    /// configuration.
    pub async fn config(&self) -> Result<LdapConfig, ServiceError> {
        let bytes = self.plugins.get_plugin_configuration(EXTENSION_ID).await?;
        serde_json::from_slice(&bytes).map_err(|e| {
            ServiceError::backend(format!(
                "read the LDAP-Auth configuration: {}",
                config_error(&e)
            ))
        })
    }

    /// Saves `config` as the plugin's configuration.
    async fn save_config(&self, config: &LdapConfig) -> Result<(), ServiceError> {
        let bytes = serde_json::to_vec(config).map_err(|e| {
            ServiceError::backend(format!("serialize the LDAP-Auth configuration: {e}"))
        })?;
        self.plugins
            .set_plugin_configuration(EXTENSION_ID, bytes)
            .await
    }

    /// Reads the configuration, changes it with `apply` and saves it, as each
    /// route does with its body before testing (`LdapPlugin.Instance
    /// .UpdateConfiguration`), and returns what was saved. Holds the lock
    /// that keeps two saves from losing each other's fields.
    async fn update_config(
        &self,
        apply: impl FnOnce(&mut LdapConfig) + Send,
    ) -> Result<LdapConfig, ServiceError> {
        let _saving = self.config_lock.lock().await;
        let mut config = self.config().await?;
        apply(&mut config);
        self.save_config(&config).await?;
        Ok(config)
    }
}

#[async_trait]
impl LdapManager for LdapAuthService {
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        match self.plugins.get_plugin(EXTENSION_ID).await? {
            Some(plugin) if plugin.enabled => Ok(()),
            _ => Err(ServiceError::not_found("the LDAP-Auth plugin is disabled")),
        }
    }

    async fn test_server_bind(
        &self,
        connection: ServerConnectionInfo,
    ) -> Result<ServerTestResponse, ServiceError> {
        self.ensure_enabled().await?;
        let config = self
            .update_config(|config| config.apply_connection(connection))
            .await?;
        Ok(test_server_bind(self.directory.as_ref(), &config).await)
    }

    async fn test_ldap_filters(
        &self,
        filters: UserFilterInfo,
    ) -> Result<LdapTestAnswer<LdapFilterResponse>, ServiceError> {
        self.ensure_enabled().await?;
        let config = self
            .update_config(|config| config.apply_filters(filters))
            .await?;
        Ok(test_ldap_filters(self.directory.as_ref(), &config).await)
    }

    async fn ldap_user_search(
        &self,
        search: UserSearchAttributes,
    ) -> Result<LdapTestAnswer<UserSearchResponse>, ServiceError> {
        self.ensure_enabled().await?;
        let UserSearchAttributes {
            ldap_search_attributes,
            test_search_username,
        } = search;
        let config = self
            .update_config(|config| {
                config.ldap_search_attributes = ldap_search_attributes.unwrap_or_default();
            })
            .await?;
        // Upstream answers an empty name with an empty response, saving only.
        let Some(username) = test_search_username.filter(|name| !name.is_empty()) else {
            return Ok(LdapTestAnswer::Ok(UserSearchResponse::default()));
        };
        Ok(ldap_user_search(self.directory.as_ref(), &config, &username).await)
    }
}

/// The `/Plugins` surface of the LDAP-Auth extension.
#[derive(Debug, Default, Clone, Copy)]
pub struct LdapAuthExtension;

impl LdapAuthExtension {
    /// The provider over the context's plugin manager, users, identity links
    /// and image fetcher, and a real directory client.
    fn provider(cx: &ExtensionContext) -> provider::LdapAuthProvider {
        provider::LdapAuthProvider::new(
            Arc::clone(&cx.plugins),
            Arc::clone(&cx.users),
            Arc::clone(&cx.external_identities),
            Arc::new(directory::Ldap3Directory::default()),
            Arc::clone(&cx.remote_images),
        )
    }
}

impl Extension for LdapAuthExtension {
    fn id(&self) -> Uuid {
        EXTENSION_ID
    }

    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: EXTENSION_ID,
            name: "LDAP-Auth".to_owned(),
            // The upstream plugin version the port (and its vendored settings
            // page) tracks: v24's assembly version.
            version: "24.0.0.0".to_owned(),
            description: "Authenticate users against an LDAP database.".to_owned(),
            // Off until an administrator enables it (D3).
            enabled: false,
            has_image: false,
            can_uninstall: false,
            // Upstream's `BasePlugin<T>.ConfigurationFileName`: the assembly
            // name with `.xml`.
            configuration_file_name: Some(adoption::CONFIGURATION_FILE_NAME.to_owned()),
        }
    }

    fn default_config(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&LdapConfig::default()).unwrap_or_else(|_| b"{}".to_vec())
    }

    fn config_pages(&self) -> Vec<PluginConfigPage> {
        // The upstream plugin's own settings page (vendored by `build.rs`),
        // named after the plugin, as upstream's `GetPages` does.
        vec![PluginConfigPage {
            name: "LDAP-Auth".to_owned(),
            bytes: include_bytes!(concat!(env!("OUT_DIR"), "/ldapauth/configPage.html")).to_vec(),
            enable_in_main_menu: false,
        }]
    }

    /// The sign-in provider, over the context's plugin manager, users and
    /// identity links and a real directory client. It gates itself on the
    /// plugin's enabled flag and a configured server.
    fn auth_providers(&self, cx: &ExtensionContext) -> Vec<Arc<dyn AuthenticationManager>> {
        vec![Arc::new(Self::provider(cx))]
    }

    /// The forgotten-password provider: the same type as the sign-in one, as
    /// upstream's class is both, gated the same way.
    fn password_reset_providers(
        &self,
        cx: &ExtensionContext,
    ) -> Vec<Arc<dyn PasswordResetManager>> {
        vec![Arc::new(Self::provider(cx))]
    }

    /// `LdapProfileImageSyncTask`, gated like the provider and on
    /// `EnableLdapProfileImageSync`.
    fn tasks(&self, cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
        vec![Arc::new(
            provider::profile_image::ProfileImageSyncTask::new(Arc::new(Self::provider(cx))),
        )]
    }
}

#[cfg(test)]
mod server_bind_tests;
