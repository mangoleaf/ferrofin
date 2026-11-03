//! [`Ldap3Directory`] against in-process servers: TLS listeners built from
//! generated certificates (the certificate checks, the client certificate, the
//! connect timeout) and a scripted LDAP responder (the StartTLS step and the
//! operations' results). The live directory test
//! (`tests/ldap_directory.rs`) covers a real server.

use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;

/// A certificate and its key, as PEM and as rustls takes them.
struct Issued {
    pem: String,
    key_pem: String,
    der: CertificateDer<'static>,
    key_der: PrivatePkcs8KeyDer<'static>,
}

/// A test CA that signs server and client certificates.
struct TestCa {
    issued: Issued,
    issuer: Issuer<'static, KeyPair>,
}

impl TestCa {
    fn new(name: &str) -> Self {
        Self::with_constraints(name, None)
    }

    /// A CA that may only issue for names under `permitted`.
    fn constrained(name: &str, permitted: &str) -> Self {
        Self::with_constraints(
            name,
            Some(rcgen::NameConstraints {
                permitted_subtrees: vec![rcgen::GeneralSubtree::DnsName(permitted.to_owned())],
                excluded_subtrees: Vec::new(),
            }),
        )
    }

    fn with_constraints(name: &str, constraints: Option<rcgen::NameConstraints>) -> Self {
        let key = KeyPair::generate().expect("ca key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
        params.name_constraints = constraints;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let certificate = params.self_signed(&key).expect("ca certificate");
        let issued = Issued {
            pem: certificate.pem(),
            key_pem: key.serialize_pem(),
            der: certificate.der().clone(),
            key_der: PrivatePkcs8KeyDer::from(key.serialize_der()),
        };
        Self {
            issued,
            issuer: Issuer::new(params, key),
        }
    }

    fn issue(&self, names: &[&str], purpose: ExtendedKeyUsagePurpose) -> Issued {
        let key = KeyPair::generate().expect("leaf key");
        let mut params =
            CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
                .expect("leaf params");
        params.distinguished_name.push(
            DnType::CommonName,
            names.first().copied().unwrap_or("client"),
        );
        params.extended_key_usages = vec![purpose];
        let certificate = params
            .signed_by(&key, &self.issuer)
            .expect("leaf certificate");
        Issued {
            pem: certificate.pem(),
            key_pem: key.serialize_pem(),
            der: certificate.der().clone(),
            key_der: PrivatePkcs8KeyDer::from(key.serialize_der()),
        }
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A TLS server config presenting `server`, requiring a client certificate
/// from `client_ca` when one is given.
fn server_config(server: &Issued, client_ca: Option<&TestCa>) -> Arc<rustls::ServerConfig> {
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions");
    let builder = match client_ca {
        None => builder.with_no_client_auth(),
        Some(ca) => {
            let mut roots = RootCertStore::empty();
            roots.add(ca.issued.der.clone()).expect("client root");
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider(),
            )
            .build()
            .expect("client verifier");
            builder.with_client_cert_verifier(verifier)
        }
    };
    Arc::new(
        builder
            .with_single_cert(
                vec![server.der.clone()],
                PrivateKeyDer::Pkcs8(server.key_der.clone_key()),
            )
            .expect("server certificate"),
    )
}

/// An LDAPS listener on 127.0.0.1 that completes one handshake and reports
/// whether the client presented a certificate (`Err` when the handshake
/// failed on the server's side).
async fn ldaps_server(
    config: Arc<rustls::ServerConfig>,
) -> (u16, oneshot::Receiver<Result<bool, String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    ldaps_server_on(listener, config)
}

/// [`ldaps_server`] on a listener already bound.
fn ldaps_server_on(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
) -> (u16, oneshot::Receiver<Result<bool, String>>) {
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let outcome = tokio_rustls::TlsAcceptor::from(config)
            .accept(tcp)
            .await
            .map(|tls| tls.get_ref().1.peer_certificates().is_some())
            .map_err(|e| e.to_string());
        let _ = tx.send(outcome);
    });
    (port, rx)
}

/// Writes `contents` to `name` under `dir`.
fn write(dir: &tempfile::TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).expect("write pem");
    path
}

