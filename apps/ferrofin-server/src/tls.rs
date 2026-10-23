//! HTTPS: the certificate, the listener that serves it, and the redirect to it.
//!
//! Port of Jellyfin's HTTPS hosting:
//! - `ApplicationHost.GetCertificate` — the PKCS#12 certificate at the network
//!   configuration's `CertificatePath` ([`load_certificate`]);
//! - `ApplicationHost.ListenWithHttps` — a loaded certificate and `EnableHttps`
//!   ([`HttpsServing::from_network_configuration`]);
//! - Kestrel's `ListenOptions.UseHttps(certificate)` on the HTTPS port
//!   ([`TlsListener`]);
//! - `HttpsRedirectionMiddleware`, added for `RequireHttps`
//!   ([`redirect_to_https`]);
//! - `ApplicationHost.ValidateSslCertificate` ([`CertificatePathCheck`]).
//!
//! rustls runs on the aws-lc-rs provider reqwest already builds, chosen
//! explicitly: the workspace also compiles ring (sqlx), so there is no single
//! process default.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ferrofin_api::forwarded_headers::{
    ForwardedRequest, effective_scheme, forwarded_through, is_valid_host,
};
use ferrofin_common::configuration::{CommittedConfigurationCheck, ConfigurationValidationError};
use ferrofin_networking::NetworkConfiguration;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

use crate::listen::Transport;

/// How long a client has to finish the TLS handshake: Kestrel's
/// `HttpsConnectionAdapterOptions.HandshakeTimeout` default.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest certificate file read. A PKCS#12 file holds a key and a few
/// certificates, kilobytes; a path pointed at something else by mistake (a
/// media file) is refused rather than read into memory at every boot.
const MAX_CERTIFICATE_BYTES: u64 = 1024 * 1024;

/// Handshaken connections waiting for the serve loop to take them.
const ACCEPTED_QUEUE: usize = 64;

/// What the server serves HTTPS with this lifetime.
#[derive(Clone)]
pub struct HttpsServing {
    /// The rustls configuration holding the certificate.
    pub config: Arc<rustls::ServerConfig>,
    /// `RequireHttps`: plain HTTP requests are redirected to HTTPS.
    pub require: bool,
}

impl std::fmt::Debug for HttpsServing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsServing")
            .field("require", &self.require)
            .finish_non_exhaustive()
    }
}

impl HttpsServing {
    /// `ListenWithHttps`: `Some` when `EnableHttps` is on and the certificate
    /// loads ([`load_certificate`]). With HTTPS enabled but no certificate the
    /// server serves HTTP only, as Jellyfin does, and logs why once: an error
    /// for a certificate that cannot be used, as Jellyfin's `GetCertificate`
    /// does, and a warning for a blank path or a missing file, where Jellyfin
    /// says nothing.
    ///
    /// HTTPS on the HTTP port is not served either: the two listeners cannot
    /// share it. Jellyfin resets both ports to their defaults in that case
    /// (`ApplicationHost.Init`), because its ports come from the dashboard.
    /// Until Ferrofin reads the dashboard's ports (gl-554), they come from the
    /// operator (`FERROFIN_PORT`, `FERROFIN_HTTPS_PORT`), so the HTTP port
    /// stays where it was put and the clash is logged as an error. Port 0 (an
    /// ephemeral port each) never clashes.
    #[must_use]
    pub fn from_network_configuration(
        network: &NetworkConfiguration,
        plain_port: u16,
        tls_port: u16,
    ) -> Option<Self> {
        if !network.enable_https {
            return None;
        }
        if tls_port != 0 && tls_port == plain_port {
            // TODO(gl-554): when the dashboard's InternalHttpPort and
            // InternalHttpsPort are read, port `ApplicationHost.Init`'s reset of
            // equal ports to 8096/8920 into the port resolution, where it also
            // applies with HTTPS off; this check then never sees equal ports.
            tracing::error!(
                port = tls_port,
                "HTTPS is enabled on the HTTP port; set FERROFIN_HTTPS_PORT to another port. \
                 Serving HTTP only"
            );
            return None;
        }
        let path = network.certificate_path.as_str();
        match load_certificate(path, &network.certificate_password) {
            Ok(config) => Some(Self {
                config,
                require: network.require_https,
            }),
            Err(error @ (CertificateError::NoPath | CertificateError::NotFound)) => {
                tracing::warn!(path, %error, "HTTPS is enabled without a certificate; serving HTTP only");
                None
            }
            Err(error) => {
                tracing::error!(path, %error, "error loading the HTTPS certificate; serving HTTP only");
                None
            }
        }
    }
}

