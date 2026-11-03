//! The LDAP-Auth configuration, the connection test
//! (`LdapAuthenticationProviderPlugin.TestServerBind`) over a fake directory,
//! and the service behind `POST /Ldap/TestServerBind` over the real plugin
//! manager.

use std::sync::{Arc, Mutex};

use ferrofin_core::FerrofinPluginManager;
use rstest::rstest;

use super::*;
use crate::registered_plugins;

/// What the fake directory answers at each step.
#[derive(Clone)]
struct Script {
    connect: Result<(), LdapError>,
    bind: Result<(), LdapError>,
    search: Result<SearchOutcome, LdapError>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            connect: Ok(()),
            bind: Ok(()),
            search: Ok(SearchOutcome::default()),
        }
    }
}

/// Everything the fake directory was asked to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Calls {
    connects: Vec<ConnectionOptions>,
    binds: Vec<(String, String)>,
    searches: Vec<(String, SearchScope, String, Vec<String>)>,
    unbinds: usize,
}

/// An [`LdapDirectory`] that answers from a [`Script`] and records its calls.
#[derive(Clone, Default)]
struct FakeDirectory {
    script: Script,
    calls: Arc<Mutex<Calls>>,
}

impl FakeDirectory {
    fn answering(script: Script) -> Self {
        Self {
            script,
            calls: Arc::default(),
        }
    }

    fn calls(&self) -> Calls {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait]
impl LdapDirectory for FakeDirectory {
    async fn connect(
        &self,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn LdapConnection>, LdapError> {
        self.calls
            .lock()
            .expect("calls")
            .connects
            .push(options.clone());
        self.script.connect.clone()?;
        Ok(Box::new(self.clone()))
    }
}

#[async_trait]
impl LdapConnection for FakeDirectory {
    async fn simple_bind(&mut self, dn: &str, password: &str) -> Result<(), LdapError> {
        self.calls
            .lock()
            .expect("calls")
            .binds
            .push((dn.to_owned(), password.to_owned()));
        self.script.bind.clone()
    }

    async fn search(
        &mut self,
        base: &str,
        scope: SearchScope,
        filter: &str,
        attributes: &[&str],
    ) -> Result<SearchOutcome, LdapError> {
        self.calls.lock().expect("calls").searches.push((
            base.to_owned(),
            scope,
            filter.to_owned(),
            attributes.iter().map(|a| (*a).to_owned()).collect(),
        ));
        self.script.search.clone()
    }

    async fn modify_replace(
        &mut self,
        _dn: &str,
        _attribute: &str,
        _values: &[&str],
    ) -> Result<(), LdapError> {
        Err(LdapError::Connection("not scripted".to_owned()))
    }

    async fn password_modify(
        &mut self,
        _user_dn: &str,
        _new_password: &str,
    ) -> Result<(), LdapError> {
        Err(LdapError::Connection("not scripted".to_owned()))
    }

    async fn supported_extensions(&mut self) -> Result<Vec<String>, LdapError> {
        Err(LdapError::Connection("not scripted".to_owned()))
    }

