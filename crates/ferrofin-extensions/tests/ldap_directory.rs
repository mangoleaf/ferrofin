//! The LDAP-Auth directory client and sign-in against a real directory:
//! OpenLDAP 2.6 in a container, over plain LDAP, LDAPS and StartTLS.
//!
//! Hand-run, like the ffmpeg gates; CI does not run it (D8):
//!
//! ```bash
//! FERROFIN_LDAP_TESTS=1 cargo test -p ferrofin-extensions --test ldap_directory -- --nocapture
//! ```
//!
//! It needs `docker`. The first run builds a small image from `alpine:3.22`
//! with Alpine's OpenLDAP packages; certificates are generated per run under
//! the target directory. Two servers start: one that takes any client, and one
//! that demands a client certificate. Without `FERROFIN_LDAP_TESTS=1` the test
//! says so and passes. Later phases of the LDAP-Auth port add their cases here.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_core::{FerrofinPluginManager, FerrofinUserManager};
use ferrofin_extensions::ldap_auth::directory::Ldap3Directory;
use ferrofin_extensions::ldap_auth::provider::{LdapAuthProvider, PROVIDER_ID, PROVIDER_NAME};
use ferrofin_extensions::ldap_auth::{
    CertificateVerification, ConnectionOptions, EXTENSION_ID, LdapAuthExtension, LdapConfig,
    LdapDirectory, SearchScope, test_server_bind,
};
use ferrofin_extensions::{Extension, registered_plugins};
use ferrofin_model::ldap::ServerTestResponse;
use ferrofin_model::secret::Secret;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::library::UserManager;
use ferrofin_traits::plugins::PluginManager;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

/// The image the directories run from: Alpine's OpenLDAP.
const IMAGE: &str = "ferrofin-ldap-test:alpine3.22-openldap";
const DOCKERFILE: &str = "FROM alpine:3.22\n\
     RUN apk add --no-cache openldap openldap-back-mdb openldap-clients\n";

/// The directory's suffix and its entries' passwords.
const BASE_DN: &str = "dc=example,dc=org";
const ADMIN_DN: &str = "cn=admin,dc=example,dc=org";
const ADMIN_PASSWORD: &str = "admin-secret";
const READER_DN: &str = "uid=reader,ou=people,dc=example,dc=org";
const READER_PASSWORD: &str = "reader-secret";
const ADA_DN: &str = "uid=ada,ou=people,dc=example,dc=org";
const ADA_PASSWORD: &str = "ada-secret";

/// An address no host answers (RFC 5737 TEST-NET-1), for the connect timeout.
/// On a network that answers it with "unreachable" at once, the case still
/// passes but no longer exercises the timeout: read its printed time.
const UNROUTABLE: &str = "192.0.2.1";

/// The RFC 3062 password modify operation's OID.
const PASSWORD_MODIFY_OID: &str = "1.3.6.1.4.1.4203.1.11.1";

const SEED: &str = "dn: dc=example,dc=org
objectClass: top
objectClass: dcObject
objectClass: organization
o: Example
dc: example

dn: ou=people,dc=example,dc=org
objectClass: organizationalUnit
ou: people

dn: uid=reader,ou=people,dc=example,dc=org
objectClass: inetOrgPerson
uid: reader
cn: reader
sn: reader
userPassword: reader-secret

dn: uid=ada,ou=people,dc=example,dc=org
objectClass: inetOrgPerson
uid: ada
cn: Ada Lovelace
sn: Lovelace
mail: ada@example.org
userPassword: ada-secret
";

/// The four entries [`SEED`] creates.
const SEEDED_ENTRIES: usize = 4;

const START: &str = "set -e
mkdir -p /tmp/ldapdata
slapadd -f /conf/slapd.conf -l /conf/seed.ldif
exec slapd -d 0 -f /conf/slapd.conf -h 'ldap:/// ldaps:///'
";

