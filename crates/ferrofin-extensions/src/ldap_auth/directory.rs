//! The production [`LdapDirectory`]: `ldap3` over tokio, with rustls (the
//! aws-lc-rs provider, the server's one TLS stack) for LDAPS and StartTLS.
//!
//! The TCP connection is opened here rather than by `ldap3`, so a failure
//! before the server answered (the Connect step) is told apart from a StartTLS
//! failure after it, as upstream's test reports them, and so the D6 connect
//! timeout covers name resolution too. The certificate options become a rustls
//! [`ClientConfig`] (`client_config`) whose verifier (`ServerCertificate`)
//! checks the server's name against the configured server itself, so an IPv6
//! address works over TLS although `ldap3` names the TLS peer after the URL's
//! bracketed host.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use ldap3::exop::{Exop, PasswordModify};
use ldap3::{LdapConnAsync, LdapConnSettings, Mod, Scope, SearchEntry, StdStream};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio::time::Instant;
use tracing::Instrument;

use super::{
    CONNECT_TIMEOUT, CertificateVerification, ClientCertificate, ConnectionOptions, FILTER_ERROR,
    LdapAttribute, LdapConnection, LdapDirectory, LdapEntry, LdapError, OPERATION_TIMEOUT,
    SearchOutcome, SearchScope,
};

/// The OID of the StartTLS extended operation (RFC 4511 section 4.14).
const START_TLS_OID: &str = "1.3.6.1.4.1.1466.20037";

/// The host the `ldap3` URL names when the server is an IPv6 address. The TCP
/// connection is already open, so the URL only names the TLS peer, and
/// [`ServerCertificate`] checks the certificate against the address instead.
const IPV6_URL_HOST: &str = "ipv6-address.invalid";

/// The root DSE attribute listing the extended operations a server supports.
const SUPPORTED_EXTENSION: &str = "supportedExtension";

/// The [`LdapDirectory`] that connects to real servers.
#[derive(Debug, Clone, Copy)]
pub struct Ldap3Directory {
    connect_timeout: Duration,
    operation_timeout: Duration,
}

impl Default for Ldap3Directory {
    /// The D6 timeouts: [`CONNECT_TIMEOUT`] and [`OPERATION_TIMEOUT`].
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            operation_timeout: OPERATION_TIMEOUT,
        }
    }
}

impl Ldap3Directory {
    /// A directory with other timeouts than D6's: tests that watch a timeout
    /// fire use short ones.
    #[cfg(test)]
    fn with_timeouts(connect_timeout: Duration, operation_timeout: Duration) -> Self {
        Self {
            connect_timeout,
            operation_timeout,
        }
    }
}