    async fn unbind(&mut self) {
        self.calls.lock().expect("calls").unbinds += 1;
    }
}

/// A configuration for a plain-LDAP directory at `ldap.example`.
fn plain_config() -> LdapConfig {
    LdapConfig {
        ldap_server: "ldap.example".to_owned(),
        ldap_port: 389,
        use_ssl: false,
        ldap_bind_user: "  cn=reader,dc=example,dc=org ".to_owned(),
        ldap_bind_password: Secret::new("reader-secret"),
        ldap_base_dn: "dc=example,dc=org".to_owned(),
        ..LdapConfig::default()
    }
}

fn entries(count: usize) -> SearchOutcome {
    SearchOutcome {
        entries: (0..count)
            .map(|i| LdapEntry {
                dn: format!("uid=user{i},dc=example,dc=org"),
                attributes: Vec::new(),
            })
            .collect(),
        references: Vec::new(),
    }
}

fn step(value: &str) -> Option<String> {
    value.to_owned().into()
}

#[tokio::test]
async fn a_reachable_directory_reports_every_step() {
    let directory = FakeDirectory::answering(Script {
        search: Ok(entries(3)),
        ..Script::default()
    });
    let response = test_server_bind(&directory, &plain_config()).await;
    assert_eq!(
        response,
        ServerTestResponse {
            connect: step("Success"),
            start_tls: None,
            bind: step("Success"),
            base_search: step("Found 3 Entities"),
            error: None,
        }
    );
    let calls = directory.calls();
    assert_eq!(
        calls.binds,
        vec![(
            "cn=reader,dc=example,dc=org".to_owned(),
            "reader-secret".to_owned()
        )],
        "Novell trims the bind DN"
    );
    assert_eq!(
        calls.searches,
        vec![(
            "dc=example,dc=org".to_owned(),
            SearchScope::Subtree,
            "(objectclass=*)".to_owned(),
            vec!["1.1".to_owned()],
        )],
        "a subtree count of the base DN, with Novell's filter for an empty one"
    );
    assert_eq!(calls.unbinds, 1, "the connection is closed after the test");
    assert_eq!(
        calls.connects,
        vec![ConnectionOptions {
            server: "ldap.example".to_owned(),
            port: 389,
            use_ssl: false,
            use_start_tls: false,
            verification: CertificateVerification::System,
            client_certificate: None,
        }]
    );
}

#[tokio::test]
async fn start_tls_is_a_step_of_its_own_when_asked_for() {
    let directory = FakeDirectory::answering(Script {
        search: Ok(entries(0)),
        ..Script::default()
    });
    let config = LdapConfig {
        use_start_tls: true,
        ..plain_config()
    };
    let response = test_server_bind(&directory, &config).await;
    assert_eq!(response.connect, step("Success"));
    assert_eq!(response.start_tls, step("Success"));
    assert_eq!(response.base_search, step("Found 0 Entities"));
    assert!(directory.calls().connects[0].use_start_tls);
}

#[tokio::test]
async fn an_empty_bind_password_binds_anonymously() {
    let directory = FakeDirectory::answering(Script {
        search: Ok(entries(1)),
        ..Script::default()
    });
    let config = LdapConfig {
        ldap_bind_password: Secret::default(),
        ..plain_config()
    };
    let response = test_server_bind(&directory, &config).await;
    assert_eq!(response.bind, step("Anonymous"));
    assert_eq!(
        directory.calls().binds,
        vec![(String::new(), String::new())],
        "Novell drops the DN of a bind without a password"
    );
    assert_eq!(response.base_search, step("Found 1 Entities"));
}

#[tokio::test]
async fn a_directory_that_cannot_be_reached_stops_at_connect() {
    let directory = FakeDirectory::answering(Script {
        connect: Err(LdapError::Connect(
            "could not connect to ldap.example:389: Connection refused (os error 111)".to_owned(),
        )),
        ..Script::default()
    });
    let response = test_server_bind(&directory, &plain_config()).await;
    assert_eq!(
        response,
        ServerTestResponse {
            connect: step("Testing..."),
            error: step("could not connect to ldap.example:389: Connection refused (os error 111)"),
            ..ServerTestResponse::default()
        }
    );
    let calls = directory.calls();
    assert!(calls.binds.is_empty() && calls.searches.is_empty());
}

#[tokio::test]
async fn a_refused_start_tls_stops_at_start_tls() {
    let directory = FakeDirectory::answering(Script {
        connect: Err(LdapError::StartTls(
            "Protocol Error: unsupported extended operation".to_owned(),
        )),
        ..Script::default()
    });
    let config = LdapConfig {
        use_start_tls: true,
        ..plain_config()
    };
    let response = test_server_bind(&directory, &config).await;
    assert_eq!(
        response,
        ServerTestResponse {
            connect: step("Success"),
            start_tls: step("Testing..."),
            error: step("Protocol Error: unsupported extended operation"),
            ..ServerTestResponse::default()
        }
    );
}

#[tokio::test]
async fn a_wrong_bind_password_stops_at_bind_and_still_closes() {
    let directory = FakeDirectory::answering(Script {
        bind: Err(LdapError::Result {
            code: 49,
            message: String::new(),
        }),
        ..Script::default()
    });
    let response = test_server_bind(&directory, &plain_config()).await;
    assert_eq!(
        response,
        ServerTestResponse {
            connect: step("Success"),
            bind: step("Testing..."),
            error: step("Invalid Credentials"),
            ..ServerTestResponse::default()
        }
    );
    let calls = directory.calls();
    assert_eq!(calls.searches, Vec::new());
    assert_eq!(calls.unbinds, 1);
}

#[tokio::test]
async fn a_failed_base_search_stops_there_with_the_servers_reason() {
    let directory = FakeDirectory::answering(Script {
        search: Err(LdapError::Result {
            code: 32,
            message: " no such entry ".to_owned(),
        }),
        ..Script::default()
    });
    let response = test_server_bind(&directory, &plain_config()).await;
    assert_eq!(response.bind, step("Success"));
    assert_eq!(response.base_search, step("Testing..."));
    assert_eq!(response.error, step("No Such Object: no such entry"));
}

#[tokio::test]
async fn a_search_reference_fails_the_base_search_as_novells_iterator_does() {
    let directory = FakeDirectory::answering(Script {
        search: Ok(SearchOutcome {
            entries: entries(2).entries,
            references: vec!["ldap://other.example/dc=other".to_owned()],
        }),
        ..Script::default()
    });
    let response = test_server_bind(&directory, &plain_config()).await;
    assert_eq!(response.base_search, step("Testing..."));
    assert_eq!(
        response.error,
        step("Search result reference received, and referral following is off")
    );
}

#[rstest]
#[case::nothing_set(false, "", "", "", CertificateVerification::System, false)]
#[case::custom_root(false, "/ca.pem", "", "", CertificateVerification::CustomRoot("/ca.pem".into()), false)]
#[case::skip_wins_over_a_root(true, "/ca.pem", "", "", CertificateVerification::None, false)]
#[case::client_certificate(false, "", "/c.pem", "/k.pem", CertificateVerification::System, true)]
#[case::a_certificate_without_its_key(
    false,
    "",
    "/c.pem",
    "",
    CertificateVerification::System,
    false
)]
#[case::a_key_without_its_certificate(
    false,
    "",
    "",
    "/k.pem",
    CertificateVerification::System,
    false
)]
fn connection_options_follow_upstreams_get_connection_options(
    #[case] skip_ssl_verify: bool,
    #[case] root_ca: &str,
    #[case] client_cert: &str,
    #[case] client_key: &str,
    #[case] verification: CertificateVerification,
    #[case] presents_a_certificate: bool,
) {
    let options = ConnectionOptions::from_config(&LdapConfig {
        skip_ssl_verify,
        ldap_root_ca_path: root_ca.to_owned(),
        ldap_client_cert_path: client_cert.to_owned(),
        ldap_client_key_path: client_key.to_owned(),
        ..plain_config()
    });
    assert_eq!(options.verification, verification);
    assert_eq!(
        options.client_certificate,
        presents_a_certificate.then(|| ClientCertificate {
            certificate: "/c.pem".into(),
            key: "/k.pem".into(),
        })
    );
}

