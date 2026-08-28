//! Port of `Jellyfin.Networking.Manager.NetworkManager` — the bind-address /
//! published-URL resolver.
//!
//! The deterministic settings pipeline (`UpdateSettings` → `InitializeLan` /
//! `InitializeRemote` / `InitializeOverrides` / `EnforceBindSettings`) and the
//! read-side queries (`get_bind_address`, `is_in_local_network`,
//! `should_allow_server_access`, `get_internal_bind_addresses`,
//! `get_all_bind_interfaces`, `get_loopbacks`, `try_parse_interface`,
//! `is_link_local_address`) are ported. Deferred (per the port charter): OS
//! `NetworkChange` event wiring, the `Thread.Sleep(2000)` debounce, `Dispose`,
//! live `GetInterfacesCore` enumeration, and the `HttpRequest` overload.
//!
//! Live interface enumeration is deferred: when no mock interface string is
//! supplied the interface list starts empty (callers on a real host will inject
//! interfaces once that adapter lands).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use ferrofin_model::net::{AddressFamily, IpData, IpNetwork, PublishedServerUriOverride};

use crate::config_keys;
use crate::logger::{Logger, NullLogger};
use crate::net_constants;
use crate::net_utils;
use crate::network_configuration::NetworkConfiguration;
use crate::remote_access_policy_result::RemoteAccessPolicyResult;

/// The `IPAddress.Loopback` sentinel (`127.0.0.1`).
const IPV4_LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// The `IPAddress.IPv6Loopback` sentinel (`::1`).
const IPV6_LOOPBACK: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
/// The `IPAddress.Any` sentinel (`0.0.0.0`).
const IPV4_ANY: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
/// The `IPAddress.IPv6Any` sentinel (`::`).
const IPV6_ANY: IpAddr = IpAddr::V6(Ipv6Addr::UNSPECIFIED);

/// The startup configuration bag (`Microsoft.Extensions.Configuration.IConfiguration`).
///
/// Only two keys are consumed (see [`crate::config_keys`]); a plain string map
/// is sufficient and mirrors the `IConfiguration[key]` indexer semantics
/// (missing key → `None`).
#[derive(Debug, Clone, Default)]
pub struct StartupConfig {
    values: HashMap<String, String>,
}

impl StartupConfig {
    /// Creates an empty startup configuration (equivalent to the test mocks).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key` to `value`, builder-style.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Returns the value for `key`, or `None` if unset (the `IConfiguration`
    /// indexer returns `null`).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

/// Network interface manager — port of `NetworkManager` (`INetworkManager`).
pub struct NetworkManager {
    logger: Arc<dyn Logger>,
    config: NetworkConfiguration,
    startup_config: StartupConfig,
    published_server_urls: Vec<PublishedServerUriOverride>,
    remote_address_filter: Vec<IpNetwork>,
    known_proxies: Vec<IpNetwork>,
    interfaces: Vec<IpData>,
    lan_subnets: Vec<IpNetwork>,
    excluded_subnets: Vec<IpNetwork>,
    trust_all_ipv6_interfaces: bool,
    /// Test seam mirroring the C# `MockNetworkSettings` static; empty means
    /// "use live interfaces" (deferred → empty list).
    mock_network_settings: String,
}

impl NetworkManager {
    /// Creates a new manager and runs the initial settings pipeline.
    ///
    /// `mock_network_settings` mirrors the C# `MockNetworkSettings` static: a
    /// `<IPAddress>,<Index>,<Name>` triple per interface, interfaces separated
    /// by `|`. Empty selects live enumeration (currently deferred → no
    /// interfaces).
    #[must_use]
    pub fn new(
        config: NetworkConfiguration,
        startup_config: StartupConfig,
        mock_network_settings: impl Into<String>,
        logger: Arc<dyn Logger>,
    ) -> Self {
        let mut manager = Self {
            logger,
            config,
            startup_config,
            published_server_urls: Vec::new(),
            remote_address_filter: Vec::new(),
            known_proxies: Vec::new(),
            interfaces: Vec::new(),
            lan_subnets: Vec::new(),
            excluded_subnets: Vec::new(),
            trust_all_ipv6_interfaces: false,
            mock_network_settings: mock_network_settings.into(),
        };

        let config = manager.config.clone();
        manager.update_settings(&config);
        manager
    }