#[async_trait]
impl LdapDirectory for Ldap3Directory {
    async fn connect(
        &self,
        options: &ConnectionOptions,
    ) -> Result<Box<dyn LdapConnection>, LdapError> {
        let deadline = Instant::now() + self.connect_timeout;
        let port = u16::try_from(options.port)
            .map_err(|_| LdapError::Connect(format!("{} is not a TCP port", options.port)))?;
        let tls = if options.use_ssl || options.use_start_tls {
            let options = options.clone();
            Some(
                tokio::task::spawn_blocking(move || client_config(&options))
                    .await
                    .map_err(|e| LdapError::Connect(format!("TLS setup failed: {e}")))??,
            )
        } else {
            None
        };

        let server = host(&options.server);
        let stream = self.open_tcp(server, port, deadline).await?;

        // StartTLS is an operation on the open connection; LDAPS's handshake is
        // part of connecting, inside what is left of the connect budget.
        let start_tls = options.use_start_tls && !options.use_ssl;
        let mut settings = LdapConnSettings::new()
            .set_std_stream(StdStream::Tcp(stream))
            .set_starttls(start_tls);
        if let Some(config) = tls {
            settings = settings
                .set_config(config)
                .set_no_tls_verify(options.verification == CertificateVerification::None)
                .set_conn_timeout(if start_tls {
                    self.operation_timeout
                } else {
                    deadline.saturating_duration_since(Instant::now())
                });
        }
        let scheme = if options.use_ssl { "ldaps" } else { "ldap" };
        let url_host = if ipv6(&options.server).is_some() {
            IPV6_URL_HOST
        } else {
            server
        };
        let url = format!("{scheme}://{url_host}:{port}");
        let (driver, ldap) =
            LdapConnAsync::with_settings(settings, &url)
                .await
                .map_err(|e| match (start_tls, e) {
                    (false, ldap3::LdapError::Timeout { .. }) => LdapError::Connect(format!(
                        "could not connect to {server}:{port} within {} seconds",
                        self.connect_timeout.as_secs_f64()
                    )),
                    (true, ldap3::LdapError::Timeout { .. }) => LdapError::StartTls(format!(
                        "StartTLS did not finish within {} seconds",
                        self.operation_timeout.as_secs_f64()
                    )),
                    (false, e) => LdapError::Connect(describe(&e)),
                    (true, e) => LdapError::StartTls(describe(&e)),
                })?;
        // The driver's own error is why a later operation finds the connection
        // closed (a TLS alert, a reset), so it is kept for that report.
        let driver = tokio::spawn(
            async move {
                let error = driver.drive().await.err()?;
                tracing::debug!(error = ?error.to_string(), "LDAP connection ended with an error");
                Some(describe(&error))
            }
            .in_current_span(),
        );
        let mut connection = Ldap3Connection {
            ldap,
            timeout: self.operation_timeout,
            driver: Some(driver),
        };

        if options.use_ssl && options.use_start_tls {
            return Err(connection.start_tls_over_tls().await);
        }
        Ok(Box::new(connection))
    }
}

impl Ldap3Directory {
    /// Opens the TCP connection to `server` by `deadline`: the part of the
    /// Connect step `ldap3` would otherwise hide inside StartTLS.
    async fn open_tcp(
        &self,
        server: &str,
        port: u16,
        deadline: Instant,
    ) -> Result<std::net::TcpStream, LdapError> {
        let tcp =
            match tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect((server, port)))
                .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    return Err(LdapError::Connect(format!(
                        "could not connect to {server}:{port}: {e}"
                    )));
                }
                Err(_) => {
                    return Err(LdapError::Connect(format!(
                        "could not connect to {server}:{port} within {} seconds",
                        self.connect_timeout.as_secs_f64()
                    )));
                }
            };
        tcp.into_std()
            .map_err(|e| LdapError::Connect(format!("could not use the connection: {e}")))
    }
}

/// The host to connect to: `server` without the brackets an IPv6 address may
/// be written in.
fn host(server: &str) -> &str {
    server
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(server)
}

/// The IPv6 address `server` names, bracketed or not.
fn ipv6(server: &str) -> Option<Ipv6Addr> {
    host(server).parse().ok()
}

/// The message for an `ldap3` error that is not a result or a timeout of an
/// operation.
fn describe(error: &ldap3::LdapError) -> String {
    match error {
        ldap3::LdapError::Io { source } => source.to_string(),
        ldap3::LdapError::LdapResult { result } => LdapError::Result {
            code: result.rc,
            message: result.text.clone(),
        }
        .to_string(),
        other => other.to_string(),
    }
}

/// The [`LdapError`] for a failed operation, the connection still open.
fn operation_error(error: ldap3::LdapError) -> LdapError {
    match error {
        ldap3::LdapError::Timeout { .. } => LdapError::Timeout,
        ldap3::LdapError::LdapResult { result } => LdapError::Result {
            code: result.rc,
            message: result.text,
        },
        ldap3::LdapError::FilterParsing => LdapError::Result {
            code: FILTER_ERROR,
            message: String::new(),
        },
        other => LdapError::Connection(describe(&other)),
    }
}

/// The system's trusted roots, read once (as `ldap3` does for its own default
/// configuration). A store that fails to load is empty, so verification
/// against it fails rather than trusting anything.
static SYSTEM_ROOTS: LazyLock<Arc<RootCertStore>> = LazyLock::new(|| {
    let mut store = RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for error in &loaded.errors {
        tracing::warn!(error = %error, "could not load a system root certificate");
    }
    let (_added, _ignored) = store.add_parsable_certificates(loaded.certs);
    Arc::new(store)
});

