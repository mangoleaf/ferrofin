//! Adopting a Jellyfin install that used LDAP-Auth (B7, D7): the one-time
//! import of `LDAP-Auth.xml` at boot, over the real plugin manager, user
//! manager and external-identity store, then sign-in through the fake
//! directory.

use super::super::super::adoption::{CONFIGURATION_FILE_NAME, import_adopted_config};
use super::*;

/// A Jellyfin-installed user's provider id as an older Jellyfin may have
/// stored it: the match is case-insensitive.
const ADOPTED_PROVIDER_ID: &str = "jellyfin.plugin.ldap_auth.ldapauthenticationproviderplugin";

/// `LDAP-Auth.xml` in `XmlSerializer`'s shape, naming `server` and holding
/// `users` as its `LdapUsers` entries (`(user id, uid, avatar hash)`): the
/// [`config`] directory otherwise.
fn upstream_xml(server: &str, users: &[(Uuid, &str, &str)]) -> String {
    let mut entries = String::new();
    for (id, uid, hash) in users {
        let _ = write!(
            entries,
            "    <LdapUser>\n      <LinkedJellyfinUserId>{id}</LinkedJellyfinUserId>\n      \
             <LdapUid>{uid}</LdapUid>\n      <ProfileImageHash>{hash}</ProfileImageHash>\n    \
             </LdapUser>\n"
        );
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <LdapUsers>
{entries}  </LdapUsers>
  <LdapServer>{server}</LdapServer>
  <LdapPort>389</LdapPort>
  <UseSsl>false</UseSsl>
  <UseStartTls>false</UseStartTls>
  <SkipSslVerify>false</SkipSslVerify>
  <LdapBindUser>{READER_DN}</LdapBindUser>
  <LdapBindPassword>{READER_PASSWORD}</LdapBindPassword>
  <LdapBaseDn>{BASE_DN}</LdapBaseDn>
  <LdapSearchFilter>(objectClass=inetOrgPerson)</LdapSearchFilter>
  <LdapAdminBaseDn />
  <LdapAdminFilter />
  <EnableLdapAdminFilterMemberUid>false</EnableLdapAdminFilterMemberUid>
  <LdapSearchAttributes>uid, mail</LdapSearchAttributes>
  <LdapClientCertPath />
  <LdapClientKeyPath />
  <LdapRootCaPath />
  <CreateUsersFromLdap>true</CreateUsersFromLdap>
  <AllowPassChange>false</AllowPassChange>
  <LdapUidAttribute>uid</LdapUidAttribute>
  <LdapUsernameAttribute>uid</LdapUsernameAttribute>
  <LdapPasswordAttribute>userPassword</LdapPasswordAttribute>
  <EnableLdapProfileImageSync>false</EnableLdapProfileImageSync>
  <RemoveImagesNotInLdap>false</RemoveImagesNotInLdap>
  <LdapProfileImageAttribute>jpegphoto</LdapProfileImageAttribute>
  <LdapProfileImageFormat>Default</LdapProfileImageFormat>
  <EnableAllFolders>true</EnableAllFolders>
  <EnabledFolders />
</PluginConfiguration>"#
    )
}

impl Harness {
    /// Writes `xml` as the adopted `LDAP-Auth.xml` and runs the boot import,
    /// returning the file's path.
    async fn adopt(&self, xml: &str) -> std::path::PathBuf {
        let dir = self.plugins_dir.path().join("jellyfin-configurations");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(CONFIGURATION_FILE_NAME);
        std::fs::write(&path, xml).expect("write");
        self.boot(&path).await;
        path
    }

    /// The boot import of `path`.
    async fn boot(&self, path: &std::path::Path) {
        import_adopted_config(
            path,
            self.plugins_dir.path(),
            self.plugins.as_ref(),
            self.users.as_ref(),
            self.identities.as_ref(),
        )
        .await;
    }

    async fn enabled(&self) -> bool {
        self.plugins
            .get_plugin(EXTENSION_ID)
            .await
            .expect("get")
            .expect("registered")
            .enabled
    }

    async fn saved_config(&self) -> LdapConfig {
        let bytes = self
            .plugins
            .get_plugin_configuration(EXTENSION_ID)
            .await
            .expect("config");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn config_file(&self) -> std::path::PathBuf {
        self.plugins_dir
            .path()
            .join(EXTENSION_ID.to_string())
            .join("config.json")
    }
}

/// Captures the crate's logs for the rest of the test.
fn capture() -> (CapturedLogs, tracing::subscriber::DefaultGuard) {
    let logs = CapturedLogs::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            // `ferrofin_core` too: the XML importer logs a rejected field.
            .with_env_filter("ferrofin_extensions=debug,ferrofin_core=debug")
            .finish(),
    );
    (logs, guard)
}