fn ldaps(server: &str, port: u16, verification: CertificateVerification) -> ConnectionOptions {
    ConnectionOptions {
        server: server.to_owned(),
        port: i32::from(port),
        use_ssl: true,
        use_start_tls: false,
        verification,
        client_certificate: None,
    }
}

fn connect_error(result: Result<Box<dyn LdapConnection>, LdapError>) -> LdapError {
    match result {
        Ok(_) => panic!("connected"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn ldaps_trusts_a_server_certificate_from_the_custom_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let server = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let root = write(&dir, "ca.pem", &ca.issued.pem);
    let (port, handshake) = ldaps_server(server_config(&server, None)).await;

    let connection = Ldap3Directory::default()
        .connect(&ldaps(
            "localhost",
            port,
            CertificateVerification::CustomRoot(root),
        ))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));
}

#[tokio::test]
async fn ldaps_refuses_a_certificate_for_another_name_even_from_the_custom_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let server = ca.issue(&["ldap.example"], ExtendedKeyUsagePurpose::ServerAuth);
    let root = write(&dir, "ca.pem", &ca.issued.pem);
    let (port, _handshake) = ldaps_server(server_config(&server, None)).await;

    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "localhost",
                port,
                CertificateVerification::CustomRoot(root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(message) if message.contains("not valid for name")),
        "{error:?}"
    );
}

#[tokio::test]
async fn ldaps_refuses_an_unknown_issuer_under_the_system_roots() {
    let ca = TestCa::new("Private CA");
    let server = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let (port, _handshake) = ldaps_server(server_config(&server, None)).await;

    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps("localhost", port, CertificateVerification::System))
            .await,
    );
    // A host with no CA bundle stops before the handshake, with its own reason.
    assert!(
        matches!(&error, LdapError::Connect(message)
            if message.contains("UnknownIssuer")
                || message.contains("no trusted root certificates")),
        "{error:?}"
    );
}

#[tokio::test]
async fn skip_verify_accepts_any_certificate_for_any_name() {
    let ca = TestCa::new("Private CA");
    let server = ca.issue(&["ldap.example"], ExtendedKeyUsagePurpose::ServerAuth);
    let (port, handshake) = ldaps_server(server_config(&server, None)).await;

    let connection = Ldap3Directory::default()
        .connect(&ldaps("127.0.0.1", port, CertificateVerification::None))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));
}

#[tokio::test]
async fn the_client_certificate_is_presented_to_a_server_that_asks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let clients = TestCa::new("Client CA");
    let server = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let client = clients.issue(&["jellyfin"], ExtendedKeyUsagePurpose::ClientAuth);
    let root = write(&dir, "ca.pem", &ca.issued.pem);
    let (port, handshake) = ldaps_server(server_config(&server, Some(&clients))).await;

    let mut options = ldaps("localhost", port, CertificateVerification::CustomRoot(root));
    options.client_certificate = Some(ClientCertificate {
        certificate: write(&dir, "client.pem", &client.pem),
        key: write(&dir, "client.key", &client.key_pem),
    });
    let connection = Ldap3Directory::default().connect(&options).await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(
        handshake.await.expect("server"),
        Ok(true),
        "the server saw the client certificate"
    );
}

#[tokio::test]
async fn a_server_that_wanted_a_client_certificate_closes_and_the_operation_says_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let clients = TestCa::new("Client CA");
    let server = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let root = write(&dir, "ca.pem", &ca.issued.pem);
    let (port, handshake) = ldaps_server(server_config(&server, Some(&clients))).await;

    // TLS 1.3 finishes the client's side of the handshake before the server
    // checks for a certificate, so the refusal arrives with the first
    // operation.
    let mut connection = Ldap3Directory::default()
        .connect(&ldaps(
            "localhost",
            port,
            CertificateVerification::CustomRoot(root),
        ))
        .await
        .expect("the client's side of the handshake");
    let error = connection
        .simple_bind("cn=reader", "secret")
        .await
        .expect_err("refused");
    assert!(
        matches!(&error, LdapError::Connection(m) if m.starts_with("the directory closed the connection: ") && m.contains("CertificateRequired")),
        "{error:?}"
    );
    assert!(handshake.await.expect("server").is_err());
}

