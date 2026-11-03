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
//! that demands a client certificate; the referral case starts two more, one
//! referring to the other. Without `FERROFIN_LDAP_TESTS=1` the test says so and
//! passes. Later phases of the LDAP-Auth port add their cases here.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrofin_core::external_identity_repository::FerrofinExternalIdentityStore;
use ferrofin_core::{FerrofinPluginManager, FerrofinUserManager};
use ferrofin_extensions::ldap_auth::directory::Ldap3Directory;
use ferrofin_extensions::ldap_auth::provider::{LdapAuthProvider, PROVIDER_ID, PROVIDER_NAME};
use ferrofin_extensions::ldap_auth::{
    CertificateVerification, ConnectionOptions, EXTENSION_ID, LdapAuthExtension, LdapConfig,
    LdapConnection, LdapDirectory, LdapError, SearchScope, ldap_user_search, test_ldap_filters,
    test_server_bind,
};
use ferrofin_extensions::{Extension, registered_plugins};
use ferrofin_model::ldap::{
    LdapFilterResponse, LdapTestErrorResponse, ServerTestResponse, UserSearchResponse,
};
use ferrofin_model::secret::Secret;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{ExternalIdentityKey, ExternalIdentityStore};
use ferrofin_traits::ldap::LdapTestAnswer;
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
const GROUPS_DN: &str = "ou=groups,dc=example,dc=org";

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
employeeType: jellyfin-admin
userPassword: ada-secret

dn: ou=groups,dc=example,dc=org
objectClass: organizationalUnit
ou: groups

dn: cn=jellyfin-admins,ou=groups,dc=example,dc=org
objectClass: groupOfNames
cn: jellyfin-admins
member: uid=ada,ou=people,dc=example,dc=org
";

/// The six entries [`SEED`] creates.
const SEEDED_ENTRIES: usize = 6;

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
    admin_filter(&plain).await;
    filter_test_and_user_search(&plain).await;
    directory_operations(&plain).await;
    referrals(&files, &conf, scratch.path()).await;
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

/// Connects and binds as the directory's administrator, to change entries.
async fn directory_admin(
    plain: &LdapConfig,
) -> Box<dyn ferrofin_extensions::ldap_auth::LdapConnection> {
    let mut admin = Ldap3Directory::default()
        .connect(&ConnectionOptions::from_config(plain))
        .await
        .expect("connect");
    admin
        .simple_bind(ADMIN_DN, ADMIN_PASSWORD)
        .await
        .expect("admin bind");
    admin
}

/// Whether the Ferrofin user `name` is an administrator.
async fn is_administrator(users: &SignIn, name: &str) -> bool {
    let user = users.user(name).await.expect("user");
    users
        .users
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .policy
        .expect("policy")
        .is_administrator
}

/// Makes the Ferrofin user `name` an administrator, as an administrator would.
async fn make_administrator(users: &SignIn, name: &str) {
    let user = users.user(name).await.expect("user");
    let mut policy = users
        .users
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .policy
        .expect("policy");
    policy.is_administrator = true;
    users
        .users
        .update_policy(uuid::Uuid::parse_str(&user.id).expect("id"), &policy)
        .await
        .expect("policy");
}

/// Signs `name` in, which must succeed, and says whether they are then an
/// administrator.
async fn signs_in_as_administrator(users: &SignIn, name: &str, password: &str) -> bool {
    users
        .sign_in(name, password)
        .await
        .expect("sign in")
        .expect("signed in");
    is_administrator(users, name).await
}

