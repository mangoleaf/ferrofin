//! Adopting a Jellyfin install that used LDAP-Auth (B7, D7): its settings and
//! uid map come across once from the plugin's `LDAP-Auth.xml`, and the plugin
//! is turned on at that boot when a user's saved authentication provider is
//! LDAP-Auth's, so those users sign in straight after the swap with no admin
//! action.
//!
//! Upstream keeps the configuration in
//! `{program data}/plugins/configurations/LDAP-Auth.xml`, the plugin
//! assembly's name with `.xml` (`BasePlugin<T>.ConfigurationFilePath`), written
//! by .NET's `XmlSerializer`. Ferrofin reads it with the server configuration's
//! importer ([`ferrofin_core::config_import`]) and never changes or removes it.
//!
//! The import is a one-shot boot step that runs only while the plugin has no
//! `config.json`, so a second boot, or an admin's later edit, is never
//! overwritten. That file is written last: an import cut short by a crash or a
//! database error runs again whole at the next boot, and its links and enabled
//! flag are the same the second time.
//!
//! The old version, running beside this one in a rolling update, has no
//! LDAP-Auth and never reads the file or the links. It can undo one thing: its
//! plugin manager saves the whole plugin state file when an admin enables or
//! disables a plugin on it, which would drop the enabled flag written here, and
//! the import, already marked done, does not run again. An admin then enables
//! LDAP-Auth by hand.

use std::path::Path;

use ferrofin_core::config_import;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::ExternalIdentityStore;
use ferrofin_traits::library::UserManager;
use ferrofin_traits::plugins::PluginManager;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::provider::{LdapAuthProvider, PROVIDER_ID};
use super::{EXTENSION_ID, LdapConfig};

/// The file upstream keeps its configuration in, under Jellyfin's
/// `plugins/configurations` directory: the assembly name, `LDAP-Auth`, with
/// `.xml`.
pub const CONFIGURATION_FILE_NAME: &str = "LDAP-Auth.xml";

/// Upstream's placeholder `LdapServer` (`PluginConfiguration()`). An adopted
/// file still holding it names no real directory: it is imported as not
/// configured (D3), so nothing connects to it.
const PLACEHOLDER_SERVER: &str = "ldap-server.contoso.com";

/// Upstream's `PluginConfiguration` as its XML holds it: the settings Ferrofin
/// keeps in `config.json`, and the `LdapUsers` uid map it keeps in the
/// external-identity store (D12).
#[derive(Serialize, Deserialize)]
struct AdoptedConfiguration {
    #[serde(flatten)]
    config: LdapConfig,
    #[serde(rename = "LdapUsers", default)]
    ldap_users: Vec<AdoptedLink>,
}

/// One `LdapUser` entry of the uid map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct AdoptedLink {
    linked_jellyfin_user_id: String,
    ldap_uid: String,
    profile_image_hash: String,
}

/// What an import did, for its log line.
#[derive(Debug, PartialEq, Eq)]
struct Imported {
    links: usize,
    /// Entries naming a user who no longer exists.
    deleted_users: usize,
    enabled: bool,
}

/// Imports `xml_path` (upstream's [`CONFIGURATION_FILE_NAME`]) once, while
/// LDAP-Auth has no configuration in `plugin_state_dir` (the plugin manager's
/// directory, holding `{id}/config.json`):
/// - every setting, the bind password included, becomes the plugin's
///   configuration, except a server still at upstream's placeholder, which is
///   left empty, so the plugin stays inactive until an admin sets one;
/// - each `LdapUsers` entry whose user still exists becomes a link in
///   `identities`; an entry for a deleted user is skipped;
/// - the plugin is enabled when any user's `AuthenticationProviderId` is
///   LDAP-Auth's (case-insensitive). Otherwise it keeps its default, off.
///
/// A missing file, or an install without LDAP-Auth registered, does nothing.
/// A file that cannot be read or parsed, or a write that fails, is logged, and
/// the next boot tries again: this never stops the server starting.
pub async fn import_adopted_config(
    xml_path: &Path,
    plugin_state_dir: &Path,
    plugins: &dyn PluginManager,
    users: &dyn UserManager,
    identities: &dyn ExternalIdentityStore,
) {
    match import(xml_path, plugin_state_dir, plugins, users, identities).await {
        Ok(None) => {}
        Ok(Some(imported)) => tracing::info!(
            path = %xml_path.display(),
            links = imported.links,
            deleted_users = imported.deleted_users,
            enabled = imported.enabled,
            "imported the LDAP-Auth configuration of the adopted Jellyfin"
        ),
        Err(error) => tracing::warn!(
            path = %xml_path.display(),
            %error,
            "the adopted LDAP-Auth configuration could not be imported; the next boot tries again"
        ),
    }
}

