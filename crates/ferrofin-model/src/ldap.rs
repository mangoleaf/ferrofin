//! The LDAP-Auth plugin's wire types: the bodies and answers of the
//! `POST /Ldap/*` routes the plugin's settings page calls when the
//! administrator presses one of its test buttons (the connection test, the
//! filter test and the user search).
//!
//! Ported from `jellyfin-plugin-ldapauth` v24 (`d11c86a`),
//! `LDAP-Auth/Api/Models/*.cs`. Not Jellyfin contract types: the routes are the
//! plugin's own controller. Their JSON is PascalCase, as the plugin's is.

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

use crate::secret::Secret;

/// The connection and bind settings the settings page tests, a subset of the
/// plugin configuration (upstream `ServerConnectionInfo`).
///
/// Every member is optional on the wire. A missing string reads as upstream's
/// property default (an empty string, or null for [`ldap_server`],
/// [`ldap_base_dn`] and [`password_reset_url`]), and a missing number or flag
/// as zero or `false`, as `System.Text.Json` leaves a C# property it does not
/// see. Upstream marks `LdapServer` and `LdapBaseDn` `[Required]`, which the
/// route checks before anything else. `Debug` leaves the bind password out.
///
/// [`ldap_server`]: Self::ldap_server
/// [`ldap_base_dn`]: Self::ldap_base_dn
/// [`password_reset_url`]: Self::password_reset_url
#[allow(clippy::struct_excessive_bools)] // one field per upstream property
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct ServerConnectionInfo {
    /// The directory server's host name or address. Required.
    pub ldap_server: Option<String>,
    /// The directory server's port.
    pub ldap_port: i32,
    /// Whether to connect over TLS from the start (LDAPS).
    pub use_ssl: bool,
    /// Whether to upgrade the connection with StartTLS.
    pub use_start_tls: bool,
    /// Whether to accept any server certificate.
    pub skip_ssl_verify: bool,
    /// Whether users may change their directory password through Ferrofin.
    pub allow_pass_change: bool,
    /// The DN to bind as.
    pub ldap_bind_user: Option<String>,
    /// The bind DN's password. An empty one binds anonymously.
    #[schema(value_type = Option<String>)]
    pub ldap_bind_password: Option<Secret>,
    /// The DN user searches start from. Required.
    pub ldap_base_dn: Option<String>,
    /// Where a user is sent to reset a forgotten password.
    pub password_reset_url: Option<String>,
    /// A PEM file holding the client certificate to present.
    pub ldap_client_cert_path: Option<String>,
    /// A PEM file holding the client certificate's private key.
    pub ldap_client_key_path: Option<String>,
    /// A PEM file holding the only root CA the server's certificate may chain to.
    pub ldap_root_ca_path: Option<String>,
}

impl ServerConnectionInfo {
    /// The members upstream marks `[Required]` that are absent, `null`,
    /// empty or white space, as `(member, message)` in ASP.NET's wording:
    /// `RequiredAttribute` refuses all four for a string.
    #[must_use]
    pub fn missing_required(&self) -> Vec<(String, String)> {
        [
            ("LdapServer", &self.ldap_server),
            ("LdapBaseDn", &self.ldap_base_dn),
        ]
        .into_iter()
        .filter(|(_, value)| value.as_deref().is_none_or(|v| v.trim().is_empty()))
        .map(|(member, _)| {
            (
                member.to_owned(),
                format!("The {member} field is required."),
            )
        })
        .collect()
    }
}

impl std::fmt::Debug for ServerConnectionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConnectionInfo")
            .field("ldap_server", &self.ldap_server)
            .field("ldap_port", &self.ldap_port)
            .field("use_ssl", &self.use_ssl)
            .field("use_start_tls", &self.use_start_tls)
            .field("skip_ssl_verify", &self.skip_ssl_verify)
            .field("allow_pass_change", &self.allow_pass_change)
            .field("ldap_bind_user", &self.ldap_bind_user)
            .field("ldap_bind_password", &self.ldap_bind_password)
            .field("ldap_base_dn", &self.ldap_base_dn)
            .field("password_reset_url", &self.password_reset_url)
            .field("ldap_client_cert_path", &self.ldap_client_cert_path)
            .field("ldap_client_key_path", &self.ldap_client_key_path)
            .field("ldap_root_ca_path", &self.ldap_root_ca_path)
            .finish()
    }
}