/// The admin filter at sign-in (`Authenticate`'s admin block): Ada's entry
/// carries `employeeType: jellyfin-admin` and she is the one member of
/// `cn=jellyfin-admins`; the reader is neither.
async fn admin_filter(plain: &LdapConfig) {
    let by_uid = LdapConfig {
        ldap_search_filter: "(objectClass=inetOrgPerson)".to_owned(),
        ldap_search_attributes: "uid, mail".to_owned(),
        ldap_uid_attribute: "uid".to_owned(),
        ldap_username_attribute: "uid".to_owned(),
        enable_all_folders: true,
        ..plain.clone()
    };

    // DN-match mode: the filter must find the user's own entry.
    let dn_match = LdapConfig {
        ldap_admin_filter: "(employeeType=jellyfin-admin)".to_owned(),
        ..by_uid.clone()
    };
    let users = SignIn::new(&dn_match).await;
    assert!(signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);
    assert!(!signs_in_as_administrator(&users, "reader", READER_PASSWORD).await);

    // Removed from the directory's administrators while she is the only one:
    // she signs in and stays one (D5).
    let mut admin = directory_admin(plain).await;
    admin
        .modify_replace(ADA_DN, "employeeType", &[])
        .await
        .expect("remove employeeType");
    assert!(signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);
    // With another administrator she loses it, and gets it back once the
    // directory names her again.
    make_administrator(&users, "reader").await;
    assert!(!signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);
    admin
        .modify_replace(ADA_DN, "employeeType", &["jellyfin-admin"])
        .await
        .expect("restore employeeType");
    admin.unbind().await;
    assert!(signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);

    // Member-uid mode: any entry found counts, here the group, searched under
    // LdapAdminBaseDn with the user's name in the filter.
    let member_uid = LdapConfig {
        ldap_admin_filter: "(&(objectClass=groupOfNames)\
            (member=uid={username},ou=people,dc=example,dc=org))"
            .to_owned(),
        enable_ldap_admin_filter_member_uid: true,
        ldap_admin_base_dn: GROUPS_DN.to_owned(),
        ..by_uid.clone()
    };
    let users = SignIn::new(&member_uid).await;
    assert!(signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);
    assert!(!signs_in_as_administrator(&users, "reader", READER_PASSWORD).await);
    // The same filter in DN-match mode finds the group, not Ada's entry.
    let users = SignIn::new(&LdapConfig {
        enable_ldap_admin_filter_member_uid: false,
        ..member_uid
    })
    .await;
    assert!(!signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);

    // Turned off: the directory leaves the flag alone, so the reader, whom no
    // filter names, stays the administrator they were made by hand.
    let users = SignIn::new(&LdapConfig {
        ldap_admin_filter: "_disabled_".to_owned(),
        ..by_uid
    })
    .await;
    assert!(!signs_in_as_administrator(&users, "ada", ADA_PASSWORD).await);
    assert!(!signs_in_as_administrator(&users, "reader", READER_PASSWORD).await);
    make_administrator(&users, "reader").await;
    assert!(signs_in_as_administrator(&users, "reader", READER_PASSWORD).await);
    println!("admin filter: ok");
}