#[rstest]
#[case(0, "Success")]
#[case(49, "Invalid Credentials")]
#[case(53, "Unwilling To Perform")]
#[case(FILTER_ERROR, "Filter Error")]
#[case(4096, "LDAP result 4096")]
fn a_result_code_reads_as_its_name(#[case] code: u32, #[case] expected: &str) {
    let error = LdapError::Result {
        code,
        message: String::new(),
    };
    assert_eq!(error.to_string(), expected);
}

#[test]
fn the_seed_configuration_is_upstreams_with_an_empty_server_and_no_uid_map() {
    let seed: serde_json::Value =
        serde_json::from_slice(&LdapAuthExtension.default_config()).expect("seed is JSON");
    assert_eq!(seed["LdapServer"], "", "D3: no placeholder host");
    assert_eq!(seed["LdapPort"], 389);
    assert_eq!(seed["UseSsl"], true);
    assert_eq!(seed["LdapBindUser"], "CN=BindUser,DC=contoso,DC=com");
    assert_eq!(seed["LdapBindPassword"], "password");
    assert_eq!(
        seed["LdapAdminFilter"],
        "(enabledService=JellyfinAdministrator)"
    );
    assert_eq!(seed["LdapSearchAttributes"], "uid, cn, mail, displayName");
    assert_eq!(seed["CreateUsersFromLdap"], true);
    assert_eq!(seed["LdapProfileImageFormat"], "Default");
    assert_eq!(seed["EnabledFolders"], serde_json::json!([]));
    assert!(
        seed["PasswordResetUrl"].is_null() && seed.get("PasswordResetUrl").is_some(),
        "the key is in the schema, so a save keeps it"
    );
    assert!(
        seed.get("LdapUsers").is_none(),
        "D12: the uid map is not config"
    );
    assert_eq!(seed.as_object().map(serde_json::Map::len), Some(28));
}