#[tokio::test]
async fn a_server_that_hangs_up_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        drop(tcp);
    });
    let mut options = ldaps("127.0.0.1", port, CertificateVerification::None);
    options.use_ssl = false;
    let mut connection = Ldap3Directory::default()
        .connect(&options)
        .await
        .expect("connect");
    let error = connection
        .search("dc=example", SearchScope::Base, "(objectClass=*)", &[])
        .await
        .expect_err("closed");
    assert!(
        matches!(&error, LdapError::Connection(m) if m.starts_with("the directory closed the connection")),
        "{error:?}"
    );
}

#[tokio::test]
async fn certificate_files_that_cannot_be_read_fail_the_connect_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.pem");
    let empty = write(&dir, "empty.pem", "");
    let ca = TestCa::new("Directory CA");
    let certificate = write(&dir, "client.pem", &ca.issued.pem);
    let not_a_key = write(&dir, "not-a-key.pem", &ca.issued.pem);
    let directory = Ldap3Directory::default();

    let error = connect_error(
        directory
            .connect(&ldaps(
                "localhost",
                1,
                CertificateVerification::CustomRoot(missing.clone()),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.starts_with("could not read the root CA") && m.contains("missing.pem")),
        "{error:?}"
    );

    let error = connect_error(
        directory
            .connect(&ldaps(
                "localhost",
                1,
                CertificateVerification::CustomRoot(empty),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("holds no PEM certificate")),
        "{error:?}"
    );

    let mut options = ldaps("localhost", 1, CertificateVerification::None);
    options.client_certificate = Some(ClientCertificate {
        certificate,
        key: not_a_key,
    });
    let error = connect_error(directory.connect(&options).await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.starts_with("could not read the client key")),
        "{error:?}"
    );
    // The CA's own key is a key, but not the certificate's.
    options.client_certificate = Some(ClientCertificate {
        certificate: write(&dir, "ca.pem", &ca.issued.pem),
        key: write(&dir, "wrong.key", &TestCa::new("Other").issued.key_pem),
    });
    let error = connect_error(directory.connect(&options).await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("cannot be used")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_closed_port_or_a_bad_port_fails_the_connect_step() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let mut options = ldaps("127.0.0.1", port, CertificateVerification::None);
    options.use_ssl = false;

    let error = connect_error(Ldap3Directory::default().connect(&options).await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.starts_with(&format!("could not connect to 127.0.0.1:{port}:"))),
        "{error:?}"
    );

    options.port = 70_000;
    let error = connect_error(Ldap3Directory::default().connect(&options).await);
    assert_eq!(
        error,
        LdapError::Connect("70000 is not a TCP port".to_owned())
    );
}

#[tokio::test]
async fn an_ldaps_handshake_that_never_finishes_runs_out_the_connect_time() {
    // Accepts the TCP connection and never answers the ClientHello.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let silent = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(tcp);
    });

    let started = std::time::Instant::now();
    let error = connect_error(
        Ldap3Directory::with_timeouts(Duration::from_millis(300), Duration::from_secs(10))
            .connect(&ldaps("127.0.0.1", port, CertificateVerification::None))
            .await,
    );
    assert_eq!(
        error,
        LdapError::Connect(format!(
            "could not connect to 127.0.0.1:{port} within 0.3 seconds"
        ))
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    silent.abort();
}

// ---- a scripted LDAP responder ---------------------------------------------

/// A BER element: tag, then the length (short or long form), then `content`.
fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(u8::try_from(len).expect("short length"));
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | u8::try_from(bytes.len()).expect("length of length"));
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

fn octets(value: &[u8]) -> Vec<u8> {
    tlv(0x04, value)
}

/// An `LDAPResult` body: result code, matched DN, diagnostic.
fn ldap_result(code: u8, diagnostic: &str) -> Vec<u8> {
    let mut body = tlv(0x0a, &[code]);
    body.extend(octets(b""));
    body.extend(octets(diagnostic.as_bytes()));
    body
}