/// Why [`load_certificate`] loaded no certificate.
#[derive(Debug)]
pub enum CertificateError {
    /// The certificate path is blank.
    NoPath,
    /// No file is at the certificate path.
    NotFound,
    /// The file cannot be read or decrypted as PKCS#12.
    Unreadable(String),
    /// The file holds no private key with its certificate.
    NoPrivateKey,
    /// TLS refuses the certificate or its key.
    Rejected(String),
}

impl std::fmt::Display for CertificateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPath => f.write_str("no certificate path is set"),
            Self::NotFound => f.write_str("the certificate file does not exist"),
            Self::Unreadable(error) => write!(f, "the certificate cannot be read: {error}"),
            Self::NoPrivateKey => f.write_str("no private key is included in the certificate"),
            Self::Rejected(error) => write!(f, "the certificate cannot be used: {error}"),
        }
    }
}

impl std::error::Error for CertificateError {}

/// Port of `ApplicationHost.GetCertificate`: the PKCS#12 (`.pfx`) file at
/// `path`, opened with `password` (a blank one is no password), as a rustls
/// server configuration.
///
/// # Errors
///
/// [`CertificateError`]: the path is blank or names no file, the file cannot
/// be read or decrypted, it holds no private key, or rustls rejects the key.
pub fn load_certificate(
    path: &str,
    password: &str,
) -> Result<Arc<rustls::ServerConfig>, CertificateError> {
    if path.trim().is_empty() {
        return Err(CertificateError::NoPath);
    }
    let Some(metadata) = std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
    else {
        return Err(CertificateError::NotFound);
    };
    if metadata.len() > MAX_CERTIFICATE_BYTES {
        return Err(CertificateError::Unreadable(format!(
            "the file is {} bytes, larger than a certificate ({MAX_CERTIFICATE_BYTES} at most)",
            metadata.len()
        )));
    }
    // A blank password is the empty password. Jellyfin passes `null` instead
    // ("Don't use an empty string password."), and .NET then also loads a
    // file keyed from the NULL password.
    // TODO(gl-815): load the NULL-password form too: its MAC and its legacy
    // (PBES1) encryption keys differ from the empty password's, and
    // p12-keystore derives the empty password's only.
    let password = if password.trim().is_empty() {
        ""
    } else {
        password
    };
    let data = std::fs::read(path).map_err(|e| CertificateError::Unreadable(e.to_string()))?;
    let keystore = p12_keystore::KeyStore::from_pkcs12(
        &data,
        password,
        p12_keystore::Pkcs12ImportPolicy::Strict,
    )
    .map_err(|e| CertificateError::Unreadable(e.to_string()))?;
    let (_, chain) = keystore
        .private_key_chain()
        .filter(|(_, chain)| !chain.certs().is_empty())
        .ok_or(CertificateError::NoPrivateKey)?;
    let certificates = chain
        .certs()
        .iter()
        .map(|certificate| rustls::pki_types::CertificateDer::from(certificate.as_der().to_vec()))
        .collect();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(chain.key().as_der().to_vec().into());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| CertificateError::Rejected(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|e| CertificateError::Rejected(e.to_string()))?;
    // The server speaks HTTP/1.1 (axum is built without HTTP/2).
    // TODO(gl-814): offer `h2` as well, as Kestrel's `Http1AndHttp2` does.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// An HTTPS listener: Kestrel's `UseHttps` on one address.
///
/// Connections are accepted on a task of their own and each handshake runs on
/// its own task, bounded by [`HANDSHAKE_TIMEOUT`], so a slow or stalled client
/// never holds up the next one; the serve loop takes finished connections.
/// Dropping the listener stops accepting and closes the socket.
pub struct TlsListener {
    accepted: tokio::sync::mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local_addr: SocketAddr,
    accept_task: tokio::task::AbortHandle,
}

impl TlsListener {
    /// Serves TLS with `config` on `listener`.
    ///
    /// # Errors
    ///
    /// The listener's address cannot be read.
    pub fn new(
        mut listener: crate::listen::Listener,
        config: Arc<rustls::ServerConfig>,
    ) -> std::io::Result<Self> {
        use axum::serve::Listener as _;
        let local_addr = listener.local_addr()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let (sender, accepted) = tokio::sync::mpsc::channel(ACCEPTED_QUEUE);
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, peer) = listener.accept().await;
                let acceptor = acceptor.clone();
                let sender = sender.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(connection)) => {
                            // Fails only once the listener is gone.
                            let _ = sender.send((connection, peer)).await;
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%peer, %error, "TLS handshake failed");
                        }
                        Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                    }
                });
            }
        })
        .abort_handle();
        Ok(Self {
            accepted,
            local_addr,
            accept_task,
        })
    }

    /// The next handshaken connection and its peer.
    pub(crate) async fn accept(&mut self) -> (TlsStream<TcpStream>, SocketAddr) {
        match self.accepted.recv().await {
            Some(connection) => connection,
            // The accept task only ends when this listener is dropped.
            None => std::future::pending().await,
        }
    }

    /// The address the listener is bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// What [`redirect_to_https`] needs to answer.