#[tokio::test]
async fn an_adopted_ldap_user_signs_in_after_the_import_with_no_admin_action() {
    let (logs, _guard) = capture();
    let harness = Harness::disabled().await;
    let ada = harness.existing_user("ada", ADOPTED_PROVIDER_ID, "").await;
    let deleted = Uuid::from_u128(0xdead);

    harness
        .adopt(&upstream_xml(
            "ldap.example",
            &[
                (id_of(&ada), "ada", "q83vEjRWeJA="),
                (deleted, "gone", ""),
                (Uuid::nil(), "", ""),
            ],
        ))
        .await;

    assert!(harness.enabled().await, "an LDAP user exists");
    assert_eq!(harness.saved_config().await, config());
    let links = harness.links().await;
    assert_eq!(links.len(), 1, "the deleted user's entry is skipped");
    assert_eq!(links[0].user_id, id_of(&ada));
    assert_eq!(links[0].subject, "ada");
    assert_eq!(links[0].avatar_hash.as_deref(), Some("q83vEjRWeJA="));

    let signed_in = harness
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("user");
    assert_eq!(signed_in.id, ada.id, "the adopted account, not a new one");
    assert!(logs.contains("imported the LDAP-Auth configuration"));
    assert!(
        logs.contains("links=1 deleted_users=1 enabled=true"),
        "the never-linked entry is not counted"
    );
    assert!(
        !logs.contains(READER_PASSWORD),
        "the bind password is never logged"
    );
}

#[tokio::test]
async fn without_ldap_users_the_settings_are_imported_and_the_plugin_stays_off() {
    let harness = Harness::disabled().await;
    harness
        .existing_user(
            "carol",
            "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider",
            "pw",
        )
        .await;

    // No `LdapUsers` element at all.
    let xml = upstream_xml("ldap.example", &[]);
    let without_map = xml.replace("  <LdapUsers>\n  </LdapUsers>\n", "");
    assert_ne!(without_map, xml);
    harness.adopt(&without_map).await;

    assert!(!harness.enabled().await);
    assert_eq!(harness.saved_config().await.ldap_server, "ldap.example");
    assert_eq!(harness.links().await, Vec::new());
}

#[tokio::test]
async fn a_second_boot_does_not_import_again_and_keeps_the_admins_edits() {
    let harness = Harness::disabled().await;
    let ada = harness.existing_user("ada", PROVIDER_ID, "").await;
    let path = harness
        .adopt(&upstream_xml("ldap.example", &[(id_of(&ada), "ada", "")]))
        .await;
    assert!(harness.enabled().await, "imported on the first boot");
    assert_eq!(harness.links().await.len(), 1);

    // The admin points the plugin elsewhere, turns it off and unlinks Ada.
    harness
        .configure(&LdapConfig {
            ldap_server: "ldap2.example".to_owned(),
            ..config()
        })
        .await;
    harness
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");
    harness
        .identities
        .unlink(LdapAuthProvider::identity("ada"))
        .await
        .expect("unlink");

    harness.boot(&path).await;

    assert_eq!(harness.saved_config().await.ldap_server, "ldap2.example");
    assert!(!harness.enabled().await);
    assert_eq!(harness.links().await, Vec::new());
}

/// An import cut short before `config.json` was written runs again whole, with
/// the same result.
#[tokio::test]
async fn an_import_cut_short_runs_again_with_the_same_result() {
    let harness = Harness::disabled().await;
    let ada = harness.existing_user("ada", PROVIDER_ID, "").await;
    let path = harness
        .adopt(&upstream_xml("ldap.example", &[(id_of(&ada), "ada", "")]))
        .await;
    // What the first run did is undone, as if it had stopped before it.
    assert!(harness.enabled().await, "the first run enabled it");
    std::fs::remove_file(harness.config_file()).expect("remove");
    assert!(
        harness
            .identities
            .unlink(LdapAuthProvider::identity("ada"))
            .await
            .expect("unlink"),
        "the first run linked Ada"
    );
    harness
        .plugins
        .disable_plugin(EXTENSION_ID)
        .await
        .expect("disable");

    harness.boot(&path).await;

    assert!(harness.enabled().await);
    assert_eq!(harness.saved_config().await, config());
    let links = harness.links().await;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].user_id, id_of(&ada));
}