#[test]
fn a_stored_null_reads_as_empty_and_a_bad_image_format_as_default() {
    let config: LdapConfig = serde_json::from_str(
        r#"{"LdapServer":null,"LdapPort":null,"UseSsl":null,"LdapBindUser":null,
            "LdapBindPassword":null,"EnabledFolders":null,"PasswordResetUrl":null,
            "LdapProfileImageFormat":"nonsense"}"#,
    )
    .expect("reads");
    assert_eq!(config.ldap_server, "");
    assert_eq!(config.ldap_port, 0);
    assert!(!config.use_ssl);
    assert_eq!(
        config.ldap_bind_user, "",
        "null, not upstream's placeholder DN"
    );
    assert_eq!(config.ldap_bind_password.expose(), "");
    assert_eq!(config.enabled_folders, Vec::<String>::new());
    assert_eq!(config.password_reset_url, None);
    assert_eq!(
        config.ldap_profile_image_format,
        ProfileImageFormat::Default
    );
    // A key that is absent keeps upstream's default.
    assert_eq!(config.ldap_uid_attribute, "uid");
}

#[rstest]
#[case(serde_json::json!("binary"), ProfileImageFormat::Binary)]
#[case(serde_json::json!("Base64"), ProfileImageFormat::Base64)]
#[case(serde_json::json!("URL"), ProfileImageFormat::Url)]
#[case(serde_json::json!(3), ProfileImageFormat::Url)]
#[case(serde_json::json!(1), ProfileImageFormat::Binary)]
#[case(serde_json::json!(2), ProfileImageFormat::Base64)]
#[case(serde_json::json!(0), ProfileImageFormat::Default)]
#[case(serde_json::json!(true), ProfileImageFormat::Default)]
fn the_image_format_reads_by_name_or_number(
    #[case] stored: serde_json::Value,
    #[case] expected: ProfileImageFormat,
) {
    let config: LdapConfig =
        serde_json::from_value(serde_json::json!({ "LdapProfileImageFormat": stored }))
            .expect("reads");
    assert_eq!(config.ldap_profile_image_format, expected);
}

#[test]
fn applying_a_connection_writes_its_thirteen_members_and_keeps_the_rest() {
    let mut config = LdapConfig {
        ldap_search_filter: "(objectClass=person)".to_owned(),
        password_reset_url: Some("https://old.example/reset".to_owned()),
        ..LdapConfig::default()
    };
    config.apply_connection(ServerConnectionInfo {
        ldap_server: Some("ldap.example".to_owned()),
        ldap_port: 636,
        use_ssl: true,
        use_start_tls: false,
        skip_ssl_verify: true,
        allow_pass_change: true,
        ldap_bind_user: None,
        ldap_bind_password: Some(Secret::new("s3cret")),
        ldap_base_dn: Some("dc=example".to_owned()),
        password_reset_url: None,
        ldap_client_cert_path: Some("/c.pem".to_owned()),
        ldap_client_key_path: None,
        ldap_root_ca_path: Some("/ca.pem".to_owned()),
    });
    assert_eq!(config.ldap_server, "ldap.example");
    assert_eq!(config.ldap_port, 636);
    assert!(config.use_ssl && config.skip_ssl_verify && config.allow_pass_change);
    assert_eq!(
        config.ldap_bind_user, "",
        "an absent member is upstream's empty default"
    );
    assert_eq!(config.ldap_bind_password.expose(), "s3cret");
    assert_eq!(config.ldap_base_dn, "dc=example");
    assert_eq!(
        config.password_reset_url, None,
        "upstream copies the null over"
    );
    assert_eq!(config.ldap_client_cert_path, "/c.pem");
    assert_eq!(config.ldap_client_key_path, "");
    assert_eq!(config.ldap_root_ca_path, "/ca.pem");
    assert_eq!(
        config.ldap_search_filter, "(objectClass=person)",
        "not a connection member"
    );
}

#[test]
fn the_settings_page_is_upstreams_named_after_the_plugin() {
    let pages = LdapAuthExtension.config_pages();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].name, "LDAP-Auth");
    assert!(!pages[0].enable_in_main_menu);
    let html = String::from_utf8(pages[0].bytes.clone()).expect("utf8");
    assert!(
        html.contains("958aad66-3784-4d2a-b89a-a7b6fab6e25c"),
        "the page's plugin id"
    );
    assert!(html.contains("ldap/TestServerBind"));
}