#[derive(Clone)]
pub struct HttpsRedirect {
    /// The port HTTPS is served on (`HttpsRedirectionOptions.HttpsPort`).
    pub port: u16,
    /// The normalized base URL (`PathBase`), which the redirect keeps.
    pub base: String,
    /// The network manager whose known proxies may forward the scheme and
    /// host; `None` trusts no forwarded header.
    pub network: Option<Arc<std::sync::RwLock<ferrofin_networking::NetworkManager>>>,
    /// The live server configuration, whose `CorsHosts` the redirect answers
    /// under, as the router does.
    pub config: Arc<dyn ferrofin_traits::configuration::ServerConfigurationManager>,
}

/// Port of `HttpsRedirectionMiddleware`, which Jellyfin adds when
/// `RequireHttps` is set and it listens with HTTPS: a request that is not HTTPS
/// is answered `307 Temporary Redirect` to `https://`, the same host on the
/// HTTPS port (left out when it is 443), and the same path and query.
///
/// It runs where Jellyfin runs it, inside the base URL mount (so a request
/// outside it gets the base URL redirect first, and the base is put back into
/// the location), and after `ForwardedHeadersMiddleware` and CORS:
/// - a request is HTTPS when it came over TLS, or when known proxies forwarded
///   it as `https`, and a forwarded host is the host redirected to;
/// - a CORS preflight is answered, and the redirect carries the CORS headers,
///   so a browser client on another origin can follow it. Both follow the
///   configured `CorsHosts`, as the router's CORS does
///   ([`ferrofin_api::cors::with_configured_cors`]).
///
/// Ferrofin's own probes, `/health/live` and `/health/ready`, sit outside
/// Jellyfin's application (as they sit outside the base URL) and are never
/// redirected. With no redirect (`None`) every request passes through.
///
/// A `Host` that is not a valid host is answered `400`, as Kestrel answers it
/// before any middleware runs, rather than copied into the location.
pub async fn redirect_to_https(
    State(redirect): State<Option<Arc<HttpsRedirect>>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(redirect) = redirect else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    if path.eq_ignore_ascii_case("/health/live") || path.eq_ignore_ascii_case("/health/ready") {
        return next.run(request).await;
    }
    // Only a forwarded scheme or host can change the answer, so the network
    // policy is consulted only for a request that carries one.
    let forwards = request.headers().contains_key("x-forwarded-proto")
        || request.headers().contains_key("x-forwarded-host");
    let forwarded = match (
        forwards,
        &redirect.network,
        request.extensions().get::<ConnectInfo<SocketAddr>>(),
    ) {
        (true, Some(network), Some(ConnectInfo(peer))) => {
            forwarded_through(network, peer.ip(), request.headers())
        }
        _ => ForwardedRequest::default(),
    };
    let is_https = effective_scheme(
        forwarded.scheme.as_deref(),
        request.extensions().get::<Transport>().copied(),
    ) == "https";
    if is_https {
        return next.run(request).await;
    }
    let host = match forwarded.host {
        Some(host) => Some(host),
        None => match request.headers().get(header::HOST) {
            None => Some(String::new()),
            Some(host) => host
                .to_str()
                .ok()
                .filter(|host| is_valid_host(host))
                .map(str::to_owned),
        },
    };
    let location = host.and_then(|host| {
        let host = host_without_port(&host);
        let authority = if redirect.port == 443 {
            host.to_owned()
        } else {
            format!("{host}:{}", redirect.port)
        };
        let query = request
            .uri()
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default();
        // The query is left out of the log: clients send tokens in it.
        tracing::debug!(
            host,
            port = redirect.port,
            uri_path = path,
            "redirecting to HTTPS"
        );
        HeaderValue::from_str(&format!(
            "https://{authority}{}{path}{query}",
            redirect.base
        ))
        .ok()
    });
    let answer = tower::service_fn(move |_: Request| {
        let response = match location.clone() {
            Some(location) => (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, location)],
            )
                .into_response(),
            None => StatusCode::BAD_REQUEST.into_response(),
        };
        async move { Ok::<_, std::convert::Infallible>(response) }
    });
    ferrofin_api::cors::with_configured_cors(redirect.config.as_ref(), request, answer).await
}

/// The host part of a `Host` value (`HostString.Host`): an IPv6 address keeps
/// its brackets and loses its port, any other host loses its port.
fn host_without_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |end| &host[..=end]);
    }
    host.split(':').next().unwrap_or(host)
}