/// [`import_adopted_config`]'s work: `None` when there was nothing to import.
async fn import(
    xml_path: &Path,
    plugin_state_dir: &Path,
    plugins: &dyn PluginManager,
    users: &dyn UserManager,
    identities: &dyn ExternalIdentityStore,
) -> Result<Option<Imported>, ServiceError> {
    if plugins.get_plugin(EXTENSION_ID).await?.is_none() {
        return Ok(None);
    }
    let config_file = plugin_state_dir
        .join(EXTENSION_ID.to_string())
        .join("config.json");
    // `try_exists`, not `exists`: a configuration that cannot be checked must
    // not be taken for a missing one and overwritten.
    if tokio::fs::try_exists(&config_file)
        .await
        .map_err(|e| ServiceError::backend(format!("check {}: {e}", config_file.display())))?
    {
        return Ok(None);
    }
    let xml = match tokio::fs::read_to_string(xml_path).await {
        Ok(xml) => xml,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ServiceError::backend(format!("read: {e}"))),
    };
    let AdoptedConfiguration {
        mut config,
        ldap_users,
    } = parse(&xml).map_err(|_| {
        // The parser's message can quote the file's text (an unknown `&name;`
        // entity), the bind password's included, so none of it is logged.
        ServiceError::backend("not a readable PluginConfiguration document")
    })?;
    if config.ldap_server.eq_ignore_ascii_case(PLACEHOLDER_SERVER) {
        tracing::warn!(
            setting = "LdapServer",
            value = PLACEHOLDER_SERVER,
            "the adopted LDAP-Auth configuration still names upstream's placeholder server; \
             LDAP-Auth stays inactive until a server is set"
        );
        config.ldap_server.clear();
    }

    let mut imported = Imported {
        links: 0,
        deleted_users: 0,
        enabled: false,
    };
    for link in &ldap_users {
        // `Guid.Empty` (`LdapUser`'s default) and an id that does not parse
        // name no user: nothing to link.
        let Some(user_id) = Uuid::parse_str(&link.linked_jellyfin_user_id)
            .ok()
            .filter(|id| !id.is_nil())
        else {
            continue;
        };
        match identities
            .link(
                user_id,
                LdapAuthProvider::identity(&link.ldap_uid),
                Some(&link.profile_image_hash),
            )
            .await
        {
            Ok(_) => imported.links += 1,
            Err(ServiceError::NotFound(_)) => imported.deleted_users += 1,
            Err(e) => return Err(e),
        }
    }

    if users.get_users().await?.iter().any(|user| {
        user.authentication_provider_id
            .eq_ignore_ascii_case(PROVIDER_ID)
    }) {
        plugins.enable_plugin(EXTENSION_ID).await?;
        imported.enabled = true;
    }
    // Last: its presence is what stops the next boot importing again.
    let bytes = serde_json::to_vec(&config)
        .map_err(|e| ServiceError::backend(format!("serialize: {e}")))?;
    plugins
        .set_plugin_configuration(EXTENSION_ID, bytes)
        .await?;
    Ok(Some(imported))
}

/// Parses upstream's `PluginConfiguration` document. A setting the XML leaves
/// out keeps Ferrofin's default, as it would upstream's constructor value.
fn parse(xml: &str) -> Result<AdoptedConfiguration, ServiceError> {
    // The one empty entry is only the shape `import_over` reads `LdapUser`
    // entries against (its strings keep an all-digit uid a string). It
    // survives only when the document has no `LdapUsers`, and then links no
    // one: its user id is empty.
    let shape = AdoptedConfiguration {
        config: LdapConfig::default(),
        ldap_users: vec![AdoptedLink::default()],
    };
    config_import::import_over(&shape, xml, "PluginConfiguration", &[])
}