/// The outcome of each step of a connection test (upstream
/// `ServerTestResponse`).
///
/// A step that was never reached is absent. One that failed reads
/// `Testing...`, and [`error`](Self::error) says why: the settings page reads
/// the steps in order and stops at the first that still reads `Testing...`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerTestResponse {
    /// Opening the connection (and, for LDAPS, its TLS handshake).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect: Option<String>,
    /// The StartTLS upgrade, when the settings ask for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_tls: Option<String>,
    /// The bind: `Success`, or `Anonymous` when no bind password was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    /// The subtree search of the base DN: `Found {count} Entities`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_search: Option<String>,
    /// Why the step that still reads `Testing...` failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The filters the settings page's filter test saves and tries, a subset of
/// the plugin configuration (upstream `UserFilterInfo`).
///
/// A missing or `null` filter reads as `None`, as `System.Text.Json` leaves a
/// C# string it does not see.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserFilterInfo {
    /// The filter a directory entry must match to sign in.
    pub ldap_search_filter: Option<String>,
    /// The filter that makes a user an administrator; empty or `_disabled_`
    /// turns it off. The settings page sends `_disabled_` for an empty box.
    pub ldap_admin_filter: Option<String>,
    /// Whether any entry the admin filter finds makes the user an
    /// administrator (a `memberUid` filter), not only the user's own entry.
    ///
    /// `None` when the body leaves it out, which keeps the saved value.
    /// Upstream saves a missing member as `false`, and its own settings page
    /// never sends it, so pressing the filter test there turns member-uid
    /// mode off until the page is saved again: a Jellyfin bug not ported. A
    /// `null` does not bind, as a C# `bool` refuses it.
    #[serde(
        deserialize_with = "present_bool",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(nullable = false)]
    pub enable_ldap_admin_filter_member_uid: Option<bool>,
}

/// A member that binds only as a boolean, so that `null` is refused like a C#
/// `bool` refuses it; with `#[serde(default)]`, an absent member is `None`.
fn present_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

/// What the filter test found (upstream `LdapFilterResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LdapFilterResponse {
    /// How many distinct entries the user filter finds.
    pub users: usize,
    /// How many distinct entries the admin filter finds; `0` while it is off.
    pub admins: usize,
    /// Whether every entry the admin filter finds is one the user filter finds.
    pub is_subset: bool,
}

/// Why a settings-page test could not run, the body of its `400` or `401`
/// (upstream `LdapTestErrorResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LdapTestErrorResponse {
    /// What went wrong, as the settings page shows it after `Failure: `.
    pub message: String,
}

impl LdapTestErrorResponse {
    /// An answer carrying `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// The search attributes the settings page's user search saves, and the name
/// to look up with them (upstream `UserSearchAttributes`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserSearchAttributes {
    /// The comma-separated attributes a sign-in name is looked up by.
    pub ldap_search_attributes: Option<String>,
    /// The name to look up; when empty or absent, the attributes are only
    /// saved.
    pub test_search_username: Option<String>,
}