/// An `LDAPMessage` with `id` around the protocol op `op`.
fn message(id: &[u8], op: Vec<u8>) -> Vec<u8> {
    let mut content = tlv(0x02, id);
    content.extend(op);
    tlv(0x30, &content)
}

/// A search result entry: `dn` with `attributes`.
fn entry(dn: &str, attributes: &[(&str, &[&[u8]])]) -> Vec<u8> {
    let mut content = octets(dn.as_bytes());
    let mut list = Vec::new();
    for (name, values) in attributes {
        let mut attribute = octets(name.as_bytes());
        let set: Vec<u8> = values.iter().flat_map(|v| octets(v)).collect();
        attribute.extend(tlv(0x31, &set));
        list.extend(tlv(0x30, &attribute));
    }
    content.extend(tlv(0x30, &list));
    tlv(0x64, &content)
}

/// Reads one BER element from `stream`, returning its tag and content.
async fn read_element(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let tag = stream.read_u8().await.ok()?;
    let first = stream.read_u8().await.ok()?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let mut len = 0usize;
        for _ in 0..(first & 0x7f) {
            len = (len << 8) | usize::from(stream.read_u8().await.ok()?);
        }
        len
    };
    let mut content = vec![0; len];
    stream.read_exact(&mut content).await.ok()?;
    Some((tag, content))
}

/// Splits an `LDAPMessage`'s content into its message id and the protocol
/// op's tag.
fn id_and_op(content: &[u8]) -> (Vec<u8>, u8) {
    assert_eq!(content[0], 0x02, "message id");
    let id_len = usize::from(content[1]);
    (content[2..2 + id_len].to_vec(), content[2 + id_len])
}

/// Answers each request on one connection with the replies `answer` gives
/// for its protocol op tag, until the client unbinds or hangs up.
async fn ldap_server(answer: impl Fn(u8) -> Vec<Vec<u8>> + Send + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.expect("accept");
        while let Some((_, content)) = read_element(&mut tcp).await {
            let (id, op) = id_and_op(&content);
            if op == 0x42 {
                break; // unbind
            }
            for reply in answer(op) {
                tcp.write_all(&message(&id, reply)).await.expect("reply");
            }
        }
    });
    port
}

fn plain(port: u16) -> ConnectionOptions {
    ConnectionOptions {
        server: "127.0.0.1".to_owned(),
        port: i32::from(port),
        use_ssl: false,
        use_start_tls: false,
        verification: CertificateVerification::System,
        client_certificate: None,
    }
}

#[tokio::test]
async fn a_refused_start_tls_is_the_start_tls_step() {
    let port = ldap_server(|op| match op {
        0x77 => vec![tlv(0x78, &ldap_result(2, "unsupported extended operation"))],
        _ => Vec::new(),
    })
    .await;
    let mut options = plain(port);
    options.use_start_tls = true;

    let error = connect_error(Ldap3Directory::default().connect(&options).await);
    assert_eq!(
        error,
        LdapError::StartTls("Protocol Error: unsupported extended operation".to_owned())
    );
}

#[tokio::test]
async fn start_tls_over_ldaps_is_still_sent_as_its_own_step() {
    // Upstream's client sends StartTLS over the LDAPS connection too. This
    // server hangs up after the handshake, so StartTLS fails; a real server
    // refuses it (the live test). What is checked is that the step is not
    // silently dropped.
    let ca = TestCa::new("Private CA");
    let server = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let (port, _handshake) = ldaps_server(server_config(&server, None)).await;
    let mut options = ldaps("localhost", port, CertificateVerification::None);
    options.use_start_tls = true;

    let error = connect_error(Ldap3Directory::default().connect(&options).await);
    assert!(matches!(error, LdapError::StartTls(_)), "{error:?}");
}

#[tokio::test]
async fn binds_report_the_servers_result() {
    let port = ldap_server(|op| match op {
        0x60 => vec![tlv(0x61, &ldap_result(49, ""))],
        _ => Vec::new(),
    })
    .await;
    let mut connection = Ldap3Directory::default()
        .connect(&plain(port))
        .await
        .expect("connect");
    assert_eq!(
        connection.simple_bind("cn=reader", "wrong").await,
        Err(LdapError::Result {
            code: 49,
            message: String::new()
        })
    );
    connection.unbind().await;
}