/// Port of `ApplicationHost.ValidateSslCertificate`, run when Jellyfin runs it:
/// after a configuration save (`OnConfigurationUpdated`), over the saved
/// network configuration. A certificate path other than the one loaded at
/// startup flags a restart when the file exists, and is an error when it does
/// not (`FileNotFoundException`).
///
/// Every save, whoever makes it, runs the check through
/// [`Self::configuration_listener`]: it flags the restart, and logs a missing
/// file, as Jellyfin's queued `ConfigurationUpdated` handler does. A named save
/// made through the API (`NamedConfigurationUpdated`, which runs before the
/// save returns) is also answered `404` and stays saved; that is the
/// [`CommittedConfigurationCheck`] the API runs.
///
/// TODO(gl-554): `OnConfigurationUpdated` also flags a restart when the
/// dashboard's `InternalHttpPort` or `InternalHttpsPort` differ from the ports
/// in use, both non-zero, while `IsPortAuthorized` is set; it then clears
/// `IsPortAuthorized` and saves (`ApplicationHost.cs:803-818`). Port it with the
/// dashboard ports.
pub struct CertificatePathCheck {
    loaded_path: String,
    network: Arc<std::sync::RwLock<ferrofin_networking::NetworkManager>>,
    lifecycle: Arc<dyn ferrofin_core::system_manager::LifecycleController>,
}

impl CertificatePathCheck {
    /// Checks against `loaded_path`, the certificate path this lifetime
    /// started with, reading the saved configuration from `network`.
    #[must_use]
    pub fn new(
        loaded_path: String,
        network: Arc<std::sync::RwLock<ferrofin_networking::NetworkManager>>,
        lifecycle: Arc<dyn ferrofin_core::system_manager::LifecycleController>,
    ) -> Self {
        Self {
            loaded_path,
            network,
            lifecycle,
        }
    }

    /// `ValidateSslCertificate` over the saved network configuration, flagging
    /// the restart it calls for: `Ok(true)` when this call flagged it. The
    /// error is upstream's message. Logs nothing; the listener does, once.
    fn validate(&self) -> Result<bool, String> {
        let path = self
            .network
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .configuration()
            .certificate_path
            .clone();
        if path.trim().is_empty() || path == self.loaded_path {
            return Ok(false);
        }
        if std::path::Path::new(&path).is_file() {
            // A restart already pending, for this or any other reason, is not
            // announced again.
            let flagged = !self.lifecycle.has_pending_restart();
            self.lifecycle.mark_restart_required();
            Ok(flagged)
        } else {
            Err(format!("Certificate file '{path}' does not exist."))
        }
    }

    /// The listener every committed save runs. It is the one place the
    /// check logs: the restart it flags, and a missing file. The API's
    /// [`CommittedConfigurationCheck`] answers the save without logging again.
    #[must_use]
    pub fn configuration_listener(
        self: &Arc<Self>,
    ) -> Arc<ferrofin_traits::configuration::ConfigurationListener> {
        let check = Arc::clone(self);
        Arc::new(move |_update| match check.validate() {
            Ok(true) => {
                tracing::info!("the server needs a restart to use the new HTTPS certificate");
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(error, "the saved HTTPS certificate path is not usable");
            }
        })
    }
}