/// The rustls configuration for `options`' certificate checks and client
/// certificate. Reads the PEM files it names, so it runs off the async
/// threads.
fn client_config(options: &ConnectionOptions) -> Result<Arc<ClientConfig>, LdapError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| LdapError::Connect(format!("TLS setup failed: {e}")))?;
    let verifier: Arc<dyn ServerCertVerifier> = match &options.verification {
        CertificateVerification::None => Arc::new(AnyServerCertificate(provider)),
        CertificateVerification::System => Arc::new(ServerCertificate::new(
            Arc::clone(&SYSTEM_ROOTS),
            Vec::new(),
            &options.server,
            provider,
        )?),
        CertificateVerification::CustomRoot(path) => {
            let (roots, listed) = read_roots(path)?;
            Arc::new(ServerCertificate::new(
                Arc::new(roots),
                listed,
                &options.server,
                provider,
            )?)
        }
    };
    let builder = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let mut config = match &options.client_certificate {
        None => builder.with_no_client_auth(),
        Some(ClientCertificate { certificate, key }) => builder
            .with_client_auth_cert(read_chain(certificate)?, read_key(key)?)
            .map_err(|e| {
                LdapError::Connect(format!(
                    "the client certificate {} cannot be used: {e}",
                    certificate.display()
                ))
            })?,
    };
    // An address is not a name: TLS sends none for it (RFC 6066 section 3),
    // and the placeholder host the URL carries must not go out either.
    if ipv6(&options.server).is_some() {
        config.enable_sni = false;
    }
    Ok(Arc::new(config))
}

/// Upstream's checks when the certificate is verified (.NET's default against
/// the system's roots, or `LdapClient_UserDefinedServerCertValidationDelegate`
/// with a root CA file): the certificate must be valid for the configured
/// server (by its subject alternative names, or by its common name when none
/// of them is a DNS name, as OpenSSL falls back for .NET on Linux) and chain
/// to one of the roots, or be one of the root CA file's certificates itself,
/// still valid for the server and within its validity period. That second
/// case is a self-signed server certificate given as the root CA, which .NET
/// accepts and webpki alone refuses when it is marked as a CA
/// (`openssl req -x509` marks it so).
#[derive(Debug)]
struct ServerCertificate {
    chain: Arc<WebPkiServerVerifier>,
    /// The root CA file's certificates, accepted as the server's own.
    listed: Vec<CertificateDer<'static>>,
    /// The name to check: the configured server, not the name `ldap3` opened
    /// the TLS session with (see [`IPV6_URL_HOST`]).
    name: ServerName<'static>,
    /// The configured server as text, for the common-name fallback.
    host: String,
    /// Whether a trusted root carries name constraints, which turns the
    /// common-name fallback off on the chain path.
    roots_constrained: bool,
}

impl ServerCertificate {
    fn new(
        roots: Arc<RootCertStore>,
        listed: Vec<CertificateDer<'static>>,
        server: &str,
        provider: Arc<CryptoProvider>,
    ) -> Result<Self, LdapError> {
        // Only the system's store can be empty: a root CA file with nothing
        // usable is refused when it is read.
        if roots.is_empty() {
            return Err(LdapError::Connect(
                "this system has no trusted root certificates: set a root CA file".to_owned(),
            ));
        }
        let roots_constrained = roots
            .roots
            .iter()
            .any(|anchor| anchor.name_constraints.is_some());
        let chain = WebPkiServerVerifier::builder_with_provider(roots, provider)
            .build()
            .map_err(|e| LdapError::Connect(format!("TLS setup failed: {e}")))?;
        let host = host(server).to_owned();
        let name = ServerName::try_from(host.clone()).map_err(|_| {
            LdapError::Connect(format!("{server} is not a host name or an IP address"))
        })?;
        Ok(Self {
            chain,
            listed,
            name,
            host,
            roots_constrained,
        })
    }
}