/// The settings page's filter test (`TestLdapFilters`) and user search
/// (`LdapUserSearch`) against the directory.
async fn filter_test_and_user_search(plain: &LdapConfig) {
    let directory = Ldap3Directory::default();
    let config = LdapConfig {
        ldap_search_filter: "(objectClass=inetOrgPerson)".to_owned(),
        ldap_admin_filter: "(employeeType=jellyfin-admin)".to_owned(),
        ldap_search_attributes: "uid, mail".to_owned(),
        ..plain.clone()
    };
    let failure = |message: &str| LdapTestErrorResponse::new(message);
    let filters = |search: &str, admin: &str| LdapConfig {
        ldap_search_filter: search.to_owned(),
        ldap_admin_filter: admin.to_owned(),
        ..config.clone()
    };

    let answer = test_ldap_filters(&directory, &config).await;
    println!("filter test: {answer:?}");
    assert_eq!(
        answer,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 2,
            admins: 1,
            is_subset: true,
        })
    );
    // The group is an admin the user filter does not find.
    assert_eq!(
        test_ldap_filters(
            &directory,
            &filters("(objectClass=inetOrgPerson)", "(cn=jellyfin-admins)")
        )
        .await,
        LdapTestAnswer::Ok(LdapFilterResponse {
            users: 2,
            admins: 1,
            is_subset: false,
        })
    );
    assert_eq!(
        test_ldap_filters(&directory, &filters("(uid=ada", "_disabled_")).await,
        LdapTestAnswer::BadRequest(failure("User Filter: Filter Error"))
    );
    assert_eq!(
        test_ldap_filters(&directory, &filters("(uid=*)", "(employeeType=")).await,
        LdapTestAnswer::BadRequest(failure("Admin Filter: Filter Error"))
    );
    let missing_base = test_ldap_filters(
        &directory,
        &LdapConfig {
            ldap_base_dn: "dc=missing,dc=org".to_owned(),
            ..config.clone()
        },
    )
    .await;
    println!("filter test under a missing base DN: {missing_base:?}");
    assert!(
        matches!(&missing_base, LdapTestAnswer::BadRequest(error)
            if error.message.starts_with("User Filter: No Such Object")),
        "{missing_base:?}"
    );
    let wrong_bind = LdapConfig {
        ldap_bind_password: Secret::new("wrong"),
        ..config.clone()
    };
    assert_eq!(
        test_ldap_filters(&directory, &wrong_bind).await,
        LdapTestAnswer::Unauthorized(failure("Failed to Connect or Bind to server."))
    );

    for name in ["ada", "ada@example.org"] {
        assert_eq!(
            ldap_user_search(&directory, &config, name).await,
            LdapTestAnswer::Ok(UserSearchResponse {
                located_dn: Some(ADA_DN.to_owned()),
            }),
            "{name}"
        );
    }
    assert_eq!(
        ldap_user_search(&directory, &config, "nobody").await,
        LdapTestAnswer::Unauthorized(failure("Found no LDAP users matching provided username."))
    );
    assert_eq!(
        ldap_user_search(&directory, &wrong_bind, "ada").await,
        LdapTestAnswer::Unauthorized(failure("Failed to Connect or Bind to server."))
    );
    assert_eq!(
        ldap_user_search(
            &directory,
            &LdapConfig {
                ldap_base_dn: "dc=missing,dc=org".to_owned(),
                ..config
            },
            "ada"
        )
        .await,
        LdapTestAnswer::Unauthorized(failure(
            "Error completing LDAP login while applying user filter."
        ))
    );
    println!("filter test and user search: ok");
}

/// Grace, whom only the referred server's directory lists.
const GRACE_DN: &str = "uid=grace,ou=partners,dc=example,dc=org";
const GRACE_PASSWORD: &str = "grace-secret";
/// The referring server's referral object, which names the referred server.
const PARTNERS_REFERRAL_DN: &str = "ou=partners-referral,dc=example,dc=org";

/// The referred server's directory: the same suffix, with the reader (the bind
/// DN binds there too) and the partners only it lists.
const REFERRED_SEED: &str = "dn: dc=example,dc=org
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

dn: ou=partners,dc=example,dc=org
objectClass: organizationalUnit
ou: partners

dn: uid=grace,ou=partners,dc=example,dc=org
objectClass: inetOrgPerson
uid: grace
cn: Grace Hopper
sn: Hopper
mail: grace@example.org
userPassword: grace-secret
";