fn slapd_conf(verify_client: &str) -> String {
    format!(
        "include /etc/openldap/schema/core.schema
include /etc/openldap/schema/cosine.schema
include /etc/openldap/schema/inetorgperson.schema
modulepath /usr/lib/openldap
moduleload back_mdb.so
pidfile /tmp/slapd.pid
argsfile /tmp/slapd.args
TLSCACertificateFile /conf/client-ca.pem
TLSCertificateFile /conf/server.pem
TLSCertificateKeyFile /conf/server.key
TLSVerifyClient {verify_client}
allow bind_anon_dn
database mdb
maxsize 104857600
suffix \"{BASE_DN}\"
rootdn \"{ADMIN_DN}\"
rootpw {ADMIN_PASSWORD}
directory /tmp/ldapdata
access to attrs=userPassword by self write by anonymous auth by * none
access to * by * read
"
    )
}

/// A CA's certificate and issuer.
struct Ca {
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

fn ca(name: &str) -> Ca {
    let key = KeyPair::generate().expect("ca key");
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params.self_signed(&key).expect("ca certificate");
    Ca {
        pem: certificate.pem(),
        issuer: Issuer::new(params, key),
    }
}

/// A certificate `ca` issues for `name`, and its key, as PEM.
fn issue(ca: &Ca, name: &str, purpose: ExtendedKeyUsagePurpose) -> (String, String) {
    let key = KeyPair::generate().expect("key");
    let mut params = CertificateParams::new(vec![name.to_owned()]).expect("params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.extended_key_usages = vec![purpose];
    let certificate = params.signed_by(&key, &ca.issuer).expect("certificate");
    (certificate.pem(), key.serialize_pem())
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write");
}

/// Runs `docker args…`, returning its standard output.
fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker runs");
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn build_image() {
    let mut build = Command::new("docker")
        .args(["build", "-q", "-t", IMAGE, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("docker build");
    build
        .stdin
        .take()
        .expect("stdin")
        .write_all(DOCKERFILE.as_bytes())
        .expect("dockerfile");
    assert!(
        build.wait().expect("docker build").success(),
        "docker build failed"
    );
}

/// A directory container, removed when dropped.
struct Directory {
    id: String,
    ldap_port: u16,
    ldaps_port: u16,
}

impl Directory {
    fn start(conf: &Path) -> Self {
        let mount = format!("{}:/conf:ro", conf.display());
        let id = docker(&[
            "run",
            "-d",
            "--rm",
            "-p",
            "127.0.0.1::389",
            "-p",
            "127.0.0.1::636",
            "-v",
            &mount,
            IMAGE,
            "sh",
            "/conf/start.sh",
        ]);
        let port = |container_port: &str| -> u16 {
            docker(&["port", &id, container_port])
                .lines()
                .next()
                .and_then(|line| line.rsplit(':').next())
                .and_then(|port| port.parse().ok())
                .expect("mapped port")
        };
        Self {
            ldap_port: port("389/tcp"),
            ldaps_port: port("636/tcp"),
            id,
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The certificate files under `dir`, as the configuration names them.
struct Files {
    root_ca: String,
    client_cert: String,
    client_key: String,
}

/// Writes both servers' configurations and the certificates into `conf`
/// (shared) and `mtls` (the client-certificate server's).
fn prepare(conf: &Path, mtls: &Path) -> Files {
    let server_ca = ca("Ferrofin LDAP test CA");
    let client_ca = ca("Ferrofin LDAP test client CA");
    // The server's certificate names only `localhost`: connecting by
    // 127.0.0.1 is the wrong-name case.
    let (server_pem, server_key) =
        issue(&server_ca, "localhost", ExtendedKeyUsagePurpose::ServerAuth);
    let (client_pem, client_key) =
        issue(&client_ca, "ferrofin", ExtendedKeyUsagePurpose::ClientAuth);
    for (dir, verify) in [(conf, "never"), (mtls, "demand")] {
        write(dir, "slapd.conf", &slapd_conf(verify));
        write(dir, "seed.ldif", SEED);
        write(dir, "start.sh", START);
        write(dir, "server.pem", &server_pem);
        write(dir, "server.key", &server_key);
        write(dir, "client-ca.pem", &client_ca.pem);
    }
    write(conf, "root-ca.pem", &server_ca.pem);
    write(conf, "client.pem", &client_pem);
    write(conf, "client.key", &client_key);
    Files {
        root_ca: conf.join("root-ca.pem").display().to_string(),
        client_cert: conf.join("client.pem").display().to_string(),
        client_key: conf.join("client.key").display().to_string(),
    }
}

fn plain_config(port: u16) -> LdapConfig {
    LdapConfig {
        ldap_server: "localhost".to_owned(),
        ldap_port: i32::from(port),
        use_ssl: false,
        use_start_tls: false,
        ldap_bind_user: READER_DN.to_owned(),
        ldap_bind_password: Secret::new(READER_PASSWORD),
        ldap_base_dn: BASE_DN.to_owned(),
        ..LdapConfig::default()
    }
}

/// Waits until the server at `port` answers an anonymous bind.
async fn wait_until_up(port: u16) {
    let started = Instant::now();
    let options = ConnectionOptions::from_config(&plain_config(port));
    loop {
        if let Ok(mut connection) = Ldap3Directory::default().connect(&options).await
            && connection.simple_bind("", "").await.is_ok()
        {
            connection.unbind().await;
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the directory never came up"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn found_all() -> Option<String> {
    format!("Found {SEEDED_ENTRIES} Entities").into()
}

/// Runs the connection test with `config`, printing the answer.
async fn run(case: &str, config: &LdapConfig) -> ServerTestResponse {
    let started = Instant::now();
    let response = test_server_bind(&Ldap3Directory::default(), config).await;
    println!(
        "{case}: {} in {:.2?}",
        serde_json::to_string(&response).expect("json"),
        started.elapsed()
    );
    response
}

fn success() -> Option<String> {
    "Success".to_owned().into()
}

fn testing() -> Option<String> {
    "Testing...".to_owned().into()
}

#[tokio::test]
async fn the_directory_client_against_openldap() {
    if std::env::var("FERROFIN_LDAP_TESTS").as_deref() != Ok("1") {
        eprintln!(
            "skipping the live LDAP directory test: set FERROFIN_LDAP_TESTS=1 (needs docker)"
        );
        return;
    }
    let scratch = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("scratch");
    let conf = scratch.path().join("conf");
    let mtls_conf = scratch.path().join("mtls");
    std::fs::create_dir_all(&conf).expect("conf dir");
    std::fs::create_dir_all(&mtls_conf).expect("mtls dir");
    let files = prepare(&conf, &mtls_conf);
    build_image();
    let open = Directory::start(&conf);
    let mtls = Directory::start(&mtls_conf);
    wait_until_up(open.ldap_port).await;

    // Plain LDAP: every step.
    let plain = plain_config(open.ldap_port);
    plain_ldap(&plain).await;

    // LDAPS and StartTLS with the custom root CA.
    let ldaps = LdapConfig {
        ldap_port: i32::from(open.ldaps_port),
        use_ssl: true,
        ldap_root_ca_path: files.root_ca.clone(),
        ..plain.clone()
    };
    let start_tls = LdapConfig {
        use_ssl: false,
        use_start_tls: true,
        ldap_root_ca_path: files.root_ca.clone(),
        ..plain.clone()
    };
    tls(&ldaps, &start_tls).await;

    // The server that demands a client certificate.
    wait_until_up_tls(&mtls, &files).await;
    client_certificates(&mtls, &files, &ldaps, &start_tls).await;

    unroutable(&plain).await;
    sign_in(&open, &plain).await;
    directory_operations(&plain).await;
}

/// Plain LDAP: every step, an anonymous bind, a wrong bind password and a
/// base DN the directory does not hold.
async fn plain_ldap(plain: &LdapConfig) {
    let response = run("plain", plain).await;
    assert_eq!(response.connect, success());
    assert_eq!(response.start_tls, None);
    assert_eq!(response.bind, success());
    assert_eq!(response.base_search, found_all());
    assert_eq!(response.error, None);

    // No bind password: an anonymous bind.
    let response = run(
        "anonymous",
        &LdapConfig {
            ldap_bind_password: Secret::default(),
            ..plain.clone()
        },
    )
    .await;
    assert_eq!(response.bind, Some("Anonymous".to_owned()));
    assert_eq!(response.base_search, found_all());

    // A wrong bind password stops at the bind.
    let response = run(
        "wrong password",
        &LdapConfig {
            ldap_bind_password: Secret::new("not-the-password"),
            ..plain.clone()
        },
    )
    .await;
    assert_eq!(response.bind, testing());
    assert_eq!(response.error.as_deref(), Some("Invalid Credentials"));
    assert_eq!(response.base_search, None);

    // A base DN the directory does not hold stops at the search.
    let response = run(
        "missing base",
        &LdapConfig {
            ldap_base_dn: "dc=missing,dc=org".to_owned(),
            ..plain.clone()
        },
    )
    .await;
    assert_eq!(response.base_search, testing());
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("No Such Object")),
        "{response:?}"
    );
}

/// LDAPS and StartTLS with the custom root CA, and the certificate checks.
async fn tls(ldaps: &LdapConfig, start_tls: &LdapConfig) {
    let response = run("ldaps, custom root CA", ldaps).await;
    assert_eq!(response.connect, success());
    assert_eq!(response.base_search, found_all());
    let response = run("starttls, custom root CA", start_tls).await;
    assert_eq!(response.start_tls, success());
    assert_eq!(response.base_search, found_all());

    // The certificate names `localhost`, not 127.0.0.1: refused, even from
    // the trusted root, at whichever step does the handshake.
    let response = run(
        "ldaps, wrong name",
        &LdapConfig {
            ldap_server: "127.0.0.1".to_owned(),
            ..ldaps.clone()
        },
    )
    .await;
    assert_eq!(response.connect, testing());
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("not valid for name")),
        "{response:?}"
    );
    let response = run(
        "starttls, wrong name",
        &LdapConfig {
            ldap_server: "127.0.0.1".to_owned(),
            ..start_tls.clone()
        },
    )
    .await;
    assert_eq!(response.connect, success());
    assert_eq!(response.start_tls, testing());
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("not valid for name")),
        "{response:?}"
    );

    // Without the root CA the system roots do not know the issuer.
    let response = run(
        "ldaps, system roots",
        &LdapConfig {
            ldap_root_ca_path: String::new(),
            ..ldaps.clone()
        },
    )
    .await;
    assert_eq!(response.connect, testing());
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("UnknownIssuer")),
        "{response:?}"
    );

    // Skip-verify takes the certificate whatever its name and issuer.
    let response = run(
        "ldaps, skip verify",
        &LdapConfig {
            ldap_server: "127.0.0.1".to_owned(),
            ldap_root_ca_path: String::new(),
            skip_ssl_verify: true,
            ..ldaps.clone()
        },
    )
    .await;
    assert_eq!(response.base_search, found_all());

    // Both LDAPS and StartTLS: StartTLS is still sent, and refused.
    let response = run(
        "ldaps and starttls",
        &LdapConfig {
            use_start_tls: true,
            ..ldaps.clone()
        },
    )
    .await;
    assert_eq!(response.connect, success());
    assert_eq!(response.start_tls, testing());
    assert!(response.error.is_some());
}

/// The server that demands a client certificate, with and without one.
async fn client_certificates(
    mtls: &Directory,
    files: &Files,
    ldaps: &LdapConfig,
    start_tls: &LdapConfig,
) {
    let with_client_cert = LdapConfig {
        ldap_port: i32::from(mtls.ldaps_port),
        ldap_client_cert_path: files.client_cert.clone(),
        ldap_client_key_path: files.client_key.clone(),
        ..ldaps.clone()
    };
    let response = run("ldaps, client certificate", &with_client_cert).await;
    assert_eq!(response.base_search, found_all());
    let response = run(
        "starttls, client certificate",
        &LdapConfig {
            ldap_port: i32::from(mtls.ldap_port),
            ..start_tls.clone()
        }
        .with_client_certificate(files),
    )
    .await;
    assert_eq!(response.base_search, found_all());
    let response = run(
        "ldaps, no client certificate",
        &LdapConfig {
            ldap_client_cert_path: String::new(),
            ldap_client_key_path: String::new(),
            ..with_client_cert.clone()
        },
    )
    .await;
    assert!(response.error.is_some(), "{response:?}");
    assert_eq!(response.base_search, None);
}

/// An address nobody answers fails the connect step in about 5 seconds.
async fn unroutable(plain: &LdapConfig) {
    let started = Instant::now();
    let response = run(
        "unroutable",
        &LdapConfig {
            ldap_server: UNROUTABLE.to_owned(),
            ldap_port: 389,
            ..plain.clone()
        },
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(response.connect, testing());
    assert!(response.error.is_some());
    assert!(elapsed < Duration::from_secs(7), "{elapsed:?}");
}

/// Waits until the client-certificate server answers over LDAPS.
async fn wait_until_up_tls(directory: &Directory, files: &Files) {
    let started = Instant::now();
    let config = LdapConfig {
        ldap_port: i32::from(directory.ldaps_port),
        use_ssl: true,
        ldap_root_ca_path: files.root_ca.clone(),
        ..plain_config(directory.ldaps_port)
    }
    .with_client_certificate(files);
    let options = ConnectionOptions::from_config(&config);
    loop {
        if let Ok(mut connection) = Ldap3Directory::default().connect(&options).await
            && connection.simple_bind("", "").await.is_ok()
        {
            connection.unbind().await;
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the mTLS directory never came up"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

trait WithClientCertificate {
    fn with_client_certificate(self, files: &Files) -> Self;
}

impl WithClientCertificate for LdapConfig {
    fn with_client_certificate(self, files: &Files) -> Self {
        Self {
            ldap_client_cert_path: files.client_cert.clone(),
            ldap_client_key_path: files.client_key.clone(),
            ..self
        }
    }
}

/// The seam's other operations, which later phases build on: search with
/// attributes, modify, the RFC 3062 password modify and the root DSE.
async fn directory_operations(plain: &LdapConfig) {
    let options = ConnectionOptions::from_config(plain);
    assert_eq!(options.verification, CertificateVerification::System);
    let mut admin = Ldap3Directory::default()
        .connect(&options)
        .await
        .expect("connect");
    admin
        .simple_bind(ADMIN_DN, ADMIN_PASSWORD)
        .await
        .expect("admin bind");

    let found = admin
        .search(BASE_DN, SearchScope::Subtree, "(uid=ada)", &["cn", "mail"])
        .await
        .expect("search");
    assert_eq!(found.entries.len(), 1);
    assert_eq!(found.entries[0].dn, ADA_DN);
    assert_eq!(
        found.entries[0].attribute("CN").map(|a| a.values.clone()),
        Some(vec![b"Ada Lovelace".to_vec()])
    );

    admin
        .modify_replace(ADA_DN, "mail", &["ada@example.net"])
        .await
        .expect("modify");
    let found = admin
        .search(ADA_DN, SearchScope::Base, "(objectClass=*)", &["mail"])
        .await
        .expect("search");
    assert_eq!(
        found.entries[0].attribute("mail").map(|a| a.values.clone()),
        Some(vec![b"ada@example.net".to_vec()])
    );

    let extensions = admin.supported_extensions().await.expect("root dse");
    println!("supportedExtension: {extensions:?}");
    assert!(extensions.iter().any(|oid| oid == PASSWORD_MODIFY_OID));
    admin
        .password_modify(ADA_DN, "ada-new-secret")
        .await
        .expect("password modify");
    admin.unbind().await;

    let mut ada = Ldap3Directory::default()
        .connect(&options)
        .await
        .expect("connect");
    assert!(
        ada.simple_bind(ADA_DN, "ada-secret").await.is_err(),
        "the old password is gone"
    );
    ada.simple_bind(ADA_DN, "ada-new-secret")
        .await
        .expect("the new password binds");
    ada.unbind().await;
    println!("search, modify, password modify and the root DSE: ok");
}

/// The user manager with LDAP-Auth enabled against the directory `config`
/// names, over an in-memory database.
struct SignIn {
    users: Arc<FerrofinUserManager>,
    identities: Arc<FerrofinExternalIdentityStore>,
    _plugins_dir: tempfile::TempDir,
}

impl SignIn {
    async fn new(config: &LdapConfig) -> Self {
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
        db.run_migrations().await.expect("migrations");
        let users = Arc::new(FerrofinUserManager::new(db.clone()));
        let identities = Arc::new(FerrofinExternalIdentityStore::new(db));
        let plugins_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("plugins");
        let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(LdapAuthExtension)];
        let plugins = Arc::new(FerrofinPluginManager::new(
            registered_plugins(&extensions),
            plugins_dir.path().to_path_buf(),
        ));
        plugins.enable_plugin(EXTENSION_ID).await.expect("enable");
        plugins
            .set_plugin_configuration(EXTENSION_ID, serde_json::to_vec(config).expect("json"))
            .await
            .expect("configure");
        users.set_extension_providers(vec![Arc::new(LdapAuthProvider::new(
            plugins,
            Arc::clone(&users) as Arc<dyn UserManager>,
            Arc::clone(&identities) as Arc<dyn ExternalIdentityStore>,
            Arc::new(Ldap3Directory::default()),
        ))]);
        Self {
            users,
            identities,
            _plugins_dir: plugins_dir,
        }
    }

    async fn sign_in(
        &self,
        name: &str,
        password: &str,
    ) -> Result<Option<ferrofin_db::entities::users::UserEntity>, ServiceError> {
        let started = Instant::now();
        let result = self
            .users
            .authenticate_user(name, password, "127.0.0.1", true)
            .await;
        println!(
            "sign in as {name:?}: {} in {:.2?}",
            match &result {
                Ok(Some(user)) => format!("signed in as {:?}", user.username),
                Ok(None) => "refused (counted)".to_owned(),
                Err(error) => format!("{error}"),
            },
            started.elapsed()
        );
        result
    }

    async fn user(&self, name: &str) -> Option<ferrofin_db::entities::users::UserEntity> {
        self.users.get_user_by_name(name).await.expect("lookup")
    }
}

/// Sign-in through LDAP-Auth (`Authenticate`): an empty password at a
/// directory that accepts unauthenticated binds, a first sign-in creating the
/// user, a wrong password, a directory rename, an adopted user relinked by
/// name, and a directory that stops answering.
async fn sign_in(open: &Directory, plain: &LdapConfig) {
    let config = LdapConfig {
        ldap_search_filter: "(objectClass=inetOrgPerson)".to_owned(),
        ldap_search_attributes: "uid, cn, mail".to_owned(),
        ldap_uid_attribute: "uid".to_owned(),
        ldap_username_attribute: "cn".to_owned(),
        enable_all_folders: true,
        ..plain.clone()
    };
    let users = SignIn::new(&config).await;
    let listed: Vec<_> = users
        .users
        .get_authentication_providers()
        .await
        .expect("providers")
        .into_iter()
        .filter_map(|pair| pair.id.zip(pair.name))
        .collect();
    assert!(
        listed.contains(&(PROVIDER_ID.to_owned(), PROVIDER_NAME.to_owned())),
        "{listed:?}"
    );
    let options = ConnectionOptions::from_config(plain);
    empty_password(&users, &options).await;
    first_sign_in_and_rename(&users, &options).await;
    adopted_user(&users).await;
    outage(&users, open).await;
    println!("sign-in: ok");
}

/// This directory takes a DN with an empty password as an unauthenticated
/// bind (`allow bind_anon_dn`); sign-in never sends one.
async fn empty_password(users: &SignIn, options: &ConnectionOptions) {
    let mut raw = Ldap3Directory::default()
        .connect(options)
        .await
        .expect("connect");
    raw.simple_bind(ADA_DN, "")
        .await
        .expect("the directory accepts an unauthenticated bind");
    raw.unbind().await;
    assert!(matches!(
        users.sign_in("ada", "").await,
        Err(ServiceError::Unauthorized(_))
    ));
    assert_eq!(users.user("Ada Lovelace").await, None, "no account");
}

/// A first sign-in creates Ada, named by her cn and linked by her uid; a
/// wrong password under her Ferrofin name is a counted failure; a directory
/// rename is followed at the next sign-in.
async fn first_sign_in_and_rename(users: &SignIn, options: &ConnectionOptions) {
    let ada = users
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("created");
    assert_eq!(ada.username, "Ada Lovelace");
    assert_eq!(ada.authentication_provider_id, PROVIDER_ID);
    assert_eq!(ada.password_reset_provider_id, PROVIDER_ID);
    let key = ExternalIdentityKey {
        provider_id: PROVIDER_ID,
        issuer: "",
        subject: "ada",
    };
    assert_eq!(
        users
            .identities
            .find(key)
            .await
            .expect("find")
            .map(|link| link.user_id.to_string().to_uppercase()),
        Some(ada.id.clone())
    );

    assert_eq!(
        users
            .sign_in("Ada Lovelace", "wrong")
            .await
            .expect("sign in"),
        None
    );
    assert_eq!(
        users
            .user("Ada Lovelace")
            .await
            .expect("ada")
            .invalid_login_attempt_count,
        1
    );

    let mut admin = Ldap3Directory::default()
        .connect(options)
        .await
        .expect("connect");
    admin
        .simple_bind(ADMIN_DN, ADMIN_PASSWORD)
        .await
        .expect("admin bind");
    admin
        .modify_replace(ADA_DN, "cn", &["Ada King"])
        .await
        .expect("rename");
    let renamed = users
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(renamed.id, ada.id);
    assert_eq!(renamed.username, "Ada King");
    admin
        .modify_replace(ADA_DN, "cn", &["Ada Lovelace"])
        .await
        .expect("rename back");
    admin.unbind().await;
    // Back under her first name for the cases that follow.
    users
        .sign_in("ada", ADA_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
}

/// An adopted user saved with the upstream provider id, never linked, signs
/// in and is linked by name.
async fn adopted_user(users: &SignIn) {
    let reader = users.users.create_user("reader").await.expect("create");
    let reader_id = uuid::Uuid::parse_str(&reader.id).expect("id");
    let mut policy = users
        .users
        .get_user_dto(&reader, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    PROVIDER_ID.clone_into(&mut policy.authentication_provider_id);
    users
        .users
        .update_policy(reader_id, &policy)
        .await
        .expect("policy");
    let relinked = users
        .sign_in("reader", READER_PASSWORD)
        .await
        .expect("sign in")
        .expect("signed in");
    assert_eq!(relinked.id, reader.id);
}

/// A directory that stops answering (the container paused): a server error
/// within the operation timeout, not counted, while a local user signs in
/// before it gives up.
async fn outage(users: &SignIn, open: &Directory) {
    let local = users.users.create_user("carol").await.expect("create");
    users
        .users
        .change_password(
            uuid::Uuid::parse_str(&local.id).expect("id"),
            "carol-secret",
        )
        .await
        .expect("password");
    let attempts = |user: Option<ferrofin_db::entities::users::UserEntity>| {
        user.expect("ada").invalid_login_attempt_count
    };
    // One counted failure first, so "unchanged" is not just zero.
    assert_eq!(
        users
            .sign_in("Ada Lovelace", "wrong")
            .await
            .expect("sign in"),
        None
    );
    let before = attempts(users.user("Ada Lovelace").await);
    assert_eq!(before, 1);
    docker(&["pause", &open.id]);
    let started = Instant::now();
    let (directory_user, local_user) = tokio::join!(
        async {
            let result = users.sign_in("Ada Lovelace", ADA_PASSWORD).await;
            (result, Instant::now())
        },
        async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let result = users.sign_in("carol", "carol-secret").await;
            (result, Instant::now())
        }
    );
    let elapsed = started.elapsed();
    docker(&["unpause", &open.id]);
    assert!(
        matches!(directory_user.0, Err(ServiceError::Backend(_))),
        "{:?}",
        directory_user.0
    );
    assert!(elapsed < Duration::from_secs(12), "{elapsed:?}");
    assert!(local_user.0.expect("sign in").is_some());
    assert!(
        local_user.1 < directory_user.1,
        "the local user did not wait for the directory"
    );
    assert_eq!(
        attempts(users.user("Ada Lovelace").await),
        before,
        "the outage is not counted"
    );
    wait_until_up(open.ldap_port).await;
}