/// Whether `error` says the certificate is not valid for the server's name.
fn is_name_mismatch(error: &rustls::Error) -> bool {
    matches!(
        error,
        rustls::Error::InvalidCertificate(
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }
        )
    )
}

/// The common-name fallback for a host name (OpenSSL's `X509_check_host`,
/// which .NET uses on Linux): a certificate with no DNS name among its
/// subject alternative names matches when a subject common name equals the
/// host, ignoring case and a trailing dot. Wildcards in the common name are
/// not matched, and an IP address host has no fallback.
fn common_name_names(certificate: &CertificateDer<'_>, host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return false;
    }
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(certificate.as_ref()) else {
        return false;
    };
    let names_a_host = match parsed.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .any(|name| matches!(name, x509_parser::extensions::GeneralName::DNSName(_))),
        Ok(None) => false,
        // An alternative name that cannot be read names something: no fallback.
        Err(_) => true,
    };
    !names_a_host
        && parsed
            .subject()
            .iter_common_name()
            .filter_map(|cn| cn.as_str().ok())
            .any(|cn| {
                cn.trim_end_matches('.')
                    .eq_ignore_ascii_case(host.trim_end_matches('.'))
            })
}

/// Whether any of `certificates` carries name constraints, or cannot be read
/// to tell. webpki applies a CA's name constraints to the subject alternative
/// names only, so the common-name fallback must not run under such a CA.
fn any_name_constrained(certificates: &[CertificateDer<'_>]) -> bool {
    certificates.iter().any(|certificate| {
        x509_parser::parse_x509_certificate(certificate.as_ref()).map_or(true, |(_, parsed)| {
            !matches!(parsed.name_constraints(), Ok(None))
        })
    })
}

impl ServerCertVerifier for ServerCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let listed = self.listed.iter().any(|listed| listed == end_entity);
        let named = if listed {
            rustls::client::verify_server_name(
                &ParsedCertificate::try_from(end_entity)?,
                &self.name,
            )
            .map(|()| ServerCertVerified::assertion())
        } else {
            // webpki checks the name last, so a name mismatch means the chain
            // and the validity period already passed.
            self.chain
                .verify_server_cert(end_entity, intermediates, &self.name, ocsp_response, now)
        };
        // webpki enforces a CA's name constraints on the alternative names
        // only, so the fallback stays off under a constrained CA.
        let constrained =
            !listed && (self.roots_constrained || any_name_constrained(intermediates));
        let verified = match named {
            Err(error)
                if is_name_mismatch(&error)
                    && !constrained
                    && common_name_names(end_entity, &self.host) =>
            {
                ServerCertVerified::assertion()
            }
            other => other?,
        };
        if listed {
            in_validity_period(end_entity, now)?;
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.chain.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.chain.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.chain.supported_verify_schemes()
    }
}

/// Whether `certificate` is within its validity period at `now`.
fn in_validity_period(
    certificate: &CertificateDer<'_>,
    now: UnixTime,
) -> Result<(), rustls::Error> {
    let unreadable = rustls::Error::InvalidCertificate(CertificateError::BadEncoding);
    let (_, parsed) = x509_parser::parse_x509_certificate(certificate.as_ref())
        .map_err(|_| unreadable.clone())?;
    let now = i64::try_from(now.as_secs())
        .ok()
        .and_then(|secs| x509_parser::time::ASN1Time::from_timestamp(secs).ok())
        .ok_or(unreadable)?;
    let validity = parsed.validity();
    if now < validity.not_before {
        Err(rustls::Error::InvalidCertificate(
            CertificateError::NotValidYet,
        ))
    } else if now > validity.not_after {
        Err(rustls::Error::InvalidCertificate(CertificateError::Expired))
    } else {
        Ok(())
    }
}

/// The certificates in the PEM file at `path` as trusted roots, and as they
/// are. A certificate rustls cannot use as a root is skipped, as the system
/// roots' are; a file with none it can use is an error.
fn read_roots(path: &Path) -> Result<(RootCertStore, Vec<CertificateDer<'static>>), LdapError> {
    let listed = read_certificates(path, "root CA")?;
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(listed.iter().cloned());
    if added == 0 {
        return Err(LdapError::Connect(format!(
            "the root CA {} holds no certificate that can be used as a root",
            path.display()
        )));
    }
    Ok((roots, listed))
}

