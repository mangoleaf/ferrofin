//! [`FerrofinServerApplicationHost`] — the concrete [`ServerApplicationHost`].
//!
//! Port of the *server-relevant* subset of
//! `Emby.Server.Implementations.ApplicationHost`: the network-facing facts
//! (ports, HTTPS, friendly name), the two URL builders, and the virtual-path
//! expand/reverse pair. The DI container, plugin loader, assembly scanning, and
//! lifetime plumbing that dominate the C# class are intentionally dropped (they
//! belong to the Wave 8 composition root).
//!
//! Injected inputs vs. C#:
//! - In C# the ports, HTTPS flag, published-server URL and base URL are read
//!   live from `NetworkConfiguration` (owned by the networking layer, which this
//!   crate must not depend on). They are therefore taken as constructor inputs
//!   ([`HostNetworkInfo`]) that the composition root fills from the network
//!   manager. The friendly name still derives from the live
//!   [`ServerConfiguration::server_name`] via the injected configuration manager.
//! - `NetManager.GetBindAddress` (which turns a remote address into the best
//!   bind host) is a networking concern; [`get_smart_api_url`] here honors the
//!   published-server URL when set and otherwise builds a URL from the request's
//!   `Host` header, reproducing the `EnablePublishedServerUriByRequest` branch of
//!   the C# `GetSmartApiUrl(HttpRequest)`.
//!
//! `expand_virtual_path`/`reverse_virtual_path` reproduce the two-step
//! `String.Replace` chain over the `%AppDataPath%`/`%MetadataPath%` placeholders,
//! reading the live data/metadata paths off the shared application paths.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::RequestContext;
use ferrofin_traits::system::{ServerApplicationHost, ServerApplicationPaths};

use crate::app_paths::FerrofinServerApplicationPaths;

/// The application product name (`ApplicationHost.ApplicationProductName`).
///
/// In C# this is `FileVersionInfo.GetVersionInfo(entryAssembly).ProductName`,
/// which for the Jellyfin server assembly is the literal below. Ferrofin speaks
/// Jellyfin's API, so the constant is the same string clients already expect
/// from `GET /System/Ping` and `PublicSystemInfo.ProductName`.
pub const PRODUCT_NAME: &str = "Jellyfin Server";

/// The network-facing facts the host reports and uses to build URLs.
///
/// Filled by the composition root from the live network configuration (which
/// this crate cannot depend on). Field meanings mirror the C# `ApplicationHost`
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
    machine_name: String,
    /// The last-published server name (`Configuration.ServerName`), or `None`
    /// when blank — in which case the friendly name is the machine name.
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
        Self {
            paths,
            configuration_manager,
            network,
            machine_name: machine_name.into(),
            server_name: std::sync::RwLock::new(None),
            core_startup_completed: std::sync::atomic::AtomicBool::new(false),
        }
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
    /// each call. A blank name clears the cache, so the friendly name falls back
    /// to the machine name.
    ///
    /// # Errors
    ///
    /// Propagates any failure reading the current configuration.
    pub async fn refresh_server_name(&self) -> Result<(), ServiceError> {
        let name = self
            .configuration_manager
            .configuration()
            .await?
            .server_name
            .clone();
        let published = if name.trim().is_empty() {
            None
        } else {
            Some(name)
        };
        if let Ok(mut guard) = self.server_name.write() {
            *guard = published;
        }
        Ok(())
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
        if hostname.to_ascii_lowercase().starts_with("http") {
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
            self.network.http_port
        });

        // Omit the port when it is the scheme's default (80/443), matching the
        // `UriBuilder` behavior of not rendering a default port.
        let default_port = if is_https { 443 } else { 80 };
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
        self.network.http_port
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
        // Published server URL always wins (C# GetSmartApiUrl short-circuit).
        if let Some(published) = &self.network.published_server_url
            && !published.is_empty()
        {
            return Ok(published.trim_matches('/').to_owned());
        }

        if self.network.enable_published_server_uri_by_request
            && let Some((host, req_port)) = Self::parse_request_host(request)
        {
            let scheme = self.request_scheme(request);
            // A default port for the scheme collapses to "no explicit port".
            let port = req_port.filter(|&p| {
                !((p == 80 && scheme.eq_ignore_ascii_case("http"))
                    || (p == 443 && scheme.eq_ignore_ascii_case("https")))
            });
            return Ok(self.build_local_api_url(&host, Some(&scheme), port));
        }

        // Fall back to the loopback local URL.
        Ok(self.build_local_api_url("localhost", None, None))
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

/// Case-insensitive `String.Replace` of every occurrence of `from` with `to`.
///
/// Mirrors C# `string.Replace(old, new, StringComparison.OrdinalIgnoreCase)`.
/// An empty `from` is a no-op (avoids an infinite loop).
fn replace_ignore_ascii_case(haystack: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return haystack.to_owned();
    }
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_from = from.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(rel) = lower_hay[cursor..].find(&lower_from) {
        let start = cursor + rel;
        out.push_str(&haystack[cursor..start]);
        out.push_str(to);
        cursor = start + from.len();
    }
    out.push_str(&haystack[cursor..]);
    out
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
}