impl CommittedConfigurationCheck for CertificatePathCheck {
    fn check(&self, _key: &str) -> Result<(), ConfigurationValidationError> {
        self.validate()
            .map(|_| ())
            .map_err(ConfigurationValidationError::FileNotFound)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use ferrofin_core::system_manager::LifecycleController as _;
    use rstest::rstest;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tower::ServiceExt as _;

    use super::*;

    /// A self-signed certificate for `127.0.0.1`, written as PKCS#12.
    struct TestCertificate {
        der: Vec<u8>,
        pkcs12: Vec<u8>,
    }

    fn test_certificate(
        password: &str,
        encryption: p12_keystore::EncryptionAlgorithm,
    ) -> TestCertificate {
        let certified =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("certificate");
        let der = certified.cert.der().to_vec();
        let chain = p12_keystore::PrivateKeyChain::new(
            "ferrofin",
            p12_keystore::PrivateKey::from_der(&certified.signing_key.serialize_der())
                .expect("key"),
            [p12_keystore::Certificate::from_der(&der).expect("certificate")],
        );
        let mut store = p12_keystore::KeyStore::new();
        store.add_entry(
            "ferrofin",
            p12_keystore::KeyStoreEntry::PrivateKeyChain(chain),
        );
        let pkcs12 = store
            .writer(password)
            .encryption_algorithm(encryption)
            .write()
            .expect("pkcs12");
        TestCertificate { der, pkcs12 }
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_blank_or_missing_path_loads_no_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            load_certificate("", ""),
            Err(CertificateError::NoPath)
        ));
        assert!(matches!(
            load_certificate("  ", ""),
            Err(CertificateError::NoPath)
        ));
        let missing = dir.path().join("missing.pfx");
        assert!(matches!(
            load_certificate(&missing.to_string_lossy(), ""),
            Err(CertificateError::NotFound)
        ));
        // A directory is not a certificate file (`File.Exists`).
        assert!(matches!(
            load_certificate(&dir.path().to_string_lossy(), ""),
            Err(CertificateError::NotFound)
        ));
    }

    #[rstest]
    #[case::pbes2(
        "secret",
        "secret",
        p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256
    )]
    #[case::legacy_triple_des(
        "secret",
        "secret",
        p12_keystore::EncryptionAlgorithm::PbeWithShaAnd3KeyTripleDesCbc
    )]
    #[case::no_password(
        "",
        "  ",
        p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256
    )]
    fn a_pkcs12_certificate_loads_with_its_password(
        #[case] written_with: &str,
        #[case] opened_with: &str,
        #[case] encryption: p12_keystore::EncryptionAlgorithm,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let certificate = test_certificate(written_with, encryption);
        let path = write(&dir, "server.pfx", &certificate.pkcs12);
        let config = load_certificate(&path, opened_with).expect("the certificate loads");
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn a_wrong_password_or_a_file_that_is_not_pkcs12_loads_no_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let certificate = test_certificate(
            "secret",
            p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256,
        );
        let path = write(&dir, "server.pfx", &certificate.pkcs12);
        assert!(matches!(
            load_certificate(&path, "wrong"),
            Err(CertificateError::Unreadable(_))
        ));
        let text = write(&dir, "server.pem", b"-----BEGIN CERTIFICATE-----\n");
        assert!(matches!(
            load_certificate(&text, ""),
            Err(CertificateError::Unreadable(_))
        ));
    }

    #[test]
    fn a_file_too_large_for_a_certificate_is_not_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("movie.mkv");
        std::fs::File::create(&path)
            .and_then(|file| file.set_len(MAX_CERTIFICATE_BYTES + 1))
            .expect("large file");
        assert!(matches!(
            load_certificate(&path.to_string_lossy(), ""),
            Err(CertificateError::Unreadable(message)) if message.contains("larger than a certificate")
        ));
    }

    #[test]
    fn a_certificate_without_its_private_key_loads_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let certificate = test_certificate(
            "",
            p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256,
        );
        let mut store = p12_keystore::KeyStore::new();
        store.add_entry(
            "certificate",
            p12_keystore::KeyStoreEntry::Certificate(
                p12_keystore::Certificate::from_der(&certificate.der).expect("certificate"),
            ),
        );
        let path = write(
            &dir,
            "public.pfx",
            &store.writer("").write().expect("pkcs12"),
        );
        assert!(matches!(
            load_certificate(&path, ""),
            Err(CertificateError::NoPrivateKey)
        ));
    }

    #[test]
    fn https_is_served_only_when_enabled_with_a_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let certificate = test_certificate(
            "secret",
            p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256,
        );
        let path = write(&dir, "server.pfx", &certificate.pkcs12);
        let mut network = NetworkConfiguration {
            certificate_path: path,
            certificate_password: "secret".to_owned(),
            require_https: true,
            ..NetworkConfiguration::default()
        };
        assert!(HttpsServing::from_network_configuration(&network, 8096, 8920).is_none());
        network.enable_https = true;
        let serving = HttpsServing::from_network_configuration(&network, 8096, 8920)
            .expect("HTTPS is served");
        assert!(serving.require);
        // The two listeners cannot share a port; ephemeral ports never clash.
        assert!(HttpsServing::from_network_configuration(&network, 8096, 8096).is_none());
        assert!(HttpsServing::from_network_configuration(&network, 0, 0).is_some());
        network.certificate_path = dir
            .path()
            .join("missing.pfx")
            .to_string_lossy()
            .into_owned();
        assert!(HttpsServing::from_network_configuration(&network, 8096, 8920).is_none());
    }

    fn trusting(certificate: &[u8]) -> tokio_rustls::TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                certificate.to_vec(),
            ))
            .expect("root");
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    #[tokio::test]
    async fn the_listener_serves_its_certificate_while_another_handshake_stalls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let certificate = test_certificate(
            "secret",
            p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256,
        );
        let path = write(&dir, "server.pfx", &certificate.pkcs12);
        let config = load_certificate(&path, "secret").expect("certificate");
        let listener = crate::listen::bind("127.0.0.1:0".parse().expect("address")).expect("bind");
        let mut listener = TlsListener::new(listener, config).expect("listener");
        let address = listener.local_addr();

        // A client that connects and never says anything.
        let _stalled = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");

        let client = tokio::spawn(async move {
            let tcp = tokio::net::TcpStream::connect(address)
                .await
                .expect("connect");
            let name = rustls::pki_types::ServerName::IpAddress(
                std::net::IpAddr::from([127, 0, 0, 1]).into(),
            );
            let mut tls = trusting(&certificate.der)
                .connect(name, tcp)
                .await
                .expect("handshake");
            let (_, session) = tls.get_ref();
            assert_eq!(session.alpn_protocol(), Some(&b"http/1.1"[..]));
            assert_eq!(
                session.peer_certificates().map(|chain| chain[0].to_vec()),
                Some(certificate.der.clone())
            );
            tls.write_all(b"ping").await.expect("write");
            tls.flush().await.expect("flush");
        });

        let (mut stream, peer) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("the second client is accepted while the first stalls");
        assert!(peer.ip().is_loopback());
        let mut ping = [0_u8; 4];
        stream.read_exact(&mut ping).await.expect("read");
        assert_eq!(&ping, b"ping");
        client.await.expect("client");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_handshake_is_dropped_after_the_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let certificate = test_certificate(
            "secret",
            p12_keystore::EncryptionAlgorithm::PbeWithHmacSha256AndAes256,
        );
        let path = write(&dir, "server.pfx", &certificate.pkcs12);
        let config = load_certificate(&path, "secret").expect("certificate");
        let listener = crate::listen::bind("127.0.0.1:0".parse().expect("address")).expect("bind");
        let listener = TlsListener::new(listener, config).expect("listener");

        let started = tokio::time::Instant::now();
        let mut stalled = tokio::net::TcpStream::connect(listener.local_addr())
            .await
            .expect("connect");
        // The server closes the connection once the handshake times out.
        let mut byte = [0_u8; 1];
        let read = stalled.read(&mut byte).await.expect("read");
        assert_eq!(read, 0);
        assert!(started.elapsed() >= HANDSHAKE_TIMEOUT);
        drop(listener);
    }

    fn known_proxy_network(
        proxy: &str,
    ) -> Arc<std::sync::RwLock<ferrofin_networking::NetworkManager>> {
        let config = NetworkConfiguration {
            known_proxies: vec![proxy.to_owned()],
            ..NetworkConfiguration::default()
        };
        Arc::new(std::sync::RwLock::new(
            ferrofin_networking::NetworkManager::with_interfaces(config, Vec::new()),
        ))
    }

    /// A configuration whose only setting the redirect reads, `CorsHosts`.
    struct CorsHosts(Vec<String>);

    #[async_trait::async_trait]
    impl ferrofin_traits::configuration::ServerConfigurationManager for CorsHosts {
        fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
            unreachable!("the HTTPS redirect reads only the server configuration")
        }
        async fn configuration(
            &self,
        ) -> Result<
            Arc<ferrofin_model::configuration::ServerConfiguration>,
            ferrofin_traits::error::ServiceError,
        > {
            Ok(Arc::new(
                ferrofin_model::configuration::ServerConfiguration {
                    cors_hosts: self.0.clone(),
                    ..ferrofin_model::configuration::ServerConfiguration::default()
                },
            ))
        }
        async fn update_configuration(
            &self,
            _configuration: &ferrofin_model::configuration::ServerConfiguration,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unreachable!("the HTTPS redirect never saves")
        }
        async fn get_branding(
            &self,
        ) -> Result<ferrofin_model::branding::BrandingOptions, ferrofin_traits::error::ServiceError>
        {
            unreachable!("the HTTPS redirect never reads branding")
        }
        async fn update_branding(
            &self,
            _branding: &ferrofin_model::branding::BrandingOptions,
        ) -> Result<(), ferrofin_traits::error::ServiceError> {
            unreachable!("the HTTPS redirect never saves")
        }
    }

    fn redirect(port: u16, base: &str) -> Arc<HttpsRedirect> {
        redirect_with_cors_hosts(port, base, &[])
    }

    fn redirect_with_cors_hosts(port: u16, base: &str, cors_hosts: &[&str]) -> Arc<HttpsRedirect> {
        Arc::new(HttpsRedirect {
            port,
            base: base.to_owned(),
            network: Some(known_proxy_network("10.0.0.1")),
            config: Arc::new(CorsHosts(
                cors_hosts.iter().map(|host| (*host).to_owned()).collect(),
            )),
        })
    }

    async fn send(
        redirect: Option<Arc<HttpsRedirect>>,
        request: axum::http::request::Builder,
        peer: &str,
        transport: Transport,
    ) -> Response {
        let app = Router::new()
            .route("/", get(|| async { "inner" }))
            .route("/{*path}", get(|| async { "inner" }))
            .layer(axum::middleware::from_fn_with_state(
                redirect,
                redirect_to_https,
            ));
        let mut request = request.body(Body::empty()).expect("request");
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            peer.parse().expect("peer"),
            50_000,
        )));
        request.extensions_mut().insert(transport);
        app.oneshot(request).await.expect("infallible")
    }

    fn location(response: &Response) -> Option<&str> {
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|location| location.to_str().ok())
    }

    #[rstest]
    #[case::port_and_base(
        8920,
        "/jf",
        "example.com:8096",
        "/System/Info/Public?x=1",
        "https://example.com:8920/jf/System/Info/Public?x=1"
    )]
    #[case::default_port_left_out(
        443,
        "",
        "example.com:8096",
        "/System/Info/Public",
        "https://example.com/System/Info/Public"
    )]
    #[case::ipv6_host(8920, "", "[::1]:8096", "/web/", "https://[::1]:8920/web/")]
    #[case::no_port_on_the_host(8920, "", "media.example", "/", "https://media.example:8920/")]
    #[tokio::test]
    async fn plain_http_is_redirected_to_the_https_port(
        #[case] port: u16,
        #[case] base: &str,
        #[case] host: &str,
        #[case] path: &str,
        #[case] expected: &str,
    ) {
        let response = send(
            Some(redirect(port, base)),
            Request::get(path).header(header::HOST, host),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(location(&response), Some(expected));
    }

    #[rstest]
    #[case::over_tls("/System/Info/Public", "192.168.1.20", Transport::Tls, None)]
    #[case::liveness_probe("/health/live", "192.168.1.20", Transport::Plain, None)]
    #[case::readiness_probe("/health/ready", "192.168.1.20", Transport::Plain, None)]
    #[case::forwarded_https_from_a_known_proxy(
        "/System/Info/Public",
        "10.0.0.1",
        Transport::Plain,
        Some("https")
    )]
    #[tokio::test]
    async fn https_requests_and_the_probes_pass_through(
        #[case] path: &str,
        #[case] peer: &str,
        #[case] transport: Transport,
        #[case] forwarded_proto: Option<&str>,
    ) {
        let mut request = Request::get(path).header(header::HOST, "example.com:8096");
        if let Some(proto) = forwarded_proto {
            request = request.header("x-forwarded-proto", proto);
        }
        let response = send(Some(redirect(8920, "")), request, peer, transport).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn without_a_redirect_nothing_is_redirected() {
        let response = send(
            None,
            Request::get("/System/Info/Public").header(header::HOST, "example.com"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn only_a_known_proxy_forwards_the_scheme_and_host() {
        // An unknown peer's `https` is not believed.
        let response = send(
            Some(redirect(8920, "")),
            Request::get("/web/")
                .header(header::HOST, "example.com:8096")
                .header("x-forwarded-proto", "https"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(location(&response), Some("https://example.com:8920/web/"));

        // A known proxy forwarding plain HTTP is redirected to the host it forwarded.
        let response = send(
            Some(redirect(8920, "")),
            Request::get("/web/")
                .header(header::HOST, "10.0.0.1:8096")
                .header("x-forwarded-for", "203.0.113.9")
                .header("x-forwarded-proto", "http")
                .header("x-forwarded-host", "media.example:80"),
            "10.0.0.1",
            Transport::Plain,
        )
        .await;
        assert_eq!(location(&response), Some("https://media.example:8920/web/"));
    }

    #[tokio::test]
    async fn an_invalid_host_is_refused_rather_than_redirected_to() {
        let response = send(
            Some(redirect(8920, "")),
            Request::get("/web/").header(header::HOST, "evil.example/path"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(location(&response).is_none());
    }

    #[tokio::test]
    async fn cors_answers_a_preflight_and_marks_the_redirect() {
        let preflight = send(
            Some(redirect(8920, "")),
            Request::options("/Users/AuthenticateByName")
                .header(header::HOST, "example.com:8096")
                .header(header::ORIGIN, "https://app.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(preflight.status(), StatusCode::OK);
        assert!(location(&preflight).is_none());
        assert_eq!(
            preflight.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("*"))
        );

        let redirected = send(
            Some(redirect(8920, "")),
            Request::get("/System/Info/Public")
                .header(header::HOST, "example.com:8096")
                .header(header::ORIGIN, "https://app.example"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(redirected.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            redirected
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("*"))
        );
    }

    #[tokio::test]
    async fn the_redirect_follows_the_configured_cors_hosts() {
        let redirect = redirect_with_cors_hosts(8920, "", &["https://app.example"]);
        let listed = send(
            Some(Arc::clone(&redirect)),
            Request::get("/System/Info/Public")
                .header(header::HOST, "example.com:8096")
                .header(header::ORIGIN, "https://app.example"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(listed.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            listed.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://app.example"))
        );
        assert_eq!(
            listed
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            Some(&HeaderValue::from_static("true"))
        );

        let other = send(
            Some(Arc::clone(&redirect)),
            Request::get("/System/Info/Public")
                .header(header::HOST, "example.com:8096")
                .header(header::ORIGIN, "https://other.example"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(other.status(), StatusCode::TEMPORARY_REDIRECT);
        assert!(
            other
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );

        let unlisted_preflight = send(
            Some(Arc::clone(&redirect)),
            Request::options("/Users/AuthenticateByName")
                .header(header::HOST, "example.com:8096")
                .header(header::ORIGIN, "https://other.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert!(
            location(&unlisted_preflight).is_none(),
            "a preflight is answered, not redirected"
        );
        assert!(
            unlisted_preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );

        let no_origin = send(
            Some(redirect),
            Request::get("/System/Info/Public").header(header::HOST, "example.com:8096"),
            "192.168.1.20",
            Transport::Plain,
        )
        .await;
        assert_eq!(no_origin.status(), StatusCode::TEMPORARY_REDIRECT);
        assert!(
            no_origin
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
        assert!(
            no_origin.headers().get(header::VARY).is_none(),
            "no CORS request"
        );
    }

    #[rstest]
    #[case("example.com:8096", "example.com")]
    #[case("example.com", "example.com")]
    #[case("[::1]:8096", "[::1]")]
    #[case("[::1]", "[::1]")]
    #[case("", "")]
    fn the_host_loses_its_port(#[case] host: &str, #[case] expected: &str) {
        assert_eq!(host_without_port(host), expected);
    }

    #[derive(Default)]
    struct FakeLifecycle {
        restart: AtomicBool,
    }

    #[async_trait::async_trait]
    impl ferrofin_core::system_manager::LifecycleController for FakeLifecycle {
        async fn stop(&self, _restart: bool) -> Result<(), ferrofin_traits::error::ServiceError> {
            Ok(())
        }
        fn has_pending_restart(&self) -> bool {
            self.restart.load(Ordering::SeqCst)
        }
        fn mark_restart_required(&self) {
            self.restart.store(true, Ordering::SeqCst);
        }
        fn is_shutting_down(&self) -> bool {
            false
        }
    }

    fn certificate_check(
        loaded: &str,
        saved: &str,
    ) -> (Arc<CertificatePathCheck>, Arc<FakeLifecycle>) {
        let network = Arc::new(std::sync::RwLock::new(
            ferrofin_networking::NetworkManager::with_interfaces(
                NetworkConfiguration {
                    certificate_path: saved.to_owned(),
                    ..NetworkConfiguration::default()
                },
                Vec::new(),
            ),
        ));
        let lifecycle = Arc::new(FakeLifecycle::default());
        let check = Arc::new(CertificatePathCheck::new(
            loaded.to_owned(),
            network,
            Arc::clone(&lifecycle) as Arc<dyn ferrofin_core::system_manager::LifecycleController>,
        ));
        (check, lifecycle)
    }

    #[test]
    fn an_unchanged_or_blank_certificate_path_changes_nothing() {
        for (loaded, saved) in [("/certs/a.pfx", "/certs/a.pfx"), ("/certs/a.pfx", " ")] {
            let (check, lifecycle) = certificate_check(loaded, saved);
            assert!(check.check("network").is_ok());
            assert!(!lifecycle.has_pending_restart());
        }
    }

    #[test]
    fn a_new_certificate_file_needs_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "new.pfx", b"pfx");
        let (check, lifecycle) = certificate_check("", &path);
        assert!(check.check("network").is_ok());
        assert!(lifecycle.has_pending_restart());
    }

    #[test]
    fn the_restart_is_reported_as_newly_flagged_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "new.pfx", b"pfx");
        let (check, lifecycle) = certificate_check("", &path);
        assert_eq!(check.validate(), Ok(true));
        assert_eq!(check.validate(), Ok(false));
        assert!(lifecycle.has_pending_restart());
    }

    #[test]
    fn a_missing_certificate_file_answers_the_save() {
        let (check, lifecycle) = certificate_check("/certs/a.pfx", "/certs/missing.pfx");
        let error = check.check("encoding").expect_err("the file is missing");
        assert!(matches!(
            &error,
            ConfigurationValidationError::FileNotFound(message)
                if message == "Certificate file '/certs/missing.pfx' does not exist."
        ));
        assert!(!lifecycle.has_pending_restart());
    }

    #[test]
    fn every_save_runs_the_check_through_the_listener() {
        use ferrofin_traits::configuration::ConfigurationUpdate;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(&dir, "new.pfx", b"pfx");
        let updates = [
            ConfigurationUpdate::Server(Arc::default()),
            ConfigurationUpdate::Named {
                key: "encoding".to_owned(),
                json: Arc::from("{}"),
            },
        ];
        for update in &updates {
            let (check, lifecycle) = certificate_check("", &path);
            (check.configuration_listener())(update);
            assert!(lifecycle.has_pending_restart());

            // A missing file is logged; nothing to restart for.
            let (check, lifecycle) = certificate_check("", "/certs/missing.pfx");
            (check.configuration_listener())(update);
            assert!(!lifecycle.has_pending_restart());
        }
    }
}