#[cfg(test)]
mod tests {
    use ferrofin_model::secret::Secret;

    use super::super::ProfileImageFormat;
    use super::*;

    /// `LDAP-Auth.xml` as `XmlSerializer` writes upstream's
    /// `PluginConfiguration` (v24): members in declaration order, `LdapUser`
    /// and `string` entries, empty strings as empty elements, a null
    /// `PasswordResetUrl` left out.
    const UPSTREAM_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <LdapUsers>
    <LdapUser>
      <LinkedJellyfinUserId>6b1f2c1e-0d3a-4c55-9a77-0e4f3f2b9a10</LinkedJellyfinUserId>
      <LdapUid>ada</LdapUid>
      <ProfileImageHash>q83vEjRWeJA=</ProfileImageHash>
    </LdapUser>
    <LdapUser>
      <LinkedJellyfinUserId>0c9a7e52-3b1d-4f0e-8d2a-5e6f7a8b9c0d</LinkedJellyfinUserId>
      <LdapUid>1001</LdapUid>
      <ProfileImageHash />
    </LdapUser>
    <LdapUser>
      <LinkedJellyfinUserId>00000000-0000-0000-0000-000000000000</LinkedJellyfinUserId>
      <LdapUid />
      <ProfileImageHash />
    </LdapUser>
  </LdapUsers>
  <LdapServer>ldap.example.org</LdapServer>
  <LdapPort>636</LdapPort>
  <UseSsl>true</UseSsl>
  <UseStartTls>false</UseStartTls>
  <SkipSslVerify>true</SkipSslVerify>
  <LdapBindUser>cn=reader,dc=example,dc=org</LdapBindUser>
  <LdapBindPassword>reader-secret</LdapBindPassword>
  <LdapBaseDn>dc=example,dc=org</LdapBaseDn>
  <LdapSearchFilter>(&amp;(objectClass=inetOrgPerson)(memberOf=cn=media,dc=example,dc=org))</LdapSearchFilter>
  <LdapAdminBaseDn />
  <LdapAdminFilter>(memberOf=cn=admins,dc=example,dc=org)</LdapAdminFilter>
  <EnableLdapAdminFilterMemberUid>true</EnableLdapAdminFilterMemberUid>
  <LdapSearchAttributes>uid, mail</LdapSearchAttributes>
  <LdapClientCertPath />
  <LdapClientKeyPath />
  <LdapRootCaPath>/config/ca.pem</LdapRootCaPath>
  <CreateUsersFromLdap>false</CreateUsersFromLdap>
  <AllowPassChange>true</AllowPassChange>
  <LdapUidAttribute>uid</LdapUidAttribute>
  <LdapUsernameAttribute>uid</LdapUsernameAttribute>
  <LdapPasswordAttribute>userPassword</LdapPasswordAttribute>
  <EnableLdapProfileImageSync>true</EnableLdapProfileImageSync>
  <RemoveImagesNotInLdap>false</RemoveImagesNotInLdap>
  <LdapProfileImageAttribute>jpegPhoto</LdapProfileImageAttribute>
  <LdapProfileImageFormat>Base64</LdapProfileImageFormat>
  <EnableAllFolders>false</EnableAllFolders>
  <EnabledFolders>
    <string>f137a2dd21bbc1b99aa5c0f6bf02a805</string>
  </EnabledFolders>
</PluginConfiguration>"#;

