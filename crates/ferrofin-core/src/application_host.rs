//! [`FerrofinServerApplicationHost`] — the concrete [`ServerApplicationHost`].
//!
//! Port of the *server-relevant* subset of
//! `Emby.Server.Implementations.ApplicationHost`: the network-facing facts
//! (ports, HTTPS, friendly name), the two URL builders, and the virtual-path
//! expand/reverse pair. The DI container, plugin loader, assembly scanning, and
//! lifetime plumbing that dominate the C# class are intentionally dropped (they
//! belong to the Wave 8 composition root).
//!
//! The composition root injects the listener facts and shared network manager.
//! Peer-aware URLs resolve against current interfaces and published subnet overrides;
//! HTTP requests may additionally use their Host header when configured. The friendly
//! name derives from the live server configuration.
//!
//! `expand_virtual_path`/`reverse_virtual_path` reproduce the two-step
//! `String.Replace` chain over the `%AppDataPath%`/`%MetadataPath%` placeholders,
//! reading the live data/metadata paths off the shared application paths.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, RwLock};

use ferrofin_networking::NetworkManager;

use async_trait::async_trait;
use ferrofin_traits::configuration::{
    ConfigurationListener, ConfigurationUpdate, ServerConfigurationManager,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::RequestContext;
use ferrofin_traits::system::{ServerApplicationHost, ServerApplicationPaths};

use crate::app_paths::FerrofinServerApplicationPaths;
use crate::virtual_paths::replace_ignore_ascii_case;

/// The application product name (`ApplicationHost.ApplicationProductName`).
///
/// In C# this is `FileVersionInfo.GetVersionInfo(entryAssembly).ProductName`,
/// which for the Jellyfin server assembly is the literal below. Ferrofin speaks
/// Jellyfin's API, so the constant is the same string clients already expect
/// from `GET /System/Ping` and `PublicSystemInfo.ProductName`.
pub const PRODUCT_NAME: &str = "Jellyfin Server";

/// The network-facing facts the host reports and uses to build URLs.
///
/// Filled by the composition root from the actual listener configuration.
/// Field meanings mirror the C# `ApplicationHost`
/// properties of the same name.
#[derive(Debug, Clone)]
pub struct HostNetworkInfo {
    /// The HTTP listen port (`HttpPort`).
    pub http_port: u16,
    /// The HTTPS listen port (`HttpsPort`).
    pub https_port: u16,
    /// Whether the server listens over HTTPS (`ListenWithHttps`).
    pub listen_with_https: bool,
    /// The explicit published server URL, if configured (`PublishedServerUrl`);
    /// when set it overrides all URL computation.
    pub published_server_url: Option<String>,
    /// The URL base path prefix (`NetworkConfiguration.BaseUrl`), e.g.
    /// `/jellyfin`; empty for none.
    pub base_url: String,
    /// Whether the smart API URL should echo the request's own host
    /// (`EnablePublishedServerUriByRequest`).
    pub enable_published_server_uri_by_request: bool,
}

impl Default for HostNetworkInfo {
    fn default() -> Self {
        Self {
            http_port: 8096,
            https_port: 8920,
            listen_with_https: false,
            published_server_url: None,
            base_url: String::new(),
            enable_published_server_uri_by_request: false,
        }
    }
}

/// The concrete server application host.
///
/// Holds the shared application paths (for virtual-path expansion), the injected
/// configuration manager (for the friendly name), the network facts, and the
/// startup-completed flag.
pub struct FerrofinServerApplicationHost {
    paths: Arc<FerrofinServerApplicationPaths>,
    configuration_manager: Arc<dyn ServerConfigurationManager>,
    network: HostNetworkInfo,
    network_manager: Option<Arc<RwLock<NetworkManager>>>,
    http_bind: Option<IpAddr>,
    bound_http_port: AtomicU16,
    machine_name: String,
    /// The last-published server name (`Configuration.ServerName`), or `None`
    /// when empty — in which case the friendly name is the machine name.
    server_name: std::sync::RwLock<Option<String>>,
    core_startup_completed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for FerrofinServerApplicationHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinServerApplicationHost")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl FerrofinServerApplicationHost {
    /// Creates a host over the given paths, configuration manager, and network
    /// facts.
    ///
    /// `machine_name` is the fallback friendly name used when
    /// `Configuration.ServerName` is blank (C# `Environment.MachineName`).
    #[must_use]
    pub fn new(
        paths: Arc<FerrofinServerApplicationPaths>,
        configuration_manager: Arc<dyn ServerConfigurationManager>,
        network: HostNetworkInfo,
        machine_name: impl Into<String>,
    ) -> Self {
        let network = HostNetworkInfo {
            base_url: ferrofin_networking::normalize_base_url(&network.base_url),
            ..network
        };
        Self {
            paths,
            configuration_manager,
            bound_http_port: AtomicU16::new(network.http_port),
            network,
            network_manager: None,
            http_bind: None,
            machine_name: machine_name.into(),
            server_name: std::sync::RwLock::new(None),
            core_startup_completed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Shares live network policy and constrains advertisement to the HTTP bind.
    #[must_use]
    pub fn with_network(mut self, manager: Arc<RwLock<NetworkManager>>, http_bind: IpAddr) -> Self {
        self.network_manager = Some(manager);
        self.http_bind = (!http_bind.is_unspecified()).then_some(http_bind);
        self
    }

    /// Records the actual HTTP port after binding, including an ephemeral port.
    pub fn set_bound_http_port(&self, port: u16) {
        self.bound_http_port.store(port, Ordering::Relaxed);
    }

    /// Marks core startup as complete (the composition root calls this once the
    /// server is fully wired).
    pub fn mark_core_startup_complete(&self) {
        self.core_startup_completed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Refreshes the cached server name from the live configuration.
    ///
    /// The [`friendly_name`](ServerApplicationHost::friendly_name) getter is
    /// synchronous, so the async `Configuration.ServerName` is snapshotted here
    /// (at startup and whenever the configuration changes) rather than fetched on
    /// each call. An empty name clears the cache, so the friendly name falls back
    /// to the machine name.
    ///
    /// # Errors
    ///
    /// Propagates any failure reading the current configuration.
    pub async fn refresh_server_name(&self) -> Result<(), ServiceError> {
        let configuration = self.configuration_manager.configuration().await?;
        self.publish_server_name(&configuration.server_name);
        Ok(())
    }

    /// Returns an observer keeping the advertised name in sync with saves.
    ///
    /// Register it before the initial refresh. It retains a weak host reference
    /// so the host/configuration-manager pair can be dropped on restart.
    #[must_use]
    pub fn configuration_listener(self: &Arc<Self>) -> Arc<ConfigurationListener> {
        let host = Arc::downgrade(self);
        Arc::new(move |update| {
            if let ConfigurationUpdate::Server(configuration) = update
                && let Some(host) = host.upgrade()
            {
                host.publish_server_name(&configuration.server_name);
            }
        })
    }

    fn publish_server_name(&self, name: &str) {
        // ApplicationHost.FriendlyName uses IsNullOrEmpty, not whitespace trim.
        let published = if name.is_empty() {
            None
        } else {
            Some(name.to_owned())
        };
        if let Ok(mut guard) = self.server_name.write() {
            *guard = published;
        }
    }

    /// Builds a URL from a host, an optional scheme, and an optional port,
    /// reproducing C# `GetLocalApiUrl`.
    ///
    /// If `hostname` already looks like a URL (`http…`) it is returned trimmed.
    /// Otherwise the scheme defaults to the HTTPS/HTTP listen mode, the port
    /// defaults to the matching listen port (omitted for the scheme's default
    /// port), and the base URL is appended. The trailing slash is always trimmed.
    fn build_local_api_url(
        &self,
        hostname: &str,
        scheme: Option<&str>,
        port: Option<u16>,
    ) -> String {
        if hostname.to_ascii_lowercase().starts_with("http://")
            || hostname.to_ascii_lowercase().starts_with("https://")
        {
            return hostname.trim_end_matches('/').to_owned();
        }

        let scheme = scheme.unwrap_or(if self.network.listen_with_https {
            "https"
        } else {
            "http"
        });
        let is_https = scheme.eq_ignore_ascii_case("https");
        let port = port.unwrap_or(if is_https {
            self.network.https_port
        } else {
            self.http_port()
        });

        // Omit the port when it is the scheme's default (80/443), matching the
        // `UriBuilder` behavior of not rendering a default port.
        let default_port = if is_https { 443 } else { 80 };
        // The public builder also accepts unbracketed IPv6 literals.
        let hostname = hostname
            .parse::<std::net::Ipv6Addr>()
            .map_or_else(|_| hostname.to_owned(), |address| format!("[{address}]"));
        let base = self.network.base_url.trim_end_matches('/');
        let url = if port == default_port {
            format!("{scheme}://{hostname}{base}")
        } else {
            format!("{scheme}://{hostname}:{port}{base}")
        };
        url.trim_end_matches('/').to_owned()
    }

    /// Parses the `Host` header of a request into `(host, port)`.
    ///
    /// The port is `None` when absent or unparseable. IPv6 literals in
    /// brackets (`[::1]:8096`) are handled.
    fn parse_request_host(request: &RequestContext) -> Option<(String, Option<u16>)> {
        let host = request.header("host")?;
        if let Some(rest) = host.strip_prefix('[') {
            // IPv6 literal: [addr]:port
            let (addr, tail) = rest.split_once(']')?;
            let port = tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
            return Some((format!("[{addr}]"), port));
        }
        match host.rsplit_once(':') {
            Some((h, p)) => Some((h.to_owned(), p.parse::<u16>().ok())),
            None => Some((host.to_owned(), None)),
        }
    }

    /// Infers the request scheme from the forwarded-proto header, defaulting to
    /// the host's listen mode.
    fn request_scheme(&self, request: &RequestContext) -> String {
        request.header("x-forwarded-proto").map_or_else(
            || {
                if self.network.listen_with_https {
                    "https".to_owned()
                } else {
                    "http".to_owned()
                }
            },
            str::to_owned,
        )
    }
}

#[async_trait]
impl ServerApplicationHost for FerrofinServerApplicationHost {
    fn core_startup_has_completed(&self) -> bool {
        self.core_startup_completed
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn http_port(&self) -> u16 {
        self.bound_http_port.load(Ordering::Relaxed)
    }

    fn https_port(&self) -> u16 {
        self.network.https_port
    }

    fn listen_with_https(&self) -> bool {
        self.network.listen_with_https
    }

    fn name(&self) -> String {
        // C# `ApplicationHost.Name => ApplicationProductName` — a build
        // constant, deliberately NOT the friendly name.
        PRODUCT_NAME.to_owned()
    }

    fn friendly_name(&self) -> String {
        // Configuration.ServerName ?? Environment.MachineName. The name is
        // snapshotted by `refresh_server_name`; fall back to the machine name
        // when unset (or the lock is poisoned).
        self.server_name
            .read()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| self.machine_name.clone())
    }

    async fn get_smart_api_url(&self, request: &RequestContext) -> Result<String, ServiceError> {
        if self.network.enable_published_server_uri_by_request
            && let Some((host, req_port)) = Self::parse_request_host(request)
        {
            let scheme = self.request_scheme(request);
            // C# passes -1 when Host omits the port, so UriBuilder omits it
            // rather than substituting the server's internal listen port.
            let port = Some(req_port.unwrap_or(if scheme.eq_ignore_ascii_case("https") {
                443
            } else {
                80
            }));
            return Ok(self.build_local_api_url(&host, Some(&scheme), port));
        }

        let peer = request
            .remote_endpoint
            .as_deref()
            .and_then(|endpoint| {
                endpoint.parse::<IpAddr>().ok().or_else(|| {
                    endpoint
                        .parse::<SocketAddr>()
                        .ok()
                        .map(|address| address.ip())
                })
            })
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        self.get_smart_api_url_for_peer(peer).await
    }

    async fn get_smart_api_url_for_peer(&self, peer: IpAddr) -> Result<String, ServiceError> {
        if let Some(published) = &self.network.published_server_url
            && !published.is_empty()
        {
            return Ok(published.trim_matches('/').to_owned());
        }
        let Some(manager) = &self.network_manager else {
            return Ok(self.build_local_api_url("localhost", None, None));
        };
        let (hostname, port) = {
            let mut manager = manager
                .write()
                .map_err(|_| ServiceError::backend("network manager lock poisoned"))?;
            manager.refresh_interfaces();
            manager.get_bind_address_for_peer(peer, self.http_bind)
        };
        Ok(self.build_local_api_url(&hostname, None, port))
    }

    async fn get_local_api_url(
        &self,
        hostname: &str,
        scheme: Option<&str>,
        port: Option<u16>,
    ) -> Result<String, ServiceError> {
        Ok(self.build_local_api_url(hostname, scheme, port))
    }

    fn expand_virtual_path(&self, path: &str) -> String {
        let data = self.paths.data_path();
        let metadata = self.paths.internal_metadata_path();
        replace_ignore_ascii_case(
            &replace_ignore_ascii_case(
                path,
                FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
                &data,
            ),
            FerrofinServerApplicationPaths::VIRTUAL_INTERNAL_METADATA_PATH,
            &metadata,
        )
    }

    fn reverse_virtual_path(&self, path: &str) -> String {
        let data = self.paths.data_path();
        let metadata = self.paths.internal_metadata_path();
        replace_ignore_ascii_case(
            &replace_ignore_ascii_case(
                path,
                &data,
                FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
            ),
            &metadata,
            FerrofinServerApplicationPaths::VIRTUAL_INTERNAL_METADATA_PATH,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_paths::test_paths;
    use crate::configuration_manager::FerrofinServerConfigurationManager;

    async fn host(network: HostNetworkInfo) -> FerrofinServerApplicationHost {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Leak the tempdir so the paths remain valid for the host's lifetime in
        // the test; the process exits shortly after.
        let root = tmp.keep();
        let paths = test_paths(&root);
        let cfg = Arc::new(
            FerrofinServerConfigurationManager::load(Arc::clone(&paths))
                .await
                .expect("load config"),
        );
        FerrofinServerApplicationHost::new(paths, cfg, network, "test-machine")
    }

    #[tokio::test]
    async fn local_api_url_builds_scheme_host_port() {
        let h = host(HostNetworkInfo::default()).await;
        assert_eq!(
            h.get_local_api_url("192.168.1.5", None, None)
                .await
                .unwrap(),
            "http://192.168.1.5:8096"
        );
        assert_eq!(
            h.get_local_api_url("192.168.1.5", Some("https"), Some(443))
                .await
                .unwrap(),
            "https://192.168.1.5"
        );
    }

    #[tokio::test]
    async fn local_api_url_passes_through_full_url() {
        let h = host(HostNetworkInfo::default()).await;
        assert_eq!(
            h.get_local_api_url("https://jelly.example.com/", None, None)
                .await
                .unwrap(),
            "https://jelly.example.com"
        );
    }

    #[tokio::test]
    async fn base_url_is_appended() {
        let net = HostNetworkInfo {
            base_url: "/jellyfin".to_owned(),
            ..Default::default()
        };
        let h = host(net).await;
        assert_eq!(
            h.get_local_api_url("host", None, Some(8096)).await.unwrap(),
            "http://host:8096/jellyfin"
        );
    }

    #[tokio::test]
    async fn published_url_wins() {
        let net = HostNetworkInfo {
            published_server_url: Some("https://public.example.com/".to_owned()),
            ..Default::default()
        };
        let h = host(net).await;
        let req = RequestContext::default();
        assert_eq!(
            h.get_smart_api_url(&req).await.unwrap(),
            "https://public.example.com"
        );
    }

    #[tokio::test]
    async fn smart_url_echoes_request_host() {
        let net = HostNetworkInfo {
            enable_published_server_uri_by_request: true,
            ..Default::default()
        };
        let h = host(net).await;
        let req = RequestContext {
            headers: vec![
                ("Host".to_owned(), "media.lan:8096".to_owned()),
                ("X-Forwarded-Proto".to_owned(), "http".to_owned()),
            ],
            ..Default::default()
        };
        assert_eq!(
            h.get_smart_api_url(&req).await.unwrap(),
            "http://media.lan:8096"
        );
    }

    #[tokio::test]
    async fn request_host_branch_precedes_published_url_and_omits_default_port() {
        let h = host(HostNetworkInfo {
            enable_published_server_uri_by_request: true,
            published_server_url: Some("https://configured.example.test".into()),
            ..Default::default()
        })
        .await;
        for host in ["media.example.test", "media.example.test:443"] {
            let request = RequestContext {
                headers: vec![
                    ("Host".into(), host.into()),
                    ("X-Forwarded-Proto".into(), "https".into()),
                ],
                ..Default::default()
            };
            assert_eq!(
                h.get_smart_api_url(&request).await.expect("url"),
                "https://media.example.test"
            );
        }
    }

    #[tokio::test]
    async fn http_named_machine_is_a_hostname_not_a_complete_url() {
        let h = host(HostNetworkInfo::default()).await;
        assert_eq!(
            h.get_local_api_url("http-media", None, None)
                .await
                .expect("url"),
            "http://http-media:8096"
        );
    }

    fn test_network() -> Arc<RwLock<NetworkManager>> {
        Arc::new(RwLock::new(NetworkManager::with_defaults(
            ferrofin_networking::NetworkConfiguration {
                enable_ipv6: true,
                ..Default::default()
            },
            "192.168.1.2/24,2,eth0|10.20.0.2/24,3,eth1|fd12::2/64,4,eth2",
        )))
    }

    #[tokio::test]
    async fn peer_urls_select_matching_subnet_and_http_uses_remote_endpoint() {
        let h = host(HostNetworkInfo::default())
            .await
            .with_network(test_network(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        for (peer, expected) in [
            ("192.168.1.45", "http://192.168.1.2:8096"),
            ("10.20.0.45", "http://10.20.0.2:8096"),
            ("fd12::45", "http://[fd12::2]:8096"),
        ] {
            assert_eq!(
                h.get_smart_api_url_for_peer(peer.parse().expect("ip"))
                    .await
                    .expect("url"),
                expected
            );
        }
        let request = RequestContext {
            remote_endpoint: Some("10.20.0.45:51000".into()),
            ..Default::default()
        };
        assert_eq!(
            h.get_smart_api_url(&request).await.expect("url"),
            "http://10.20.0.2:8096"
        );
    }

    #[tokio::test]
    async fn explicit_http_bind_and_actual_port_constrain_advertisement() {
        let h = host(HostNetworkInfo {
            base_url: "/jellyfin/".into(),
            http_port: 0,
            ..Default::default()
        })
        .await
        .with_network(test_network(), "192.168.1.2".parse().expect("bind"));
        h.set_bound_http_port(18096);
        assert_eq!(h.http_port(), 18096);
        assert_eq!(
            h.get_smart_api_url_for_peer("10.20.0.45".parse().expect("peer"))
                .await
                .expect("url"),
            "http://192.168.1.2:18096/jellyfin",
        );
    }

    #[tokio::test]
    async fn configuration_save_updates_shared_published_subnet_override() {
        let network = test_network();
        let h = host(HostNetworkInfo::default())
            .await
            .with_network(Arc::clone(&network), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let peer = "192.168.1.45".parse().expect("peer");
        assert_eq!(
            h.get_smart_api_url_for_peer(peer).await.expect("url"),
            "http://192.168.1.2:8096"
        );
        network.write().expect("lock").update_settings(
            &ferrofin_networking::NetworkConfiguration {
                published_server_uri_by_subnet: vec![
                    "192.168.1.0/24=https://media.example.test:8443/jellyfin/".into(),
                ],
                ..Default::default()
            },
        );
        assert_eq!(
            h.get_smart_api_url_for_peer(peer).await.expect("url"),
            "https://media.example.test:8443/jellyfin"
        );
    }

    #[tokio::test]
    async fn explicit_published_url_wins_over_bind_and_network_overrides() {
        let h = host(HostNetworkInfo {
            published_server_url: Some("https://public.example.test/media/".into()),
            ..Default::default()
        })
        .await
        .with_network(test_network(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(
            h.get_smart_api_url_for_peer("10.20.0.45".parse().expect("peer"))
                .await
                .expect("url"),
            "https://public.example.test/media"
        );
    }

    #[tokio::test]
    async fn local_url_formats_unbracketed_ipv6_literal() {
        let h = host(HostNetworkInfo::default()).await;
        assert_eq!(
            h.get_local_api_url("fd12::2", None, None)
                .await
                .expect("url"),
            "http://[fd12::2]:8096"
        );
    }

    #[tokio::test]
    async fn virtual_path_expand_and_reverse_roundtrip() {
        let h = host(HostNetworkInfo::default()).await;
        let data = h.paths.data_path();
        let virtual_path = format!(
            "{}/subtitles/x.srt",
            FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH
        );
        let expanded = h.expand_virtual_path(&virtual_path);
        assert_eq!(expanded, format!("{data}/subtitles/x.srt"));
        assert_eq!(h.reverse_virtual_path(&expanded), virtual_path);
    }
    #[tokio::test]
    async fn saved_names_update_the_host_and_empty_names_restore_the_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let paths = test_paths(directory.path());
        let configuration = Arc::new(
            FerrofinServerConfigurationManager::load(Arc::clone(&paths))
                .await
                .unwrap(),
        );
        let host = Arc::new(FerrofinServerApplicationHost::new(
            paths,
            configuration.clone(),
            HostNetworkInfo::default(),
            "fixture-machine",
        ));
        configuration.add_configuration_listener(host.configuration_listener());
        host.refresh_server_name().await.unwrap();
        for (saved, expected) in [
            ("Living Room", "Living Room"),
            ("   ", "   "),
            ("", "fixture-machine"),
        ] {
            let mut update = configuration.snapshot();
            update.server_name = saved.into();
            configuration.update_configuration(&update).await.unwrap();
            assert_eq!(host.friendly_name(), expected);
        }
        configuration
            .named_configuration_updated("branding", Arc::from(r#"{"ServerName":"unrelated"}"#));
        assert_eq!(host.friendly_name(), "fixture-machine");
        let weak = Arc::downgrade(&host);
        drop(host);
        assert!(
            weak.upgrade().is_none(),
            "the subscription must not retain a stopped host"
        );
        configuration
            .update_configuration(&configuration.snapshot())
            .await
            .unwrap();
    }
}