/// The client certificate chain in the PEM file at `path`, leaf first.
fn read_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, LdapError> {
    read_certificates(path, "client certificate")
}

/// The certificates in the PEM file at `path`; `what` names it in errors.
fn read_certificates(path: &Path, what: &str) -> Result<Vec<CertificateDer<'static>>, LdapError> {
    let unreadable = |e: &dyn std::fmt::Display| {
        LdapError::Connect(format!("could not read the {what} {}: {e}", path.display()))
    };
    let certificates = CertificateDer::pem_file_iter(path)
        .map_err(|e| unreadable(&e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| unreadable(&e))?;
    if certificates.is_empty() {
        return Err(unreadable(&"it holds no PEM certificate"));
    }
    Ok(certificates)
}

/// The private key in the PEM file at `path` (PKCS#8, PKCS#1 or SEC1).
fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, LdapError> {
    PrivateKeyDer::from_pem_file(path).map_err(|e| {
        LdapError::Connect(format!(
            "could not read the client key {}: {e}",
            path.display()
        ))
    })
}

/// Accepts any server certificate (`SkipSslVerify`), still checking that the
/// handshake was signed by the key the certificate holds.
#[derive(Debug)]
struct AnyServerCertificate(Arc<CryptoProvider>);

impl AnyServerCertificate {
    fn algorithms(&self) -> &WebPkiSupportedAlgorithms {
        &self.0.signature_verification_algorithms
    }
}

impl ServerCertVerifier for AnyServerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, self.algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, self.algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms().supported_schemes()
    }
}

/// How long an operation that found the connection closed waits for the
/// driver task to say why. It has already stopped by then; this only bounds
/// the join.
const CLOSE_REASON_WAIT: Duration = Duration::from_secs(1);

/// An open `ldap3` connection; its driver task ends when this is dropped.
struct Ldap3Connection {
    ldap: ldap3::Ldap,
    timeout: Duration,
    /// The task driving the connection, answering why it stopped when it
    /// stopped on an error; taken by the first operation that needs it.
    driver: Option<tokio::task::JoinHandle<Option<String>>>,
}

impl Ldap3Connection {
    /// StartTLS on a connection that is already LDAPS: Novell's client sends it
    /// anyway, the server refuses it, and that answer is the StartTLS step's.
    async fn start_tls_over_tls(&mut self) -> LdapError {
        let answer = self
            .ldap
            .with_timeout(self.timeout)
            .extended(Exop {
                name: Some(START_TLS_OID.to_owned()),
                val: None,
            })
            .await
            .and_then(ldap3::result::ExopResult::success);
        LdapError::StartTls(match answer {
            Err(e) => self.failure(e).await.to_string(),
            Ok(_) => "the server accepted StartTLS on a connection already using TLS".to_owned(),
        })
    }

    /// The [`LdapError`] for a failed operation. One that found the connection
    /// gone reports why the driver stopped, rather than `ldap3`'s
    /// "result recv error: channel closed".
    async fn failure(&mut self, error: ldap3::LdapError) -> LdapError {
        match error {
            ldap3::LdapError::OpSend { .. }
            | ldap3::LdapError::ResultRecv { .. }
            | ldap3::LdapError::IdScrubSend { .. }
            | ldap3::LdapError::EndOfStream => {
                let reason = match self.driver.take() {
                    Some(driver) => tokio::time::timeout(CLOSE_REASON_WAIT, driver)
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten(),
                    None => None,
                };
                LdapError::Connection(reason.map_or_else(
                    || "the directory closed the connection".to_owned(),
                    |reason| format!("the directory closed the connection: {reason}"),
                ))
            }
            other => operation_error(other),
        }
    }
}