/// What the referring server adds to [`SEED`]: a referral object naming the
/// referred server's partners, by plain LDAP and by LDAPS (alternatives, in
/// that order), and Grace's credentials. Sign-in checks the user's password at
/// the configured server, as upstream does, which in Active Directory can
/// authenticate a user of another domain in its forest; OpenLDAP cannot, so it
/// keeps her password in an `account` entry, which the user filter
/// (`inetOrgPerson`) does not find.
fn referring_seed(referred: &Directory) -> String {
    format!(
        "{SEED}
dn: ou=partners,dc=example,dc=org
objectClass: organizationalUnit
ou: partners

dn: {GRACE_DN}
objectClass: account
objectClass: simpleSecurityObject
uid: grace
userPassword: {GRACE_PASSWORD}

dn: {PARTNERS_REFERRAL_DN}
objectClass: referral
objectClass: extensibleObject
ou: partners-referral
ref: ldap://localhost:{}/ou=partners,dc=example,dc=org
ref: ldaps://localhost:{}/ou=partners,dc=example,dc=org
",
        referred.ldap_port, referred.ldaps_port
    )
}

/// Writes a server's configuration into `dir`, with the seed `seed` and the
/// certificates `conf` holds, adding `global` to slapd.conf's global section.
fn prepare_server(dir: &Path, conf: &Path, seed: &str, global: &str) {
    std::fs::create_dir_all(dir).expect("server dir");
    write(
        dir,
        "slapd.conf",
        &slapd_conf("never").replace("database mdb", &format!("{global}database mdb")),
    );
    write(dir, "seed.ldif", seed);
    write(dir, "start.sh", START);
    for file in ["server.pem", "server.key", "client-ca.pem"] {
        std::fs::copy(conf.join(file), dir.join(file)).expect("certificate");
    }
}

/// The real directory client, recording where it connects.
#[derive(Default)]
struct Recording {
    connects: Mutex<Vec<ConnectionOptions>>,
}

impl Recording {
    /// The connections asked for since the last call.
    fn take(&self) -> Vec<ConnectionOptions> {
        std::mem::take(&mut *self.connects.lock().expect("connects"))
    }
}