/// What the user search found (upstream `UserSearchResponse`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UserSearchResponse {
    /// The DN of the entry the name found; absent when no name was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub located_dn: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_answer_omits_the_steps_it_never_reached() {
        let answer = ServerTestResponse {
            connect: Some("Testing...".to_owned()),
            error: Some("connection refused".to_owned()),
            ..ServerTestResponse::default()
        };
        assert_eq!(
            serde_json::to_value(&answer).expect("json"),
            serde_json::json!({"Connect": "Testing...", "Error": "connection refused"})
        );
    }

    #[test]
    fn the_connection_settings_read_pascal_case_and_default_what_is_missing() {
        let info: ServerConnectionInfo = serde_json::from_str(
            r#"{"LdapServer":"ldap.example","LdapPort":636,"UseSsl":true,
                "LdapBindPassword":"hunter2","LdapBaseDn":"dc=example"}"#,
        )
        .expect("binds");
        assert_eq!(info.ldap_server.as_deref(), Some("ldap.example"));
        assert_eq!(info.ldap_port, 636);
        assert!(info.use_ssl && !info.use_start_tls);
        assert_eq!(info.ldap_bind_user, None);
        assert_eq!(
            info.ldap_bind_password.as_ref().map(Secret::expose),
            Some("hunter2")
        );
        let debug = format!("{info:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("ldap.example"), "{debug}");
        assert_eq!(info.missing_required(), Vec::new());
    }

    #[test]
    fn a_missing_empty_or_blank_required_member_is_reported_as_aspnet_does() {
        let info = ServerConnectionInfo {
            ldap_server: Some("  ".to_owned()),
            ldap_base_dn: None,
            ..ServerConnectionInfo::default()
        };
        assert_eq!(
            info.missing_required(),
            vec![
                (
                    "LdapServer".to_owned(),
                    "The LdapServer field is required.".to_owned()
                ),
                (
                    "LdapBaseDn".to_owned(),
                    "The LdapBaseDn field is required.".to_owned()
                ),
            ]
        );
        let empty_base = ServerConnectionInfo {
            ldap_server: Some("ldap.example".to_owned()),
            ldap_base_dn: Some(String::new()),
            ..ServerConnectionInfo::default()
        };
        assert_eq!(empty_base.missing_required().len(), 1);
    }

    #[test]
    fn the_filter_test_body_keeps_member_uid_mode_unless_it_is_sent() {
        // What upstream's settings page sends: no member-uid flag.
        let page: UserFilterInfo = serde_json::from_str(
            r#"{"LdapSearchFilter":"(objectClass=person)","LdapAdminFilter":"_disabled_"}"#,
        )
        .expect("binds");
        assert_eq!(
            page.ldap_search_filter.as_deref(),
            Some("(objectClass=person)")
        );
        assert_eq!(page.ldap_admin_filter.as_deref(), Some("_disabled_"));
        assert_eq!(page.enable_ldap_admin_filter_member_uid, None);

        let sent: UserFilterInfo = serde_json::from_str(
            r#"{"LdapSearchFilter":null,"EnableLdapAdminFilterMemberUid":true}"#,
        )
        .expect("binds");
        assert_eq!(sent.ldap_search_filter, None);
        assert_eq!(sent.enable_ldap_admin_filter_member_uid, Some(true));

        assert!(
            serde_json::from_str::<UserFilterInfo>(r#"{"EnableLdapAdminFilterMemberUid":null}"#)
                .is_err(),
            "a C# bool refuses null"
        );
    }

    #[test]
    fn the_member_uid_flag_is_documented_and_sent_as_a_plain_boolean() {
        use utoipa::PartialSchema;
        let schema = serde_json::to_value(UserFilterInfo::schema()).expect("schema");
        assert_eq!(
            schema["properties"]["EnableLdapAdminFilterMemberUid"]["type"], "boolean",
            "not nullable: {schema}"
        );
        assert_eq!(
            serde_json::to_value(UserFilterInfo::default()).expect("json"),
            serde_json::json!({"LdapSearchFilter": null, "LdapAdminFilter": null})
        );
    }

    #[test]
    fn the_filter_and_search_answers_are_upstreams_shapes() {
        let found = LdapFilterResponse {
            users: 2,
            admins: 1,
            is_subset: true,
        };
        assert_eq!(
            serde_json::to_value(&found).expect("json"),
            serde_json::json!({"Users": 2, "Admins": 1, "IsSubset": true})
        );
        assert_eq!(
            serde_json::to_value(LdapTestErrorResponse::new(
                "Failed to Connect or Bind to server."
            ))
            .expect("json"),
            serde_json::json!({"Message": "Failed to Connect or Bind to server."})
        );
        assert_eq!(
            serde_json::to_value(UserSearchResponse::default()).expect("json"),
            serde_json::json!({}),
            "no LocatedDn without a name, which the page reads as undefined"
        );
        assert_eq!(
            serde_json::to_value(UserSearchResponse {
                located_dn: Some("uid=ada,dc=example,dc=org".to_owned()),
            })
            .expect("json"),
            serde_json::json!({"LocatedDn": "uid=ada,dc=example,dc=org"})
        );
        let search: UserSearchAttributes =
            serde_json::from_str(r#"{"LdapSearchAttributes":"uid, mail"}"#).expect("binds");
        assert_eq!(search.ldap_search_attributes.as_deref(), Some("uid, mail"));
        assert_eq!(search.test_search_username, None);
    }
}