#[tokio::test]
async fn a_malformed_file_logs_a_warning_and_the_next_boot_tries_again() {
    let (logs, _guard) = capture();
    let harness = Harness::disabled().await;
    let ada = harness.existing_user("ada", PROVIDER_ID, "").await;

    harness
        .adopt("<PluginConfiguration><LdapServer>ldap.example")
        .await;

    assert_eq!(
        logs.count("WARN", "could not be imported; the next boot tries again"),
        1
    );
    assert!(!harness.config_file().exists());
    assert!(!harness.enabled().await);

    // An unknown entity in the bind password: the parser's message would name
    // it, so it is not logged.
    harness
        .adopt(&upstream_xml("ldap.example", &[]).replace(
            &format!("<LdapBindPassword>{READER_PASSWORD}</LdapBindPassword>"),
            "<LdapBindPassword>pass&zq7x;word</LdapBindPassword>",
        ))
        .await;
    assert_eq!(
        logs.count("WARN", "could not be imported; the next boot tries again"),
        2
    );
    assert!(!logs.contains("zq7x"));
    assert!(!harness.config_file().exists());

    // Fixed by the next boot.
    harness
        .adopt(&upstream_xml("ldap.example", &[(id_of(&ada), "ada", "")]))
        .await;
    assert!(harness.enabled().await);
    assert_eq!(harness.saved_config().await, config());
    assert_eq!(harness.links().await.len(), 1);
}

/// A setting the importer rejects keeps its default and is warned about by
/// name, and the bind password beside it never reaches the log.
#[tokio::test]
async fn a_rejected_setting_keeps_its_default_and_the_bind_password_stays_unlogged() {
    let (logs, _guard) = capture();
    let harness = Harness::disabled().await;

    harness
        .adopt(&upstream_xml("ldap.example", &[]).replace(
            "<LdapPort>389</LdapPort>",
            "<LdapPort>99999999999</LdapPort>",
        ))
        .await;

    assert_eq!(
        logs.count("WARN", "could not import jellyfin configuration field"),
        1
    );
    assert!(logs.contains("LdapPort"));
    let saved = harness.saved_config().await;
    assert_eq!(saved.ldap_port, 389);
    assert_eq!(saved.ldap_bind_password, Secret::new(READER_PASSWORD));
    assert!(!logs.contains(READER_PASSWORD));
}

/// Without LDAP-Auth registered (the benchmark's `disable_extensions`), the
/// file is left alone.
#[tokio::test]
async fn without_ldap_auth_registered_nothing_is_imported() {
    let (logs, _guard) = capture();
    let harness = Harness::disabled().await;
    let ada = harness.existing_user("ada", PROVIDER_ID, "").await;
    let dir = harness.plugins_dir.path().join("jellyfin-configurations");
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join(CONFIGURATION_FILE_NAME);
    std::fs::write(
        &path,
        upstream_xml("ldap.example", &[(id_of(&ada), "ada", "")]),
    )
    .expect("write");
    let state_dir = harness.plugins_dir.path().join("none-registered");
    let none = FerrofinPluginManager::new(Vec::new(), state_dir.clone());

    import_adopted_config(
        &path,
        &state_dir,
        &none,
        harness.users.as_ref(),
        harness.identities.as_ref(),
    )
    .await;

    assert_eq!(harness.links().await, Vec::new());
    assert!(!state_dir.join(EXTENSION_ID.to_string()).exists());
    assert!(!logs.contains("LDAP-Auth configuration"));
}

#[tokio::test]
async fn a_missing_file_is_silent() {
    let (logs, _guard) = capture();
    let harness = Harness::disabled().await;
    harness.existing_user("ada", PROVIDER_ID, "").await;

    harness
        .boot(&harness.plugins_dir.path().join(CONFIGURATION_FILE_NAME))
        .await;

    assert!(!harness.config_file().exists());
    assert!(!harness.enabled().await);
    assert!(!logs.contains("LDAP-Auth configuration"), "nothing logged");
}

#[tokio::test]
async fn upstreams_placeholder_server_is_imported_as_not_configured() {
    let (logs, _guard) = capture();
    let harness = Harness::disabled().await;
    let ada = harness.existing_user("ada", PROVIDER_ID, "").await;

    harness
        .adopt(&upstream_xml(
            "ldap-server.contoso.com",
            &[(id_of(&ada), "ada", "")],
        ))
        .await;

    assert_eq!(
        logs.count("WARN", "still names upstream's placeholder server"),
        1
    );
    assert!(logs.contains("LdapServer"), "the warning names the setting");
    let saved = harness.saved_config().await;
    assert_eq!(saved.ldap_server, "", "not configured (D3)");
    assert_eq!(saved.ldap_bind_user, READER_DN, "the rest is imported");
    assert!(harness.enabled().await, "on for the LDAP users");
    assert_eq!(harness.links().await.len(), 1);

    // The saved provider is inactive, so the user manager's invalid provider
    // refuses her.
    assert_eq!(
        harness.sign_in("ada", ADA_PASSWORD).await.expect("sign in"),
        None
    );
    assert!(harness.directory.untouched(), "no network call");
}