    /// Convenience constructor with a [`NullLogger`] and empty startup config.
    #[must_use]
    pub fn with_defaults(
        config: NetworkConfiguration,
        mock_network_settings: impl Into<String>,
    ) -> Self {
        Self::new(
            config,
            StartupConfig::new(),
            mock_network_settings,
            Arc::new(NullLogger),
        )
    }

    /// Whether IPv4 is enabled (`IsIPv4Enabled`).
    #[must_use]
    pub fn is_ipv4_enabled(&self) -> bool {
        self.config.enable_ipv4
    }

    /// Whether IPv6 is enabled (`IsIPv6Enabled`).
    #[must_use]
    pub fn is_ipv6_enabled(&self) -> bool {
        self.config.enable_ipv6
    }

    /// Whether all IPv6 interfaces are trusted as internal
    /// (`TrustAllIPv6Interfaces`).
    #[must_use]
    pub fn trust_all_ipv6_interfaces(&self) -> bool {
        self.trust_all_ipv6_interfaces
    }

    /// The published-server override list (`PublishedServerUrls`).
    #[must_use]
    pub fn published_server_urls(&self) -> &[PublishedServerUriOverride] {
        &self.published_server_urls
    }

    /// Reloads all settings and re-initializes the manager
    /// (`NetworkManager.UpdateSettings`).
    pub fn update_settings(&mut self, config: &NetworkConfiguration) {
        self.config = config.clone();

        self.initialize_lan(config);
        self.initialize_remote(config);

        if self.mock_network_settings.is_empty() {
            // Live enumeration deferred: start from no interfaces.
            self.interfaces = Vec::new();
        } else {
            // Format is <IPAddress>,<Index>,<Name>: <next interface>.
            let mut interfaces = Vec::new();
            for details in self.mock_network_settings.split('|') {
                let parts: Vec<&str> = details.split(',').collect();
                if let Some(mut data) = net_utils::try_parse_to_subnet(parts[0], false) {
                    if let Some(index) = parts.get(1).and_then(|p| p.parse::<i32>().ok()) {
                        data.index = index;
                        let family = data.address_family();
                        if (family == AddressFamily::InterNetwork
                            || family == AddressFamily::InterNetworkV6)
                            && let Some(name) = parts.get(2)
                        {
                            (*name).clone_into(&mut data.name);
                            interfaces.push(data);
                        }
                    }
                } else {
                    self.logger.warn(&format!(
                        "Could not parse mock interface settings: {details}"
                    ));
                }
            }

            self.interfaces = interfaces;
        }

        self.initialize_known_proxies(config);
        self.enforce_bind_settings(config);
        self.initialize_overrides(config);
    }

    /// Initializes the internal LAN cache (`InitializeLan`).
    fn initialize_lan(&mut self, config: &NetworkConfiguration) {
        let subnets = &config.local_network_subnets;

        let lan_subnets =
            net_utils::try_parse_to_subnets(subnets, false, Some(self.logger.as_ref()));

        match lan_subnets {
            Some(ref parsed) if !parsed.is_empty() => {
                self.lan_subnets = parsed.iter().map(|x| x.subnet).collect();
            }
            _ => {
                // No LAN addresses specified: all private subnets + loopback.
                let mut fallback = Vec::new();
                if self.is_ipv6_enabled() {
                    fallback.push(net_constants::ipv6_rfc4291_loopback());
                    fallback.push(net_constants::ipv6_rfc4291_site_local());
                    fallback.push(net_constants::ipv6_rfc4193_unique_local());
                }
                if self.is_ipv4_enabled() {
                    fallback.push(net_constants::ipv4_rfc5735_loopback());
                    fallback.push(net_constants::ipv4_rfc1918_private_class_a());
                    fallback.push(net_constants::ipv4_rfc1918_private_class_b());
                    fallback.push(net_constants::ipv4_rfc1918_private_class_c());
                }
                self.lan_subnets = fallback;
            }
        }

        self.excluded_subnets =
            match net_utils::try_parse_to_subnets(subnets, true, Some(self.logger.as_ref())) {
                Some(parsed) => parsed.iter().map(|x| x.subnet).collect(),
                None => Vec::new(),
            };
    }

