//! Port of `MediaBrowser.Common.Net.NetworkConfiguration` and its store key.
//!
//! This is the settings DTO the network manager reads. `BaseUrl` carries the
//! leading/trailing slash normalization from the C# property setter.
//!
//! The serialized names are the contract's, and several of them are pinned
//! with an explicit `rename` because `PascalCase` over `snake_case` does not
//! reproduce Jellyfin's capitalization (`EnableIPv4`, not `EnableIpv4`).
//! `served_field_names_match_the_vendored_contract` in
//! `tests/network_configuration_tests.rs` holds the whole set to the spec, so
//! adding a field means adding it there too — a name jellyfin-web does not
//! recognise is a setting that silently never arrives. Each corrected name
//! also carries a serde `alias` for the spelling Ferrofin used to write, or an
//! existing `network.json` would stop parsing.
//!
//! ## What actually acts on these settings
//!
//! Almost nothing, yet. This document is persisted and served so the dashboard
//! round-trips, and it is consumed by [`crate::manager::NetworkManager`] — the
//! ported LAN / remote-access / bind resolver — which **the server does not
//! construct** (`crates/ferrofin-api/src/auth.rs` explains why: its
//! `Rc<dyn Logger>` is not `Send`). Request handling uses a fixed RFC1918
//! check instead, so `LocalNetworkSubnets`, `KnownProxies`, `RemoteIPFilter`
//! and `IsRemoteIPFilterBlacklist` are all settable and unenforced today.
//! Wiring the manager into `AppState` is open work, and it is what makes these
//! real — do not read a field's presence here as evidence it is applied.
//!
//! `EnableUPnP` is a different case, and not a Ferrofin gap: upstream marks it
//! `[Obsolete("No longer supported")]`
//! (`MediaBrowser.Common/Net/NetworkConfiguration.cs:113`) and reads it nowhere
//! outside its own pre-startup migration DTOs. Jellyfin 10.11 does no UPnP port
//! forwarding either. The field is vestigial in both, carried because the
//! contract and the dashboard expect the key; ignoring it is exact parity, so
//! there is nothing here to warn about.

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
// `default` at the container: this document is only ever written by Ferrofin
// itself, and the one caller that reads it typed
// (`POST /Startup/RemoteAccess`) writes the WHOLE struct back afterwards. So a
// field this version does not find must fall back to its default, never fail
// the parse — a failed parse there is silently replaced by defaults and
// persisted, which would wipe `RemoteIPFilter` and every other setting. That is
// how adding `EnableUPnP` alone would have broken every existing install.
#[serde(rename_all = "PascalCase", default)]
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

    /// Whether to open the public ports with UPnP.
    ///
    /// Vestigial in both implementations — upstream marks it
    /// `[Obsolete("No longer supported")]` and never reads it. Carried so the
    /// served key set matches the contract; nothing acts on it here either.
    #[serde(rename = "EnableUPnP")]
    pub enable_upnp: bool,

    /// Whether IPv4 is enabled.
    ///
    /// `PascalCase` would give `EnableIpv4`; the contract and the C#
    /// `MediaBrowser.Common/Net/NetworkConfiguration.cs` both spell it
    /// `EnableIPv4`, so the name is pinned explicitly. Same for the three
    /// below.
    ///
    /// The `alias` reads a document Ferrofin wrote under the old, wrong name.
    /// Without it the parse fails and `POST /Startup/RemoteAccess` — which
    /// discards the error and writes the whole struct back — would reset every
    /// setting on the first upgraded boot.
    #[serde(rename = "EnableIPv4", alias = "EnableIpv4")]
    pub enable_ipv4: bool,

    /// Whether IPv6 is enabled.
    #[serde(rename = "EnableIPv6", alias = "EnableIpv6")]
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
    ///
    /// Persisted and served, but **not yet enforced**: the code that applies it
    /// lives in [`crate::manager::NetworkManager`], which the server does not
    /// construct — request handling falls back to a fixed RFC1918 check. Wiring
    /// the manager in is open work; until then this is a setting the dashboard
    /// can save and the server does not act on.
    #[serde(rename = "RemoteIPFilter", alias = "RemoteIpFilter")]
    pub remote_ip_filter: Vec<String>,

    /// Whether [`Self::remote_ip_filter`] is a blacklist (default is allowlist).
    #[serde(
        rename = "IsRemoteIPFilterBlacklist",
        alias = "IsRemoteIpFilterBlacklist"
    )]
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
            // C# `EnableUPnP` has no initializer, so a fresh instance is false.
            enable_upnp: false,
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