#[tokio::test]
async fn a_search_returns_entries_and_references_and_fails_on_a_bad_result() {
    let port = ldap_server(|op| match op {
        0x60 => vec![tlv(0x61, &ldap_result(0, ""))],
        0x63 => vec![
            entry(
                "uid=ada,dc=example",
                &[("cn", &[b"Ada"]), ("jpegPhoto", &[&[0xff, 0xd8, 0xff]])],
            ),
            entry("uid=bob,dc=example", &[]),
            tlv(0x73, &octets(b"ldap://other.example/dc=other")),
            tlv(0x65, &ldap_result(0, "")),
        ],
        _ => Vec::new(),
    })
    .await;
    let mut connection = Ldap3Directory::default()
        .connect(&plain(port))
        .await
        .expect("connect");
    connection
        .simple_bind("cn=reader", "secret")
        .await
        .expect("bind");
    let found = connection
        .search(
            "dc=example",
            SearchScope::Subtree,
            "(uid=*)",
            &["cn", "jpegPhoto"],
        )
        .await
        .expect("search");
    assert_eq!(found.entries.len(), 2);
    assert_eq!(found.entries[0].dn, "uid=ada,dc=example");
    assert_eq!(
        found.entries[0].attribute("CN").map(|a| a.values.clone()),
        Some(vec![b"Ada".to_vec()]),
        "attribute names match ignoring case"
    );
    assert_eq!(
        found.entries[0]
            .attribute("jpegphoto")
            .map(|a| a.values.clone()),
        Some(vec![vec![0xff, 0xd8, 0xff]]),
        "binary values arrive as they were sent"
    );
    assert_eq!(
        found.references,
        vec!["ldap://other.example/dc=other".to_owned()]
    );
    assert_eq!(
        connection
            .search("dc=example", SearchScope::Base, "(uid=", &[])
            .await,
        Err(LdapError::Result {
            code: FILTER_ERROR,
            message: String::new()
        }),
        "a filter that does not parse is Novell's Filter Error"
    );
}

#[tokio::test]
async fn a_search_ending_in_an_error_result_is_that_error() {
    let port = ldap_server(|op| match op {
        0x63 => vec![tlv(0x65, &ldap_result(32, "no such base"))],
        _ => Vec::new(),
    })
    .await;
    let mut connection = Ldap3Directory::default()
        .connect(&plain(port))
        .await
        .expect("connect");
    let error = connection
        .search("dc=missing", SearchScope::OneLevel, "(objectClass=*)", &[])
        .await
        .expect_err("no such object");
    assert_eq!(error.to_string(), "No Such Object: no such base");
}

#[tokio::test]
async fn modify_password_modify_and_the_root_dse_report_the_servers_results() {
    let port = ldap_server(|op| match op {
        0x66 => vec![tlv(0x67, &ldap_result(50, "no write access"))],
        0x77 => vec![tlv(0x78, &ldap_result(0, ""))],
        0x63 => vec![
            entry(
                "",
                &[(
                    "supportedExtension",
                    &[b"1.3.6.1.4.1.4203.1.11.1", b"1.3.6.1.4.1.1466.20037"],
                )],
            ),
            tlv(0x65, &ldap_result(0, "")),
        ],
        _ => Vec::new(),
    })
    .await;
    let mut connection = Ldap3Directory::default()
        .connect(&plain(port))
        .await
        .expect("connect");
    assert_eq!(
        connection
            .modify_replace("uid=ada,dc=example", "userPassword", &["new"])
            .await
            .map_err(|e| e.to_string()),
        Err("Insufficient Access Rights: no write access".to_owned())
    );
    assert_eq!(
        connection
            .password_modify("uid=ada,dc=example", "new")
            .await,
        Ok(())
    );
    assert_eq!(
        connection.supported_extensions().await.expect("root dse"),
        vec![
            "1.3.6.1.4.1.4203.1.11.1".to_owned(),
            "1.3.6.1.4.1.1466.20037".to_owned()
        ]
    );
}