    /// Enforce bind addresses and exclusions on the interfaces
    /// (`EnforceBindSettings`).
    fn enforce_bind_settings(&mut self, config: &NetworkConfiguration) {
        let interfaces = std::mem::take(&mut self.interfaces);
        self.interfaces = Self::filter_bind_settings(
            config,
            interfaces,
            self.is_ipv4_enabled(),
            self.is_ipv6_enabled(),
        );
    }

    /// Filters bind addresses and exclusions on available interfaces
    /// (`FilterBindSettings`).
    #[must_use]
    fn filter_bind_settings(
        config: &NetworkConfiguration,
        mut interfaces: Vec<IpData>,
        is_ipv4_enabled: bool,
        is_ipv6_enabled: bool,
    ) -> Vec<IpData> {
        let local_network_addresses = &config.local_network_addresses;
        if !local_network_addresses.is_empty() && !local_network_addresses[0].trim().is_empty() {
            let mut bind_addresses: Vec<IpAddr> = local_network_addresses
                .iter()
                .map(|p| {
                    if let Some(network) = net_utils::try_parse_to_subnet(p, false) {
                        network.address
                    } else {
                        interfaces
                            .iter()
                            .find(|x| x.name.eq_ignore_ascii_case(p))
                            .map_or_else(net_utils::ip_none, |x| x.address)
                    }
                })
                .filter(|x| *x != net_utils::ip_none())
                .collect();
            dedup(&mut bind_addresses);

            interfaces.retain(|x| bind_addresses.contains(&x.address));

            if bind_addresses.contains(&IPV4_LOOPBACK)
                && !interfaces.iter().any(|i| i.address == IPV4_LOOPBACK)
            {
                interfaces.push(IpData::new(
                    IPV4_LOOPBACK,
                    Some(net_constants::ipv4_rfc5735_loopback()),
                    "lo",
                ));
            }

            if bind_addresses.contains(&IPV6_LOOPBACK)
                && !interfaces.iter().any(|i| i.address == IPV6_LOOPBACK)
            {
                interfaces.push(IpData::new(
                    IPV6_LOOPBACK,
                    Some(net_constants::ipv6_rfc4291_loopback()),
                    "lo",
                ));
            }
        }

        // Remove interfaces matching a virtual-machine interface prefix.
        if config.ignore_virtual_interfaces {
            let virtual_prefixes: Vec<String> = config
                .virtual_interface_names
                .iter()
                .map(|i| i.replace('*', ""))
                .collect();

            if !interfaces.is_empty() {
                for prefix in &virtual_prefixes {
                    interfaces.retain(|x| {
                        !x.name
                            .to_ascii_lowercase()
                            .starts_with(&prefix.to_ascii_lowercase())
                    });
                }
            }
        }

        if !is_ipv4_enabled {
            interfaces.retain(|x| x.address_family() != AddressFamily::InterNetwork);
        }

        if !is_ipv6_enabled {
            interfaces.retain(|x| x.address_family() != AddressFamily::InterNetworkV6);
        }

        // Only return one IP per address for binding; let the OS handle the rest.
        distinct_by_address(interfaces)
    }

    /// Parses `KnownProxies` into the networks a forwarded header may be
    /// trusted from — C# `AddProxyAddresses`, which turns a bare address into a
    /// single-host subnet (`/32`, `/128`) and takes a CIDR as written.
    fn initialize_known_proxies(&mut self, config: &NetworkConfiguration) {
        let mut parsed: Vec<IpNetwork> = Vec::new();
        let cidrs: Vec<String> = config
            .known_proxies
            .iter()
            .filter(|x| x.contains('/'))
            .cloned()
            .collect();
        if let Some(subnets) = net_utils::try_parse_to_subnets(&cidrs, false, None) {
            parsed.extend(subnets.iter().map(|x| x.subnet));
        }
        for proxy in config.known_proxies.iter().filter(|x| !x.contains('/')) {
            if let Ok(ip) = proxy.trim().parse::<IpAddr>() {
                let prefix = match ip {
                    IpAddr::V4(_) => net_constants::MINIMUM_IPV4_PREFIX_SIZE,
                    IpAddr::V6(_) => net_constants::MINIMUM_IPV6_PREFIX_SIZE,
                };
                parsed.push(IpNetwork::new(ip, prefix));
            }
        }
        self.known_proxies = parsed;
    }

    /// Whether `address` is one of the configured `KnownProxies`.
    #[must_use]
    pub fn is_known_proxy(&self, address: IpAddr) -> bool {
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
            IpAddr::V4(_) => address,
        };
        self.known_proxies
            .iter()
            .any(|network| net_utils::subnet_contains_address(*network, address))
    }