    #[test]
    fn upstreams_serializer_output_is_read_whole() {
        let adopted = parse(UPSTREAM_XML).expect("parse");
        assert_eq!(
            adopted.config,
            LdapConfig {
                ldap_server: "ldap.example.org".to_owned(),
                ldap_port: 636,
                use_ssl: true,
                use_start_tls: false,
                skip_ssl_verify: true,
                ldap_bind_user: "cn=reader,dc=example,dc=org".to_owned(),
                ldap_bind_password: Secret::new("reader-secret"),
                ldap_base_dn: "dc=example,dc=org".to_owned(),
                ldap_search_filter:
                    "(&(objectClass=inetOrgPerson)(memberOf=cn=media,dc=example,dc=org))".to_owned(),
                ldap_admin_base_dn: String::new(),
                ldap_admin_filter: "(memberOf=cn=admins,dc=example,dc=org)".to_owned(),
                enable_ldap_admin_filter_member_uid: true,
                ldap_search_attributes: "uid, mail".to_owned(),
                ldap_client_cert_path: String::new(),
                ldap_client_key_path: String::new(),
                ldap_root_ca_path: "/config/ca.pem".to_owned(),
                create_users_from_ldap: false,
                allow_pass_change: true,
                ldap_uid_attribute: "uid".to_owned(),
                ldap_username_attribute: "uid".to_owned(),
                ldap_password_attribute: "userPassword".to_owned(),
                enable_ldap_profile_image_sync: true,
                remove_images_not_in_ldap: false,
                ldap_profile_image_attribute: "jpegPhoto".to_owned(),
                ldap_profile_image_format: ProfileImageFormat::Base64,
                enable_all_folders: false,
                enabled_folders: vec!["f137a2dd21bbc1b99aa5c0f6bf02a805".to_owned()],
                password_reset_url: None,
            }
        );
        assert_eq!(
            adopted.ldap_users,
            [
                AdoptedLink {
                    linked_jellyfin_user_id: "6b1f2c1e-0d3a-4c55-9a77-0e4f3f2b9a10".to_owned(),
                    ldap_uid: "ada".to_owned(),
                    profile_image_hash: "q83vEjRWeJA=".to_owned(),
                },
                AdoptedLink {
                    linked_jellyfin_user_id: "0c9a7e52-3b1d-4f0e-8d2a-5e6f7a8b9c0d".to_owned(),
                    ldap_uid: "1001".to_owned(),
                    profile_image_hash: String::new(),
                },
                AdoptedLink {
                    linked_jellyfin_user_id: "00000000-0000-0000-0000-000000000000".to_owned(),
                    ..AdoptedLink::default()
                },
            ]
        );
    }

    /// [`UPSTREAM_XML`] with its `LdapUsers` element replaced by `uid_map`.
    fn with_uid_map(uid_map: &str) -> String {
        let start = UPSTREAM_XML.find("<LdapUsers>").expect("start");
        let end = UPSTREAM_XML.find("<LdapServer>").expect("end");
        format!(
            "{}{uid_map}\n  {}",
            &UPSTREAM_XML[..start],
            &UPSTREAM_XML[end..]
        )
    }

    /// A one-entry `LdapUsers` is a list (not an object with one member), and
    /// an all-digit uid stays text; an empty map is empty, and a missing one
    /// leaves only the shape entry, which names no user.
    #[test]
    fn a_one_entry_an_empty_and_a_missing_uid_map_are_read() {
        let one = with_uid_map(
            "<LdapUsers>\n    <LdapUser>\n      \
             <LinkedJellyfinUserId>0c9a7e52-3b1d-4f0e-8d2a-5e6f7a8b9c0d</LinkedJellyfinUserId>\n      \
             <LdapUid>1001</LdapUid>\n      <ProfileImageHash />\n    </LdapUser>\n  </LdapUsers>",
        );
        assert_eq!(
            parse(&one).expect("parse").ldap_users,
            [AdoptedLink {
                linked_jellyfin_user_id: "0c9a7e52-3b1d-4f0e-8d2a-5e6f7a8b9c0d".to_owned(),
                ldap_uid: "1001".to_owned(),
                profile_image_hash: String::new(),
            }]
        );
        assert_eq!(
            parse(&with_uid_map("<LdapUsers />"))
                .expect("parse")
                .ldap_users,
            Vec::new()
        );
        assert_eq!(
            parse(&with_uid_map("")).expect("parse").ldap_users,
            [AdoptedLink::default()]
        );
    }

    #[test]
    fn a_document_that_is_not_the_plugins_configuration_is_refused() {
        assert!(parse("<ServerConfiguration />").is_err());
        assert!(parse("<PluginConfiguration><LdapServer>").is_err());
    }
}