#[tokio::test]
async fn an_operation_without_an_answer_times_out() {
    let port = ldap_server(|_| Vec::new()).await;
    let mut connection =
        Ldap3Directory::with_timeouts(Duration::from_secs(5), Duration::from_millis(200))
            .connect(&plain(port))
            .await
            .expect("connect");
    let started = std::time::Instant::now();
    assert_eq!(
        connection.simple_bind("cn=reader", "secret").await,
        Err(LdapError::Timeout)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn an_ipv6_address_is_read_with_or_without_brackets() {
    assert_eq!(host("[::1]"), "::1");
    assert_eq!(host("::1"), "::1");
    assert_eq!(host("ldap.example"), "ldap.example");
    assert_eq!(ipv6("[::1]"), Some(Ipv6Addr::LOCALHOST));
    assert_eq!(ipv6("::1"), Some(Ipv6Addr::LOCALHOST));
    assert_eq!(ipv6("127.0.0.1"), None);
    assert_eq!(ipv6("ldap.example"), None);
}

#[tokio::test]
async fn ldaps_to_an_ipv6_address_checks_the_certificate_against_the_address() {
    let Ok(listener) = TcpListener::bind("[::1]:0").await else {
        eprintln!("skipping: this host has no IPv6 loopback");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let root = write(&dir, "ca.pem", &ca.issued.pem);
    let for_address = ca.issue(&["::1"], ExtendedKeyUsagePurpose::ServerAuth);
    let (port, handshake) = ldaps_server_on(listener, server_config(&for_address, None));
    // Bracketed as in a URL here, bare below: both name the address.
    let connection = Ldap3Directory::default()
        .connect(&ldaps(
            "[::1]",
            port,
            CertificateVerification::CustomRoot(root.clone()),
        ))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));

    // A certificate for a name, not for the address, is refused.
    let listener = TcpListener::bind("[::1]:0").await.expect("bind");
    let for_name = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let (port, _handshake) = ldaps_server_on(listener, server_config(&for_name, None));
    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "::1",
                port,
                CertificateVerification::CustomRoot(root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("not valid for name")),
        "{error:?}"
    );

    // Skip-verify connects to the address whatever the certificate says.
    let listener = TcpListener::bind("[::1]:0").await.expect("bind");
    let (port, handshake) = ldaps_server_on(listener, server_config(&for_name, None));
    let connection = Ldap3Directory::default()
        .connect(&ldaps("::1", port, CertificateVerification::None))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));
}

/// A self-signed server certificate marked as a CA, as `openssl req -x509`
/// makes one, for `name`, valid from `not_before` to `not_after`.
fn self_signed_ca_server(name: &str, not_before: i32, not_after: i32) -> Issued {
    let key = KeyPair::generate().expect("key");
    let mut params = CertificateParams::new(vec![name.to_owned()]).expect("params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.not_before = date_time_ymd(not_before, 1, 1);
    params.not_after = date_time_ymd(not_after, 1, 1);
    let certificate = params.self_signed(&key).expect("certificate");
    Issued {
        pem: certificate.pem(),
        key_pem: key.serialize_pem(),
        der: certificate.der().clone(),
        key_der: PrivatePkcs8KeyDer::from(key.serialize_der()),
    }
}

#[tokio::test]
async fn a_self_signed_server_certificate_given_as_the_root_ca_is_accepted_for_its_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = self_signed_ca_server("localhost", 2000, 4000);
    let root = write(&dir, "server.pem", &server.pem);

    let (port, handshake) = ldaps_server(server_config(&server, None)).await;
    let connection = Ldap3Directory::default()
        .connect(&ldaps(
            "localhost",
            port,
            CertificateVerification::CustomRoot(root.clone()),
        ))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));

    // Still for its name only.
    let (port, _handshake) = ldaps_server(server_config(&server, None)).await;
    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "127.0.0.1",
                port,
                CertificateVerification::CustomRoot(root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("not valid for name")),
        "{error:?}"
    );

    // And only while it is valid.
    let expired = self_signed_ca_server("localhost", 2000, 2001);
    let expired_root = write(&dir, "expired.pem", &expired.pem);
    let (port, _handshake) = ldaps_server(server_config(&expired, None)).await;
    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "localhost",
                port,
                CertificateVerification::CustomRoot(expired_root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.to_lowercase().contains("expired")),
        "{error:?}"
    );
}