    /// The address of the client that actually made the request, given the
    /// transport `peer` and the `X-Forwarded-For` chain (left to right, as
    /// sent).
    ///
    /// Port of what ASP.NET's `ForwardedHeadersMiddleware` does with the options
    /// C# `ConfigureForwardHeaders` builds:
    ///
    /// - with NO `KnownProxies` configured the header is ignored completely
    ///   (`ForwardedHeaders.None`) — which is the only safe default, since any
    ///   client can send one;
    /// - otherwise the chain is walked from the RIGHT, and a hop is taken only
    ///   while the address currently being trusted is a known proxy. The walk
    ///   stops at the first address that is not, so a client cannot forge its
    ///   way past the proxy by prepending entries of its own.
    ///
    /// `ForwardLimit` is null upstream once any proxy is configured, so the
    /// whole chain is walkable.
    #[must_use]
    pub fn client_address(&self, peer: IpAddr, forwarded_for: &[IpAddr]) -> IpAddr {
        if self.known_proxies.is_empty() {
            return peer;
        }
        let mut current = peer;
        let mut remaining = forwarded_for.len();
        while remaining > 0 && self.is_known_proxy(current) {
            remaining -= 1;
            current = forwarded_for[remaining];
        }
        current
    }

    /// Initializes the remote address filter (`InitializeRemote`).
    fn initialize_remote(&mut self, config: &NetworkConfiguration) {
        let remote_ip_filter = &config.remote_ip_filter;
        if remote_ip_filter.is_empty() || remote_ip_filter[0].trim().is_empty() {
            return;
        }

        let mut remote_address_filter: Vec<IpNetwork> = Vec::new();

        // Parse all IPs with netmask to a subnet.
        let remote_filtered_subnets: Vec<String> = remote_ip_filter
            .iter()
            .filter(|x| x.contains('/'))
            .cloned()
            .collect();
        if let Some(parsed) = net_utils::try_parse_to_subnets(&remote_filtered_subnets, false, None)
        {
            remote_address_filter = parsed.iter().map(|x| x.subnet).collect();
        }

        // Everything else is a single-IP subnet.
        for ip in remote_ip_filter.iter().filter(|x| !x.contains('/')) {
            if let Ok(ipp) = ip.parse::<IpAddr>() {
                let prefix = match ipp {
                    IpAddr::V4(_) => net_constants::MINIMUM_IPV4_PREFIX_SIZE,
                    IpAddr::V6(_) => net_constants::MINIMUM_IPV6_PREFIX_SIZE,
                };
                remote_address_filter.push(IpNetwork::new(ipp, prefix));
            }
        }

        self.remote_address_filter = remote_address_filter;
    }

