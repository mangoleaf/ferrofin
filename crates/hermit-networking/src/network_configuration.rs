//! Port of `MediaBrowser.Common.Net.NetworkConfiguration` and its store key.
//!
//! This is the settings DTO the network manager reads. Deprecated `EnableUPnP`
//! is dropped per the port charter. `BaseUrl` carries the leading/trailing
//! slash normalization from the C# property setter.

use serde::{Deserialize, Serialize};

/// Configuration store key for the network configuration
/// (`NetworkConfigurationStore.StoreKey`).
pub const STORE_KEY: &str = "network";

/// The default value for the internal/public HTTP port.
pub const DEFAULT_HTTP_PORT: u16 = 8096;

/// The default value for the internal/public HTTPS port.
pub const DEFAULT_HTTPS_PORT: u16 = 8920;

/// Network-related server settings — port of `NetworkConfiguration`.
///
/// Field names use `snake_case`; the serde representation is `PascalCase` to
/// match the on-disk / OpenAPI contract Jellyfin uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
// The DTO faithfully mirrors the C# `NetworkConfiguration`, which carries this
// many independent boolean toggles; collapsing them into enums would diverge
// from the on-disk contract for no behavioral gain.
#[allow(clippy::struct_excessive_bools)]
pub struct NetworkConfiguration {
    /// URL prefix that the instance can be accessed at (normalized: leading
    /// slash, no trailing slash). The normalization is applied by
    /// [`NetworkConfiguration::set_base_url`]; assign through it rather than
    /// writing the field directly to preserve the invariant.
    #[serde(rename = "BaseUrl")]
    pub base_url: String,

    /// Whether to use HTTPS.
    pub enable_https: bool,

    /// Whether the server should force connections over HTTPS.
    pub require_https: bool,

    /// Filesystem path of an X.509 certificate to use for SSL.
    pub certificate_path: String,

    /// Password required to access the X.509 certificate data.
    pub certificate_password: String,

    /// The internal HTTP server port.
    pub internal_http_port: u16,

    /// The internal HTTPS server port.
    pub internal_https_port: u16,

    /// The public HTTP port.
    pub public_http_port: u16,

    /// The public HTTPS port.
    pub public_https_port: u16,

    /// Whether auto-discovery is enabled.
    pub auto_discovery: bool,

    /// Whether IPv4 is enabled.
    pub enable_ipv4: bool,

    /// Whether IPv6 is enabled.
    pub enable_ipv6: bool,

    /// Whether access from outside the LAN is permitted.
    pub enable_remote_access: bool,

    /// The subnets that are deemed to make up the LAN.
    pub local_network_subnets: Vec<String>,

    /// The interface addresses which the server will bind to. Empty = all.
    pub local_network_addresses: Vec<String>,

    /// The known proxies.
    pub known_proxies: Vec<String>,

    /// Whether interface names matching [`Self::virtual_interface_names`] are
    /// ignored for binding.
    pub ignore_virtual_interfaces: bool,

    /// Interface-name prefixes that should be ignored (case-insensitive).
    pub virtual_interface_names: Vec<String>,

    /// Whether the published server URI is derived from HTTP request info.
    pub enable_published_server_uri_by_request: bool,

    /// Published server URIs to advertise for specific subnets.
    pub published_server_uri_by_subnet: Vec<String>,

    /// Filter for remote IP connectivity (see
    /// [`Self::is_remote_ip_filter_blacklist`]).
    pub remote_ip_filter: Vec<String>,

    /// Whether [`Self::remote_ip_filter`] is a blacklist (default is allowlist).
    pub is_remote_ip_filter_blacklist: bool,
}

impl Default for NetworkConfiguration {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            enable_https: false,
            require_https: false,
            certificate_path: String::new(),
            certificate_password: String::new(),
            internal_http_port: DEFAULT_HTTP_PORT,
            internal_https_port: DEFAULT_HTTPS_PORT,
            public_http_port: DEFAULT_HTTP_PORT,
            public_https_port: DEFAULT_HTTPS_PORT,
            auto_discovery: true,
            enable_ipv4: true,
            enable_ipv6: false,
            enable_remote_access: true,
            local_network_subnets: Vec::new(),
            local_network_addresses: Vec::new(),
            known_proxies: Vec::new(),
            ignore_virtual_interfaces: true,
            virtual_interface_names: vec!["veth".to_owned()],
            enable_published_server_uri_by_request: false,
            published_server_uri_by_subnet: Vec::new(),
            remote_ip_filter: Vec::new(),
            is_remote_ip_filter_blacklist: false,
        }
    }
}

impl NetworkConfiguration {
    /// The normalized base URL prefix (mirrors the C# `BaseUrl` getter).
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sets the base URL, applying the C# `BaseUrl` setter normalization:
    /// an empty/whitespace value becomes empty; otherwise a leading `/` is
    /// ensured and any trailing `/` removed.
    pub fn set_base_url(&mut self, value: impl AsRef<str>) {
        let value = value.as_ref();
        if value.trim().is_empty() {
            self.base_url = String::new();
            return;
        }

        let mut normalized = value.to_owned();
        if !normalized.starts_with('/') {
            normalized.insert(0, '/');
        }

        if normalized.ends_with('/') {
            normalized.pop();
        }

        self.base_url = normalized;
    }

    /// Builder-style [`Self::set_base_url`].
    #[must_use]
    pub fn with_base_url(mut self, value: impl AsRef<str>) -> Self {
        self.set_base_url(value);
        self
    }
}