/// A certificate naming its host only in the subject common name, with no
/// subject alternative name, as `openssl req -x509 -subj /CN=host` makes one;
/// marked as a CA, signed by itself.
fn common_name_only(host: &str) -> Issued {
    let key = KeyPair::generate().expect("key");
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.distinguished_name.push(DnType::CommonName, host);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let certificate = params.self_signed(&key).expect("certificate");
    Issued {
        pem: certificate.pem(),
        key_pem: key.serialize_pem(),
        der: certificate.der().clone(),
        key_der: PrivatePkcs8KeyDer::from(key.serialize_der()),
    }
}

/// A certificate `ca` signs for `san` (none when empty) with `cn` as its
/// subject common name.
fn leaf_with_common_name(ca: &TestCa, san: &[&str], cn: &str) -> Issued {
    leaf_valid_until(ca, san, cn, 4000)
}

/// [`leaf_with_common_name`], valid from 2000 to the start of `until`.
fn leaf_valid_until(ca: &TestCa, san: &[&str], cn: &str, until: i32) -> Issued {
    let key = KeyPair::generate().expect("key");
    let mut params =
        CertificateParams::new(san.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
            .expect("params");
    params.distinguished_name.push(DnType::CommonName, cn);
    params.not_before = date_time_ymd(2000, 1, 1);
    params.not_after = date_time_ymd(until, 1, 1);
    let leaf = params.signed_by(&key, &ca.issuer).expect("leaf");
    Issued {
        pem: leaf.pem(),
        key_pem: key.serialize_pem(),
        der: leaf.der().clone(),
        key_der: PrivatePkcs8KeyDer::from(key.serialize_der()),
    }
}

#[tokio::test]
async fn a_certificate_without_alternative_names_is_matched_by_its_common_name() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Given as its own root CA (the listed path), the name ignoring case.
    let server = common_name_only("LocalHost");
    let root = write(&dir, "server.pem", &server.pem);
    let (port, handshake) = ldaps_server(server_config(&server, None)).await;
    let connection = Ldap3Directory::default()
        .connect(&ldaps(
            "localhost",
            port,
            CertificateVerification::CustomRoot(root.clone()),
        ))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));
    let (port, _handshake) = ldaps_server(server_config(&server, None)).await;
    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "127.0.0.1",
                port,
                CertificateVerification::CustomRoot(root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("not valid for name")),
        "{error:?}"
    );

    // Signed by a CA (the chain path).
    let ca = TestCa::new("Directory CA");
    let leaf = leaf_with_common_name(&ca, &[], "localhost");
    let ca_root = write(&dir, "ca.pem", &ca.issued.pem);
    let (port, handshake) = ldaps_server(server_config(&leaf, None)).await;
    let connection = Ldap3Directory::default()
        .connect(&ldaps(
            "localhost",
            port,
            CertificateVerification::CustomRoot(ca_root),
        ))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));
}

/// Connects to an LDAPS server presenting `leaf` with `root_pem` as the root
/// CA file, by the name `server`.
async fn connect_with_root(
    leaf: &Issued,
    root_pem: &str,
    server: &str,
) -> Result<Box<dyn LdapConnection>, LdapError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = write(&dir, "root.pem", root_pem);
    let (port, _handshake) = ldaps_server(server_config(leaf, None)).await;
    Ldap3Directory::default()
        .connect(&ldaps(
            server,
            port,
            CertificateVerification::CustomRoot(root),
        ))
        .await
}