    /// Parses the published-server URL overrides (`InitializeOverrides`).
    fn initialize_overrides(&mut self, config: &NetworkConfiguration) {
        let mut published_server_urls: Vec<PublishedServerUriOverride> = Vec::new();

        // Prefer startup configuration.
        if let Some(startup_override) = self.startup_config.get(config_keys::ADDRESS_OVERRIDE_KEY)
            && !startup_override.is_empty()
        {
            published_server_urls.push(PublishedServerUriOverride::new(
                IpData::new(IPV4_ANY, Some(net_constants::ipv4_any()), String::new()),
                startup_override,
                true,
                true,
            ));
            published_server_urls.push(PublishedServerUriOverride::new(
                IpData::new(IPV6_ANY, Some(net_constants::ipv6_any()), String::new()),
                startup_override,
                true,
                true,
            ));
            self.published_server_urls = published_server_urls;
            return;
        }

        for entry in &config.published_server_uri_by_subnet {
            let parts: Vec<&str> = entry.split('=').collect();
            if parts.len() != 2 {
                self.logger
                    .warn(&format!("Unable to parse bind override: {entry}"));
                return;
            }

            let replacement = parts[1].trim();
            let identifier = parts[0];
            if identifier.eq_ignore_ascii_case("all") {
                published_server_urls.clear();
                published_server_urls.push(PublishedServerUriOverride::new(
                    IpData::new(IPV4_ANY, Some(net_constants::ipv4_any()), String::new()),
                    replacement,
                    true,
                    true,
                ));
                published_server_urls.push(PublishedServerUriOverride::new(
                    IpData::new(IPV6_ANY, Some(net_constants::ipv6_any()), String::new()),
                    replacement,
                    true,
                    true,
                ));
                break;
            } else if identifier.eq_ignore_ascii_case("external") {
                published_server_urls.push(PublishedServerUriOverride::new(
                    IpData::new(IPV4_ANY, Some(net_constants::ipv4_any()), String::new()),
                    replacement,
                    false,
                    true,
                ));
                published_server_urls.push(PublishedServerUriOverride::new(
                    IpData::new(IPV6_ANY, Some(net_constants::ipv6_any()), String::new()),
                    replacement,
                    false,
                    true,
                ));
            } else if identifier.eq_ignore_ascii_case("internal") {
                for lan in &self.lan_subnets {
                    let lan_prefix = lan.base_address;
                    published_server_urls.push(PublishedServerUriOverride::new(
                        IpData::new(
                            lan_prefix,
                            Some(IpNetwork::new(lan_prefix, lan.prefix_length)),
                            String::new(),
                        ),
                        replacement,
                        true,
                        false,
                    ));
                }
            } else if let Some(result) = net_utils::try_parse_to_subnet(identifier, false) {
                published_server_urls.push(PublishedServerUriOverride::new(
                    result,
                    replacement,
                    true,
                    true,
                ));
            } else if let Some(ifaces) = self.try_parse_interface(identifier) {
                for iface in ifaces {
                    published_server_urls.push(PublishedServerUriOverride::new(
                        iface,
                        replacement,
                        true,
                        true,
                    ));
                }
            } else {
                self.logger
                    .warn(&format!("Unable to parse bind override: {entry}"));
            }
        }

        self.published_server_urls = published_server_urls;
    }