#[test]
fn the_descriptor_is_upstreams_plugin_disabled() {
    let descriptor = LdapAuthExtension.descriptor();
    assert_eq!(descriptor.id, EXTENSION_ID);
    assert_eq!(descriptor.name, "LDAP-Auth");
    assert_eq!(descriptor.version, "24.0.0.0");
    assert!(!descriptor.enabled);
    assert_eq!(
        descriptor.configuration_file_name.as_deref(),
        Some("LDAP-Auth.xml")
    );
}

/// The real plugin manager holding just LDAP-Auth, under `dir`.
fn plugin_manager(dir: &std::path::Path) -> Arc<FerrofinPluginManager> {
    let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(LdapAuthExtension)];
    Arc::new(FerrofinPluginManager::new(
        registered_plugins(&extensions),
        dir.to_path_buf(),
    ))
}

fn connection_info() -> ServerConnectionInfo {
    ServerConnectionInfo {
        ldap_server: Some("ldap.example".to_owned()),
        ldap_port: 389,
        ldap_bind_user: Some("cn=reader,dc=example,dc=org".to_owned()),
        ldap_bind_password: Some(Secret::new("reader-secret")),
        ldap_base_dn: Some("dc=example,dc=org".to_owned()),
        password_reset_url: Some("https://id.example/reset".to_owned()),
        ..ServerConnectionInfo::default()
    }
}

#[tokio::test]
async fn while_disabled_the_service_neither_saves_nor_connects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugins = plugin_manager(dir.path());
    let directory = FakeDirectory::default();
    let service = LdapAuthService::new(plugins.clone(), Arc::new(directory.clone()));

    assert!(matches!(
        service.ensure_enabled().await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(matches!(
        service.test_server_bind(connection_info()).await,
        Err(ServiceError::NotFound(_))
    ));
    assert_eq!(directory.calls(), Calls::default(), "no network call");
    assert_eq!(
        service.config().await.expect("config"),
        LdapConfig::default(),
        "nothing saved"
    );
}

#[tokio::test]
async fn once_enabled_the_test_saves_the_connection_and_tests_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugins = plugin_manager(dir.path());
    plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
    let directory = FakeDirectory::answering(Script {
        search: Ok(entries(5)),
        ..Script::default()
    });
    let service = LdapAuthService::new(plugins.clone(), Arc::new(directory.clone()));

    service.ensure_enabled().await.expect("enabled");
    let response = service
        .test_server_bind(connection_info())
        .await
        .expect("tested");
    assert_eq!(response.base_search, step("Found 5 Entities"));
    assert_eq!(directory.calls().connects[0].server, "ldap.example");

    // Saved through the plugin manager, as `GET /Plugins/{id}/Configuration`
    // reads it back.
    let saved: serde_json::Value = serde_json::from_slice(
        &plugins
            .get_plugin_configuration(EXTENSION_ID)
            .await
            .expect("config"),
    )
    .expect("json");
    assert_eq!(saved["LdapServer"], "ldap.example");
    assert_eq!(saved["UseSsl"], false);
    assert_eq!(saved["LdapBindPassword"], "reader-secret");
    assert_eq!(saved["PasswordResetUrl"], "https://id.example/reset");
    assert_eq!(
        saved["LdapUidAttribute"], "uid",
        "other members keep their value"
    );
    assert!(saved.get("LdapUsers").is_none());
}

/// A stored configuration that does not parse is a server error whose text
/// names the kind of error, never the value: serde would quote a bind
/// password stored as a number.
#[tokio::test]
async fn an_unreadable_configuration_is_an_error_without_its_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugins = plugin_manager(dir.path());
    let config_dir = dir.path().join(EXTENSION_ID.to_string());
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.json"),
        br#"{"LdapBindPassword": 9876543210}"#,
    )
    .expect("write");
    let service = LdapAuthService::new(plugins, Arc::new(FakeDirectory::default()));
    match service.config().await {
        Err(ServiceError::Backend(message)) => {
            assert!(message.contains("Data error"), "{message}");
            assert!(!message.contains("9876543210"), "{message}");
        }
        other => panic!("expected a backend error, got {other:?}"),
    }
}