#[async_trait]
impl LdapConnection for Ldap3Connection {
    async fn simple_bind(&mut self, dn: &str, password: &str) -> Result<(), LdapError> {
        match self
            .ldap
            .with_timeout(self.timeout)
            .simple_bind(dn, password)
            .await
            .and_then(ldap3::LdapResult::success)
        {
            Ok(_) => Ok(()),
            Err(e) => Err(self.failure(e).await),
        }
    }

    async fn search(
        &mut self,
        base: &str,
        scope: SearchScope,
        filter: &str,
        attributes: &[&str],
    ) -> Result<SearchOutcome, LdapError> {
        let scope = match scope {
            SearchScope::Base => Scope::Base,
            SearchScope::OneLevel => Scope::OneLevel,
            SearchScope::Subtree => Scope::Subtree,
        };
        let ldap3::SearchResult(entries, result) = match self
            .ldap
            .with_timeout(self.timeout)
            .search(base, scope, filter, attributes.to_vec())
            .await
        {
            Ok(found) => found,
            Err(e) => return Err(self.failure(e).await),
        };
        if result.rc != 0 {
            return Err(LdapError::Result {
                code: result.rc,
                message: result.text,
            });
        }
        Ok(SearchOutcome {
            entries: entries
                .into_iter()
                .map(ldap_entry)
                .collect::<Result<_, _>>()?,
            references: result.refs,
        })
    }

    async fn modify_replace(
        &mut self,
        dn: &str,
        attribute: &str,
        values: &[&str],
    ) -> Result<(), LdapError> {
        let values: HashSet<&str> = values.iter().copied().collect();
        match self
            .ldap
            .with_timeout(self.timeout)
            .modify(dn, vec![Mod::Replace(attribute, values)])
            .await
            .and_then(ldap3::LdapResult::success)
        {
            Ok(_) => Ok(()),
            Err(e) => Err(self.failure(e).await),
        }
    }

    async fn password_modify(
        &mut self,
        user_dn: &str,
        new_password: &str,
    ) -> Result<(), LdapError> {
        match self
            .ldap
            .with_timeout(self.timeout)
            .extended(PasswordModify {
                user_id: Some(user_dn),
                old_pass: None,
                new_pass: Some(new_password),
            })
            .await
            .and_then(ldap3::result::ExopResult::success)
        {
            Ok(_) => Ok(()),
            Err(e) => Err(self.failure(e).await),
        }
    }

    async fn supported_extensions(&mut self) -> Result<Vec<String>, LdapError> {
        let root_dse = self
            .search(
                "",
                SearchScope::Base,
                "(objectClass=*)",
                &[SUPPORTED_EXTENSION],
            )
            .await?;
        Ok(root_dse
            .entries
            .iter()
            .filter_map(|entry| entry.attribute(SUPPORTED_EXTENSION))
            .flat_map(|attribute| &attribute.values)
            .filter_map(|value| String::from_utf8(value.clone()).ok())
            .collect())
    }

    async fn unbind(&mut self) {
        if let Err(e) = self.ldap.with_timeout(self.timeout).unbind().await {
            tracing::debug!(error = ?e.to_string(), "LDAP unbind failed");
        }
    }
}

/// An `ldap3` entry as a [`LdapEntry`], its text and binary attributes alike.
/// `SearchEntry::construct` panics on an entry it cannot read (a DN that is
/// not UTF-8, say), which here fails the search instead.
fn ldap_entry(entry: ldap3::ResultEntry) -> Result<LdapEntry, LdapError> {
    let entry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        SearchEntry::construct(entry)
    }))
    .map_err(|_| {
        LdapError::Connection("the directory sent an entry that cannot be read".to_owned())
    })?;
    let text = entry.attrs.into_iter().map(|(name, values)| LdapAttribute {
        name,
        values: values.into_iter().map(String::into_bytes).collect(),
    });
    let binary = entry
        .bin_attrs
        .into_iter()
        .map(|(name, values)| LdapAttribute { name, values });
    Ok(LdapEntry {
        dn: entry.dn,
        attributes: text.chain(binary).collect(),
    })
}

#[cfg(test)]
mod connect_tests;