    /// Matches interfaces whose name equals `intf` (`TryParseInterface`).
    ///
    /// Returns `None` when nothing matches (mirrors the C# `NotNullWhen(true)`
    /// contract), otherwise the matches ordered by interface index.
    #[must_use]
    pub fn try_parse_interface(&self, intf: &str) -> Option<Vec<IpData>> {
        if intf.is_empty() || self.interfaces.is_empty() {
            return None;
        }

        let mut result: Vec<IpData> = self
            .interfaces
            .iter()
            .filter(|i| {
                i.name.eq_ignore_ascii_case(intf)
                    && ((self.is_ipv4_enabled()
                        && i.address_family() == AddressFamily::InterNetwork)
                        || (self.is_ipv6_enabled()
                            && i.address_family() == AddressFamily::InterNetworkV6))
            })
            .cloned()
            .collect();
        result.sort_by_key(|x| x.index);

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// Determines whether a remote request should be allowed
    /// (`ShouldAllowServerAccess`).
    #[must_use]
    pub fn should_allow_server_access(&self, remote_ip: IpAddr) -> RemoteAccessPolicyResult {
        if self.is_in_local_network(remote_ip) {
            return RemoteAccessPolicyResult::Allow;
        }

        if !self.config.enable_remote_access {
            return RemoteAccessPolicyResult::RejectDueToRemoteAccessDisabled;
        }

        if self.remote_address_filter.is_empty() {
            return RemoteAccessPolicyResult::Allow;
        }

        let any_matches = self
            .remote_address_filter
            .iter()
            .any(|network| net_utils::subnet_contains_address(*network, remote_ip));

        if self.config.is_remote_ip_filter_blacklist {
            if any_matches {
                RemoteAccessPolicyResult::RejectDueToIpBlocklist
            } else {
                RemoteAccessPolicyResult::Allow
            }
        } else if any_matches {
            RemoteAccessPolicyResult::Allow
        } else {
            RemoteAccessPolicyResult::RejectDueToNotAllowlistedRemoteIp
        }
    }

    /// The loopback interfaces for the enabled families (`GetLoopbacks`).
    #[must_use]
    pub fn get_loopbacks(&self) -> Vec<IpData> {
        let mut loopbacks = Vec::new();
        if !self.is_ipv4_enabled() && !self.is_ipv6_enabled() {
            return loopbacks;
        }

        if self.is_ipv4_enabled() {
            loopbacks.push(IpData::new(
                IPV4_LOOPBACK,
                Some(net_constants::ipv4_rfc5735_loopback()),
                "lo",
            ));
        }
        if self.is_ipv6_enabled() {
            loopbacks.push(IpData::new(
                IPV6_LOOPBACK,
                Some(net_constants::ipv6_rfc4291_loopback()),
                "lo",
            ));
        }

        loopbacks
    }

    /// Produces the list of interfaces the server should bind to
    /// (`GetAllBindInterfaces`).
    #[must_use]
    pub fn get_all_bind_interfaces(&self, individual_interfaces: bool) -> Vec<IpData> {
        let local_network_addresses = &self.config.local_network_addresses;
        let read_ipv4 = self.is_ipv4_enabled();
        let read_ipv6 = self.is_ipv6_enabled();

        if (!local_network_addresses.is_empty()
            && !local_network_addresses[0].trim().is_empty()
            && !self.interfaces.is_empty())
            || individual_interfaces
        {
            return self.interfaces.clone();
        }

        // No bind address and no exclusions, so listen on all interfaces.
        let mut result = Vec::new();
        if read_ipv4 && read_ipv6 {
            result.push(IpData::new(
                IPV6_ANY,
                Some(net_constants::ipv6_any()),
                String::new(),
            ));
        } else if read_ipv4 {
            result.push(IpData::new(
                IPV4_ANY,
                Some(net_constants::ipv4_any()),
                String::new(),
            ));
        } else if read_ipv6 {
            for iface in &self.interfaces {
                if iface.address_family() == AddressFamily::InterNetworkV6 {
                    result.push(iface.clone());
                }
            }
        }

        result
    }

    /// Resolves the bind address for a textual `source` (`GetBindAddress`).
    ///
    /// Returns the bind address string and, via the tuple, an optional port
    /// parsed out of a matching published-server override.
    #[must_use]
    pub fn get_bind_address(&self, source: &str) -> (String, Option<u16>) {
        let addresses =
            net_utils::try_parse_host(source, self.is_ipv4_enabled(), self.is_ipv6_enabled())
                .unwrap_or_default();
        let resolved = self.get_bind_address_for_ip(addresses.first().copied(), false);
        // Per-request published-URL resolution → debug (RULES_LOGGING volume rule);
        // the startup banner already logs the configured result at info. This
        // answers "why this address for this client" when a published URL looks off.
        tracing::debug!(source, address = %resolved.0, port = ?resolved.1, "resolved bind address");
        resolved
    }

    /// Resolves the bind address for an optional source IP
    /// (`GetBindAddress(IPAddress?, out int?, bool)`).
    #[must_use]
    pub fn get_bind_address_for_ip(
        &self,
        source: Option<IpAddr>,
        skip_overrides: bool,
    ) -> (String, Option<u16>) {
        if let Some(source) = source {
            let is_external = !self.is_in_local_network(source);

            if !skip_overrides
                && let Some((result, port)) = self.matches_published_server_url(source, is_external)
            {
                return (result, port);
            }

            if let Some(result) = self.matches_bind_interface(source, is_external) {
                return (result, None);
            }

            if is_external && let Some(result) = self.matches_external_interface(source) {
                return (result, None);
            }
        }

        // First LAN interface that isn't excluded and isn't loopback.
        let mut available: Vec<&IpData> = self
            .interfaces
            .iter()
            .filter(|x| !is_loopback(x.address))
            .collect();
        available.sort_by(|a, b| {
            // OrderByDescending(IsInLocalNetwork).ThenBy(Index)
            let a_local = self.is_in_local_network(a.address);
            let b_local = self.is_in_local_network(b.address);
            b_local.cmp(&a_local).then(a.index.cmp(&b.index))
        });

        if available.is_empty() {
            let result = if source
                .is_some_and(|s| AddressFamily::of(s) == AddressFamily::InterNetwork)
                && self.is_ipv4_enabled()
            {
                "127.0.0.1"
            } else if source.is_some_and(|s| AddressFamily::of(s) == AddressFamily::InterNetworkV6)
                && self.is_ipv6_enabled()
            {
                "::1"
            } else if self.is_ipv4_enabled() {
                "127.0.0.1"
            } else {
                "::1"
            };
            return (result.to_owned(), None);
        }

        let Some(source) = source else {
            // No source: use the preferred (first) interface.
            return (
                net_utils::format_ip_string(Some(available[0].address)),
                None,
            );
        };

        // Does the request originate in one of the interface subnets?
        for intf in &available {
            if net_utils::subnet_contains_address(intf.subnet, source) {
                return (net_utils::format_ip_string(Some(intf.address)), None);
            }
        }

        // Fallback to an interface matching the source's address family.
        if let Some(preferred) = available
            .iter()
            .find(|x| AddressFamily::of(x.address) == AddressFamily::of(source))
        {
            return (net_utils::format_ip_string(Some(preferred.address)), None);
        }

        (
            net_utils::format_ip_string(Some(available[0].address)),
            None,
        )
    }

    /// The local (in-LAN) bind interfaces ordered by index
    /// (`GetInternalBindAddresses`).
    #[must_use]
    pub fn get_internal_bind_addresses(&self) -> Vec<IpData> {
        let mut result: Vec<IpData> = self
            .interfaces
            .iter()
            .filter(|x| self.is_in_local_network(x.address))
            .cloned()
            .collect();
        result.sort_by_key(|x| x.index);
        result
    }

    /// Whether a textual address is in the local network
    /// (`IsInLocalNetwork(string)`).
    #[must_use]
    pub fn is_in_local_network_str(&self, address: &str) -> bool {
        if let Some(subnet) = net_utils::try_parse_to_subnet(address, false) {
            return self.is_in_local_network(subnet.address);
        }

        net_utils::try_parse_host(address, self.is_ipv4_enabled(), self.is_ipv6_enabled())
            .is_some_and(|addresses| addresses.iter().any(|a| self.is_in_local_network(*a)))
    }

    /// Whether an address is link-local (`IsLinkLocalAddress`).
    #[must_use]
    pub fn is_link_local_address(&self, address: IpAddr) -> bool {
        net_utils::subnet_contains_address(net_constants::ipv4_rfc3927_link_local(), address)
            || net_utils::is_ipv6_link_local(address)
    }

    /// Whether an address is in the local network (`IsInLocalNetwork(IPAddress)`).
    #[must_use]
    pub fn is_in_local_network(&self, address: IpAddr) -> bool {
        let address = unmap_v4_mapped(address);

        if (self.trust_all_ipv6_interfaces
            && AddressFamily::of(address) == AddressFamily::InterNetworkV6)
            || is_loopback(address)
        {
            return true;
        }

        self.check_if_lan_and_not_excluded(address)
    }

    /// Whether `address` is in a LAN subnet and not excluded
    /// (`CheckIfLanAndNotExcluded`).
    fn check_if_lan_and_not_excluded(&self, address: IpAddr) -> bool {
        for lan_subnet in &self.lan_subnets {
            if net_utils::subnet_contains_address(*lan_subnet, address) {
                for excluded_subnet in &self.excluded_subnets {
                    if net_utils::subnet_contains_address(*excluded_subnet, address) {
                        return false;
                    }
                }
                return true;
            }
        }
        false
    }

    /// Matches `source` against the published-server URL overrides
    /// (`MatchesPublishedServerUrl`). Returns `Some((uri, port))` on a match.
    fn matches_published_server_url(
        &self,
        source: IpAddr,
        is_in_external_subnet: bool,
    ) -> Option<(String, Option<u16>)> {
        let mut valid: Vec<&PublishedServerUriOverride> = self
            .published_server_urls
            .iter()
            .filter(|x| {
                let matches_side = if is_in_external_subnet {
                    x.is_external_override
                } else {
                    x.is_internal_override
                };
                matches_side && net_utils::subnet_contains_address(x.data.subnet, source)
            })
            .collect();
        // OrderByDescending(PrefixLength) — stable to preserve insertion order.
        valid.sort_by(|a, b| {
            b.data
                .subnet
                .prefix_length
                .cmp(&a.data.subnet.prefix_length)
        });

        let mut bind_preference = String::new();
        for data in &valid {
            let mut candidates: Vec<&IpData> = self.interfaces.iter().collect();
            candidates.sort_by_key(|x| x.index);
            let intf = candidates
                .into_iter()
                .find(|x| net_utils::subnet_contains_address(data.data.subnet, x.address));

            let family = data.data.address_family();
            if intf.is_some()
                || (family == AddressFamily::InterNetwork && data.data.address == IPV4_ANY)
                || (family == AddressFamily::InterNetworkV6 && data.data.address == IPV6_ANY)
            {
                bind_preference.clone_from(&data.override_uri);
                break;
            }
        }

        if bind_preference.is_empty() {
            return None;
        }

        // Handle override specifying a port.
        let parts: Vec<&str> = bind_preference.split(':').collect();
        if parts.len() > 1
            && let Ok(p) = parts[1].parse::<u16>()
        {
            return Some((parts[0].to_owned(), Some(p)));
        }

        Some((bind_preference, None))
    }

    /// Matches `source` against the user-defined bind interfaces
    /// (`MatchesBindInterface`).
    fn matches_bind_interface(
        &self,
        source: IpAddr,
        is_in_external_subnet: bool,
    ) -> Option<String> {
        let mut count = self.interfaces.len();
        if count == 1
            && (self.interfaces[0].address == IPV4_ANY || self.interfaces[0].address == IPV6_ANY)
        {
            count = 0;
        }
        if count == 0 {
            return None;
        }

        if is_in_external_subnet {
            let mut external: Vec<&IpData> = self
                .interfaces
                .iter()
                .filter(|x| !self.is_in_local_network(x.address))
                .filter(|x| !self.is_link_local_address(x.address))
                .collect();
            external.sort_by_key(|x| x.index);

            if !external.is_empty() {
                // OrderByDescending(subnet-contains).ThenByDescending(prefix).ThenBy(index)
                external.sort_by(|a, b| {
                    let a_c = net_utils::subnet_contains_address(a.subnet, source);
                    let b_c = net_utils::subnet_contains_address(b.subnet, source);
                    b_c.cmp(&a_c)
                        .then(b.subnet.prefix_length.cmp(&a.subnet.prefix_length))
                        .then(a.index.cmp(&b.index))
                });
                return Some(net_utils::format_ip_string(Some(external[0].address)));
            }
        } else {
            let mut internal: Vec<&IpData> = self
                .interfaces
                .iter()
                .filter(|x| self.is_in_local_network(x.address))
                .collect();
            if !internal.is_empty() {
                internal.sort_by(|a, b| {
                    let a_c = net_utils::subnet_contains_address(a.subnet, source);
                    let b_c = net_utils::subnet_contains_address(b.subnet, source);
                    b_c.cmp(&a_c)
                        .then(b.subnet.prefix_length.cmp(&a.subnet.prefix_length))
                        .then(a.index.cmp(&b.index))
                });
                return Some(net_utils::format_ip_string(Some(internal[0].address)));
            }
        }

        None
    }

    /// Matches `source` against external interfaces (`MatchesExternalInterface`).
    fn matches_external_interface(&self, source: IpAddr) -> Option<String> {
        let mut ext: Vec<&IpData> = self
            .interfaces
            .iter()
            .filter(|p| !self.is_in_local_network(p.address))
            .filter(|p| AddressFamily::of(p.address) == AddressFamily::of(source))
            .filter(|p| !self.is_link_local_address(p.address))
            .collect();
        ext.sort_by_key(|x| x.index);

        if ext.is_empty() {
            return None;
        }

        for intf in &ext {
            if net_utils::subnet_contains_address(intf.subnet, source) {
                return Some(net_utils::format_ip_string(Some(intf.address)));
            }
        }

        Some(net_utils::format_ip_string(Some(ext[0].address)))
    }
}

/// Whether `address` is a loopback address (`IPAddress.IsLoopback`).
fn is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Unwraps an IPv4-mapped IPv6 address to plain IPv4; others pass through.
fn unmap_v4_mapped(address: IpAddr) -> IpAddr {
    if let IpAddr::V6(v6) = address
        && let Some(v4) = v6.to_ipv4_mapped()
    {
        return IpAddr::V4(v4);
    }
    address
}

/// Deduplicates while preserving first-seen order (C# `ToHashSet` keeps the
/// set membership; order here only needs to be stable for `contains` checks).
fn dedup(addresses: &mut Vec<IpAddr>) {
    let mut seen = Vec::new();
    addresses.retain(|a| {
        if seen.contains(a) {
            false
        } else {
            seen.push(*a);
            true
        }
    });
}

/// Keeps the first interface for each distinct address (`DistinctBy(Address)`).
fn distinct_by_address(interfaces: Vec<IpData>) -> Vec<IpData> {
    let mut seen = Vec::new();
    let mut result = Vec::new();
    for iface in interfaces {
        if !seen.contains(&iface.address) {
            seen.push(iface.address);
            result.push(iface);
        }
    }
    result
}
