//! The adopted `LDAP-Auth.xml` import (B7), end to end through the composition
//! root.
//!
//! This is an integration test because the wiring lives in `build_app_state`:
//! the extension's tests call the import with paths and stores they build
//! themselves, so dropping the call, or passing it the wrong data directory,
//! state directory or identity store, would leave all of them green.

use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_extensions::ldap_auth::EXTENSION_ID;
use ferrofin_extensions::ldap_auth::adoption::CONFIGURATION_FILE_NAME;
use ferrofin_extensions::ldap_auth::provider::PROVIDER_ID;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::library::UserManager;
use uuid::Uuid;

/// A minimal `LDAP-Auth.xml` as `XmlSerializer` writes it, linking `user_id`
/// to the uid `ada`.
fn upstream_xml(user_id: Uuid) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <LdapUsers>
    <LdapUser>
      <LinkedJellyfinUserId>{user_id}</LinkedJellyfinUserId>
      <LdapUid>ada</LdapUid>
      <ProfileImageHash />
    </LdapUser>
  </LdapUsers>
  <LdapServer>ldap.example</LdapServer>
  <LdapPort>636</LdapPort>
  <UseSsl>false</UseSsl>
</PluginConfiguration>"#
    )
}

/// An adopted install: an LDAP user, and Jellyfin's `LDAP-Auth.xml` where
/// Jellyfin keeps it, `{program data}/plugins/configurations`.
struct Adopted {
    _temp: tempfile::TempDir,
    config: Config,
    db: ferrofin_db::Database,
    ada: Uuid,
}

impl Adopted {
    async fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        for d in ["config", "data", "cache"] {
            std::fs::create_dir_all(temp.path().join(d)).expect("dir");
        }
        let config = Config::test_stub(temp.path());
        let db = ferrofin_db::Database::connect(&config.database_url())
            .await
            .expect("open db");
        db.run_migrations().await.expect("migrations");

        let users = ferrofin_core::FerrofinUserManager::new(db.clone());
        let user = users.create_user("ada").await.expect("create");
        let mut policy = users
            .get_user_dto(&user, None)
            .await
            .expect("dto")
            .policy
            .expect("policy");
        PROVIDER_ID.clone_into(&mut policy.authentication_provider_id);
        let ada = Uuid::parse_str(&user.id).expect("id");
        users.update_policy(ada, &policy).await.expect("policy");

        let configurations = config.data_dir.join("plugins").join("configurations");
        std::fs::create_dir_all(&configurations).expect("dir");
        std::fs::write(
            configurations.join(CONFIGURATION_FILE_NAME),
            upstream_xml(ada),
        )
        .expect("write");
        Self {
            _temp: temp,
            config,
            db,
            ada,
        }
    }

    /// The plugin manager's own `config.json` for LDAP-Auth.
    fn config_file(&self) -> std::path::PathBuf {
        self.config
            .config_dir
            .join("plugins")
            .join(EXTENSION_ID.to_string())
            .join("config.json")
    }

    /// Boots the composition root, returning LDAP-Auth's enabled flag, its
    /// served configuration and whether Ada's uid is linked to her.
    async fn boot(&self) -> (bool, serde_json::Value, bool) {
        let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
            encoder_app_path_display: None,
            ffmpeg: std::path::PathBuf::from("ffmpeg"),
            ffprobe: std::path::PathBuf::from("ffprobe"),
            capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
            chromaprint_muxer: false,
        };
        let (shutdown_tx, _rx) = tokio::sync::oneshot::channel();
        let wired = build_app_state(&self.db, &self.config, &ffmpeg, None, shutdown_tx)
            .await
            .expect("wire app state");
        let plugins = &wired.state.plugins;
        let enabled = plugins
            .get_plugin(EXTENSION_ID)
            .await
            .expect("get")
            .expect("registered")
            .enabled;
        let stored = serde_json::from_slice(
            &plugins
                .get_plugin_configuration(EXTENSION_ID)
                .await
                .expect("config"),
        )
        .expect("json");
        let linked = FerrofinExternalIdentityStore::new(self.db.clone())
            .find(ExternalIdentityKey {
                provider_id: PROVIDER_ID,
                issuer: "",
                subject: "ada",
            })
            .await
            .expect("find")
            .is_some_and(|link| link.user_id == self.ada);
        (enabled, stored, linked)
    }
}

#[tokio::test]
async fn an_adopted_ldap_auth_xml_is_imported_at_boot() {
    let adopted = Adopted::new().await;

    let (enabled, stored, linked) = adopted.boot().await;

    assert!(enabled, "an LDAP user exists");
    assert_eq!(stored["LdapServer"], "ldap.example");
    assert_eq!(stored["LdapPort"], 636);
    assert_eq!(stored["UseSsl"], false);
    assert!(linked, "the uid map went to the identity store");
    assert!(adopted.config_file().is_file());
}

/// The import checks the plugin manager's own directory: a configuration
/// already there is the admin's, and nothing is imported over it.
#[tokio::test]
async fn an_existing_configuration_is_never_imported_over() {
    let adopted = Adopted::new().await;
    let file = adopted.config_file();
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
    std::fs::write(&file, br#"{"LdapServer":"kept.example"}"#).expect("write");

    let (enabled, stored, linked) = adopted.boot().await;

    assert_eq!(stored["LdapServer"], "kept.example");
    assert!(!enabled);
    assert!(!linked);
}