#[tokio::test]
async fn the_common_name_fallback_never_skips_the_chain() {
    let trusted = TestCa::new("Trusted CA");
    let other = TestCa::new("Other CA");

    // A common-name-only certificate from a CA the file does not hold.
    let foreign = leaf_with_common_name(&other, &[], "localhost");
    let error = connect_error(connect_with_root(&foreign, &trusted.issued.pem, "localhost").await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("UnknownIssuer")),
        "{error:?}"
    );

    // One from the trusted CA, but expired.
    let expired = leaf_valid_until(&trusted, &[], "localhost", 2001);
    let error = connect_error(connect_with_root(&expired, &trusted.issued.pem, "localhost").await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.to_lowercase().contains("expired")),
        "{error:?}"
    );

    // One from a CA constrained to another domain: webpki checks the
    // constraints against alternative names only, so the fallback is off.
    let constrained = TestCa::constrained("Constrained CA", "example.org");
    let outside = leaf_with_common_name(&constrained, &[], "localhost");
    let error =
        connect_error(connect_with_root(&outside, &constrained.issued.pem, "localhost").await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("not valid for name")),
        "{error:?}"
    );
}

#[test]
fn an_intermediate_with_name_constraints_turns_the_fallback_off() {
    let constrained = TestCa::constrained("Constrained CA", "example.org");
    let plain = TestCa::new("Plain CA");
    let junk = CertificateDer::from(b"not a certificate".to_vec());
    assert!(any_name_constrained(std::slice::from_ref(
        &constrained.issued.der
    )));
    assert!(!any_name_constrained(std::slice::from_ref(
        &plain.issued.der
    )));
    assert!(!any_name_constrained(&[]));
    assert!(
        any_name_constrained(&[plain.issued.der.clone(), junk]),
        "one that cannot be read counts as constrained"
    );
}

#[tokio::test]
async fn only_a_dns_alternative_name_rules_out_the_common_name() {
    // An address in the alternative names names no host, as OpenSSL reads it.
    let ca = TestCa::new("Directory CA");
    let leaf = leaf_with_common_name(&ca, &["127.0.0.1"], "localhost");
    let connection = connect_with_root(&leaf, &ca.issued.pem, "localhost").await;
    assert!(connection.is_ok(), "{:?}", connection.err());

    // An address host has no fallback: its common name is not read.
    let by_address = leaf_with_common_name(&ca, &[], "127.0.0.1");
    let error = connect_error(connect_with_root(&by_address, &ca.issued.pem, "127.0.0.1").await);
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("not valid for name")),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_alternative_name_rules_out_the_common_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let leaf = leaf_with_common_name(&ca, &["ldap.example"], "localhost");
    let root = write(&dir, "ca.pem", &ca.issued.pem);
    let (port, _handshake) = ldaps_server(server_config(&leaf, None)).await;
    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "localhost",
                port,
                CertificateVerification::CustomRoot(root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("not valid for name")),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_root_ca_file_is_the_only_trust_anchor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let trusted = TestCa::new("Trusted CA");
    let other = TestCa::new("Other CA");
    let server = other.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    let root = write(&dir, "ca.pem", &trusted.issued.pem);
    let (port, _handshake) = ldaps_server(server_config(&server, None)).await;

    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "localhost",
                port,
                CertificateVerification::CustomRoot(root),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("UnknownIssuer")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_root_ca_bundle_skips_a_certificate_it_cannot_use() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca = TestCa::new("Directory CA");
    let server = ca.issue(&["localhost"], ExtendedKeyUsagePurpose::ServerAuth);
    // PEM framing around bytes that are not a certificate.
    let junk = "-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydGlmaWNhdGU=\n-----END CERTIFICATE-----\n";
    let bundle = write(&dir, "bundle.pem", &format!("{junk}{}", ca.issued.pem));
    let (port, handshake) = ldaps_server(server_config(&server, None)).await;

    let connection = Ldap3Directory::default()
        .connect(&ldaps(
            "localhost",
            port,
            CertificateVerification::CustomRoot(bundle),
        ))
        .await;
    assert!(connection.is_ok(), "{:?}", connection.err());
    assert_eq!(handshake.await.expect("server"), Ok(false));

    let only_junk = write(&dir, "junk.pem", junk);
    let error = connect_error(
        Ldap3Directory::default()
            .connect(&ldaps(
                "localhost",
                1,
                CertificateVerification::CustomRoot(only_junk),
            ))
            .await,
    );
    assert!(
        matches!(&error, LdapError::Connect(m) if m.contains("holds no certificate that can be used as a root")),
        "{error:?}"
    );
}