#[async_trait::async_trait]
impl LdapDirectory for Recording {
    async fn connect(
        &self,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn LdapConnection>, LdapError> {
        self.connects
            .lock()
            .expect("connects")
            .push(options.clone());
        Ldap3Directory::default().connect(options).await
    }
}

/// `(use_ssl, use_start_tls, port)` of each connection to the referred
/// server among `connects`.
fn referred_connections(
    connects: &[ConnectionOptions],
    referred: &Directory,
) -> Vec<(bool, bool, i32)> {
    let ports = [
        i32::from(referred.ldap_port),
        i32::from(referred.ldaps_port),
    ];
    connects
        .iter()
        .filter(|options| ports.contains(&options.port))
        .map(|options| (options.use_ssl, options.use_start_tls, options.port))
        .collect()
}

/// Referrals (`GetSearchConstraints` with `LdapAuthHandler`): the referring
/// server lists Grace's part of the tree as a referral to the referred one,
/// which refuses a simple bind without TLS. A user only the referred server
/// lists is found by the user search and the filter test and signs in, over
/// plain LDAP, LDAPS and StartTLS, the referred connection never in clear; a
/// search whose base is the referral object follows the `referral` result;
/// the connection test's base search does not follow, as upstream's.
async fn referrals(files: &Files, conf: &Path, scratch: &Path) {
    let referred_conf = scratch.join("referred");
    prepare_server(
        &referred_conf,
        conf,
        REFERRED_SEED,
        "security simple_bind=1\n",
    );
    let referred = Directory::start(&referred_conf);
    let referring_conf = scratch.join("referring");
    prepare_server(&referring_conf, conf, &referring_seed(&referred), "");
    let referring = Directory::start(&referring_conf);
    wait_until_up(referred.ldap_port).await;
    wait_until_up(referring.ldap_port).await;

    // The referred server refuses the bind password in clear.
    let mut clear = Ldap3Directory::default()
        .connect(&ConnectionOptions::from_config(&plain_config(
            referred.ldap_port,
        )))
        .await
        .expect("connect");
    let refused = clear.simple_bind(READER_DN, READER_PASSWORD).await;
    clear.unbind().await;
    println!("referred server, a bind in clear: {refused:?}");
    assert!(
        matches!(refused, Err(LdapError::Result { code: 13, .. })),
        "{refused:?}"
    );

    referral_configurations(files, &referring, &referred).await;
    println!("referrals: ok");
}

/// Over plain LDAP, LDAPS and StartTLS to the referring server: the user
/// search finds Grace and the filter test counts her, the referred connection
/// never in clear, and she signs in; then a search based at the referral
/// object, and the connection test.
async fn referral_configurations(files: &Files, referring: &Directory, referred: &Directory) {
    let plain = LdapConfig {
        ldap_search_filter: "(objectClass=inetOrgPerson)".to_owned(),
        ldap_search_attributes: "uid, cn, mail".to_owned(),
        ldap_uid_attribute: "uid".to_owned(),
        ldap_username_attribute: "cn".to_owned(),
        ldap_admin_filter: "_disabled_".to_owned(),
        ldap_root_ca_path: files.root_ca.clone(),
        enable_all_folders: true,
        ..plain_config(referring.ldap_port)
    };
    let ldaps = LdapConfig {
        ldap_port: i32::from(referring.ldaps_port),
        use_ssl: true,
        ..plain.clone()
    };
    let start_tls = LdapConfig {
        use_start_tls: true,
        ..plain.clone()
    };
    let ldap = i32::from(referred.ldap_port);
    let ldaps_port = i32::from(referred.ldaps_port);
    // Each configuration and how it reaches the referred server: the plain
    // URL first, over the configured TLS; plain LDAP is refused there, and an
    // LDAPS connection to the plain port fails, so both go on to the LDAPS URL.
    let cases = [
        (
            "plain",
            &plain,
            vec![(false, false, ldap), (true, false, ldaps_port)],
        ),
        (
            "ldaps",
            &ldaps,
            vec![(true, false, ldap), (true, false, ldaps_port)],
        ),
        ("starttls", &start_tls, vec![(false, true, ldap)]),
    ];
    let directory = Recording::default();
    for (case, config, path) in cases {
        let answer = ldap_user_search(&directory, config, "grace").await;
        println!("referral, {case}: user search {answer:?}");
        assert_eq!(
            answer,
            LdapTestAnswer::Ok(UserSearchResponse {
                located_dn: Some(GRACE_DN.to_owned()),
            }),
            "{case}"
        );
        assert_eq!(
            referred_connections(&directory.take(), referred),
            path,
            "{case}: the referred connections"
        );

        // The reader and Ada here, Grace there.
        assert_eq!(
            test_ldap_filters(&directory, config).await,
            LdapTestAnswer::Ok(LdapFilterResponse {
                users: 3,
                admins: 0,
                is_subset: true,
            }),
            "{case}"
        );
        directory.take();

        let users = SignIn::new(config).await;
        let grace = users
            .sign_in("grace", GRACE_PASSWORD)
            .await
            .expect("sign in")
            .expect("created");
        assert_eq!(grace.username, "Grace Hopper", "{case}");
        assert_eq!(
            users
                .sign_in("Grace Hopper", "wrong")
                .await
                .expect("sign in"),
            None,
            "{case}: a wrong password, checked at the configured server"
        );
    }

    // A search whose base is the referral object gets a `referral` result,
    // followed to the DN it names.
    let at_the_referral = LdapConfig {
        ldap_base_dn: PARTNERS_REFERRAL_DN.to_owned(),
        ..start_tls.clone()
    };
    assert_eq!(
        ldap_user_search(&directory, &at_the_referral, "grace").await,
        LdapTestAnswer::Ok(UserSearchResponse {
            located_dn: Some(GRACE_DN.to_owned()),
        }),
        "a referral result"
    );

    // The connection test's base search does not follow, as upstream's.
    let response = run("referral, connection test", &plain).await;
    assert_eq!(response.base_search, testing());
    assert_eq!(
        response.error.as_deref(),
        Some("Search result reference received, and referral following is off")
    );
}
