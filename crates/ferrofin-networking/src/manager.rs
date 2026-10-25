//! Port of `Jellyfin.Networking.Manager.NetworkManager` — the bind-address /
//! published-URL resolver.
//!
//! Live managers refresh operating-system interfaces at boot, configuration saves,
//! and peer URL resolution. Mock and provided snapshots remain deterministic.
//! TODO: refresh shared policy caches on OS network events (Linux rtnetlink and
//! corresponding Windows/macOS notifications), with debounce and parity tests.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use ferrofin_model::net::{AddressFamily, IpData, IpNetwork, PublishedServerUriOverride};

use crate::config_keys;
use crate::known_proxies::KnownProxies;
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

/// The published server overrides among `overrides` that are full URLs whose
/// public path does not end with `base_url`: the test inside
/// `NetworkManager.WarnIfPublishedUrlBasePathDiffers`. Returns each one's
/// `host:port` (the scheme's default port when it names none), once per
/// override ignoring case, never its user info, query or fragment. Bare host
/// or IP overrides are exempt, since the base URL is appended when the API URL
/// is built from them; an empty `base_url` leaves nothing to compare.
#[must_use]
pub fn published_urls_missing_base_url<'a>(
    overrides: impl IntoIterator<Item = &'a str>,
    base_url: &str,
) -> Vec<String> {
    let mut hosts = Vec::new();
    if base_url.is_empty() {
        return hosts;
    }
    let mut seen = std::collections::HashSet::new();
    for uri in overrides {
        let lowered = uri.to_ascii_lowercase();
        let (scheme_len, default_port) = if lowered.starts_with("http://") {
            ("http://".len(), 80)
        } else if lowered.starts_with("https://") {
            ("https://".len(), 443)
        } else {
            continue;
        };
        if !seen.insert(lowered) {
            continue;
        }
        let rest = uri[scheme_len..]
            .split(['?', '#'])
            .next()
            .unwrap_or_default();
        let (authority, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        // `Uri.TryCreate` rejects an override without a host.
        if host.is_empty() {
            continue;
        }
        let path = percent_encoding::percent_decode_str(path).decode_utf8_lossy();
        let path = path.trim_end_matches('/');
        if path
            .len()
            .checked_sub(base_url.len())
            .and_then(|at| path.get(at..))
            .is_some_and(|tail| tail.eq_ignore_ascii_case(base_url))
        {
            continue;
        }
        // `GetComponents(HostAndPort)`: the explicit port, else the default.
        let has_port = !host.ends_with(']')
            && host.rsplit_once(':').is_some_and(|(_, port)| {
                !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
            });
        hosts.push(if has_port {
            host.to_owned()
        } else {
            format!("{host}:{default_port}")
        });
    }
    hosts
}

/// Network interface manager — port of `NetworkManager` (`INetworkManager`).
pub struct NetworkManager {
    logger: Arc<dyn Logger>,
    config: NetworkConfiguration,
    startup_config: StartupConfig,
    published_server_urls: Vec<PublishedServerUriOverride>,
    remote_address_filter: Vec<IpNetwork>,
    known_proxies: KnownProxies,
    interfaces: Vec<IpData>,
    lan_subnets: Vec<IpNetwork>,
    excluded_subnets: Vec<IpNetwork>,
    trust_all_ipv6_interfaces: bool,
    interface_source: InterfaceSource,
    /// Host-name lookups (`Dns.GetHostAddresses`): the OS resolver unless
    /// [`NetworkManager::with_resolver`] replaced it.
    resolver: net_utils::HostResolver,
}

/// The address half of `IsInLocalNetwork(string)`, independent of the
/// manager it came from ([`NetworkManager::host_lookup`]).
#[derive(Clone, Copy)]
pub struct HostLookup {
    is_ipv4_enabled: bool,
    is_ipv6_enabled: bool,
    resolver: net_utils::HostResolver,
}

impl std::fmt::Debug for HostLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostLookup")
            .field("is_ipv4_enabled", &self.is_ipv4_enabled)
            .field("is_ipv6_enabled", &self.is_ipv6_enabled)
            .finish_non_exhaustive()
    }
}

impl HostLookup {
    /// The addresses `address` stands for: itself when it parses as an
    /// address or subnet (`TryParseToSubnet`), else what `TryParseHost`
    /// resolves, which can block on DNS. `None` when it is neither.
    #[must_use]
    pub fn addresses(&self, address: &str) -> Option<Vec<IpAddr>> {
        if let Some(subnet) = net_utils::try_parse_to_subnet(address, false) {
            return Some(vec![subnet.address]);
        }
        net_utils::try_parse_host_with(
            address,
            self.is_ipv4_enabled,
            self.is_ipv6_enabled,
            self.resolver,
        )
    }
}

enum InterfaceSource {
    Mock(String),
    Provided(Vec<IpData>),
    Live,
}

impl NetworkManager {
    /// Creates a new manager and runs the initial settings pipeline.
    ///
    /// `mock_network_settings` mirrors the C# `MockNetworkSettings` static: a
    /// `<IPAddress>,<Index>,<Name>` triple per interface, interfaces separated
    /// by `|`. Empty deliberately supplies no interfaces. Use [`Self::live`]
    /// for operating-system enumeration.
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
            known_proxies: KnownProxies::default(),
            interfaces: Vec::new(),
            lan_subnets: Vec::new(),
            excluded_subnets: Vec::new(),
            trust_all_ipv6_interfaces: false,
            interface_source: InterfaceSource::Mock(mock_network_settings.into()),
            resolver: net_utils::resolve_host,
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

    /// Constructs a manager that enumerates live OS interfaces.
    #[must_use]
    pub fn live(config: NetworkConfiguration) -> Self {
        let mut manager = Self::with_defaults(config, "");
        manager.interface_source = InterfaceSource::Live;
        manager.refresh_interfaces();
        manager
    }

    /// Constructs a manager with an explicit, deterministic interface snapshot.
    #[must_use]
    pub fn with_interfaces(config: NetworkConfiguration, interfaces: Vec<IpData>) -> Self {
        let mut manager = Self::with_defaults(config, "");
        manager.set_interfaces(interfaces);
        manager
    }

    /// Looks host names up with `resolver` instead of the operating system's
    /// resolver, builder-style, and parses `KnownProxies` again with it.
    ///
    /// Only the textual-source paths ([`Self::get_bind_address`],
    /// [`Self::is_in_local_network_str`]) and `KnownProxies` host names resolve
    /// names, so tests use this to keep them off live DNS. Every other
    /// constructor keeps the OS resolver, and has already looked up any
    /// `KnownProxies` host name in the configuration it was given with it:
    /// start from one without host names and pass them to
    /// [`Self::update_settings`] afterwards.
    #[must_use]
    pub fn with_resolver(mut self, resolver: net_utils::HostResolver) -> Self {
        self.resolver = resolver;
        let proxies = KnownProxies::resolve(&self.config, resolver);
        self.adopt_known_proxies(proxies);
        self
    }

    /// The host-name resolver, for a lookup made outside the manager's lock
    /// ([`KnownProxies::resolve`]).
    #[must_use]
    pub fn resolver(&self) -> net_utils::HostResolver {
        self.resolver
    }

    /// Replaces the supplied snapshot, including with an intentionally empty list.
    /// This switches a live manager to deterministic supplied interfaces.
    pub fn set_interfaces(&mut self, interfaces: Vec<IpData>) {
        self.interface_source = InterfaceSource::Provided(interfaces);
        let config = self.config.clone();
        self.update_settings(&config);
    }

    /// Refreshes live interfaces and dependent settings; injected snapshots stay fixed.
    pub fn refresh_interfaces(&mut self) {
        if matches!(self.interface_source, InterfaceSource::Live) {
            let config = self.config.clone();
            self.update_settings(&config);
        }
    }

    /// The network configuration the manager was last given
    /// (`GetNetworkConfiguration`).
    #[must_use]
    pub fn configuration(&self) -> &NetworkConfiguration {
        &self.config
    }

    /// Whether discovery should start for the next server lifetime.
    #[must_use]
    pub fn auto_discovery(&self) -> bool {
        self.config.auto_discovery
    }

    /// Whether smart URLs echo the request's own host
    /// (`EnablePublishedServerUriByRequest`).
    #[must_use]
    pub fn enable_published_server_uri_by_request(&self) -> bool {
        self.config.enable_published_server_uri_by_request
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

    /// [`Self::update_settings`] with `KnownProxies` already parsed (outside
    /// the manager's lock, as host names can block on DNS). Proxies parsed from
    /// another configuration are ignored, and `config`'s are parsed here.
    pub fn update_settings_with_known_proxies(
        &mut self,
        config: &NetworkConfiguration,
        proxies: KnownProxies,
    ) {
        if proxies.is_parsed_from(config) {
            self.adopt_known_proxies(proxies);
        }
        self.update_settings(config);
    }

    /// Reloads all settings and re-initializes the manager
    /// (`NetworkManager.UpdateSettings`).
    ///
    /// `KnownProxies` is parsed again only when its entries or the address
    /// families changed, so an interface refresh resolves no host name.
    pub fn update_settings(&mut self, config: &NetworkConfiguration) {
        self.config = config.clone();

        self.initialize_lan(config);
        self.initialize_remote(config);

        self.interfaces = match &self.interface_source {
            InterfaceSource::Live => {
                crate::interfaces::enumerate(config.enable_ipv4, config.enable_ipv6)
            }
            InterfaceSource::Provided(interfaces) => interfaces.clone(),
            InterfaceSource::Mock(settings) if settings.is_empty() => Vec::new(),
            InterfaceSource::Mock(settings) => {
                // Format is <IPAddress>,<Index>,<Name>: <next interface>.
                let mut interfaces = Vec::new();
                for details in settings.split('|') {
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

                interfaces
            }
        };

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
    /// trusted from (C# `AddProxyAddresses`, [`KnownProxies::resolve`]),
    /// unless it was already parsed from these entries and address families.
    fn initialize_known_proxies(&mut self, config: &NetworkConfiguration) {
        if !self.known_proxies.is_parsed_from(config) {
            let proxies = KnownProxies::resolve(config, self.resolver);
            self.adopt_known_proxies(proxies);
        }
    }

    /// Takes `proxies` as the known proxies, warning once for each entry that
    /// parsed to nothing: upstream drops it silently, which leaves an operator
    /// no way to tell a mistyped proxy from a working one.
    fn adopt_known_proxies(&mut self, proxies: KnownProxies) {
        for entry in proxies.unresolved() {
            tracing::warn!(
                entry = ?entry,
                "KnownProxies entry is not an address, a subnet or a resolvable host name; ignored"
            );
        }
        self.known_proxies = proxies;
    }

    /// The networks `KnownProxies` was parsed into, in entry order: what
    /// upstream's `AddProxyAddresses` files as
    /// `ForwardedHeadersOptions.KnownProxies` (single hosts) and
    /// `KnownIPNetworks` (subnets). ASP.NET's loopback defaults, which
    /// [`Self::is_known_proxy`] also trusts, are not listed.
    #[must_use]
    pub fn known_proxies(&self) -> &[IpNetwork] {
        self.known_proxies.networks()
    }

    /// Whether `address` may vouch for the client behind it
    /// ([`KnownProxies::contains`]).
    #[must_use]
    pub fn is_known_proxy(&self, address: IpAddr) -> bool {
        self.known_proxies.contains(address)
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
    ///   client can send one; a configured list whose entries all fail to parse
    ///   still enables it, trusting only ASP.NET's loopback defaults;
    /// - otherwise the chain is walked from the RIGHT, and a hop is taken only
    ///   while the address currently being trusted is a known proxy. The walk
    ///   stops at the first address that is not, so a client cannot forge its
    ///   way past the proxy by prepending entries of its own.
    ///
    /// `ForwardLimit` is null upstream once any proxy is configured, so the
    /// whole chain is walkable. `ApplyForwarders` also stops at the first entry
    /// from the right that does not parse; the caller passes the chain already
    /// ended there (`ferrofin-api`'s `forwarded_for`).
    #[must_use]
    pub fn client_address(&self, peer: IpAddr, forwarded_for: &[IpAddr]) -> IpAddr {
        if !self.known_proxies.forwarding_enabled() {
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
        let addresses = net_utils::try_parse_host_with(
            source,
            self.is_ipv4_enabled(),
            self.is_ipv6_enabled(),
            self.resolver,
        )
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

    /// Resolves an advertised address while respecting the actual HTTP listener.
    /// Published URL overrides take precedence over an explicit local bind. A
    /// specific bind is the sole reachable candidate; it never trims policy state.
    #[must_use]
    pub fn get_bind_address_for_peer(
        &self,
        peer: IpAddr,
        http_bind: Option<IpAddr>,
    ) -> (String, Option<u16>) {
        if let Some(result) =
            self.matches_published_server_url(peer, !self.is_in_local_network(peer))
        {
            return result;
        }
        if let Some(bind) = http_bind.filter(|ip| !ip.is_unspecified()) {
            if !bind.is_loopback() && !self.interfaces.iter().any(|iface| iface.address == bind) {
                tracing::debug!(address = %bind, "HTTP bind address is absent from available interfaces; advertising the bound listener");
            }
            return (net_utils::format_ip_string(Some(bind)), None);
        }
        self.get_bind_address_for_ip(Some(peer), true)
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
    ///
    /// A host name is resolved, which can block on DNS. A caller holding the
    /// manager behind a lock can resolve outside it: take
    /// [`Self::host_lookup`], resolve, and test each address with
    /// [`Self::is_in_local_network`].
    #[must_use]
    pub fn is_in_local_network_str(&self, address: &str) -> bool {
        self.host_lookup()
            .addresses(address)
            .is_some_and(|addresses| addresses.iter().any(|a| self.is_in_local_network(*a)))
    }

    /// How [`Self::is_in_local_network_str`] reads a textual address into the
    /// addresses it tests: the address-family switches and the resolver,
    /// copied out so the lookup needs no access to the manager.
    #[must_use]
    pub fn host_lookup(&self) -> HostLookup {
        HostLookup {
            is_ipv4_enabled: self.is_ipv4_enabled(),
            is_ipv6_enabled: self.is_ipv6_enabled(),
            resolver: self.resolver,
        }
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
        let address = net_utils::unmap_v4_mapped(address);

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

        Some(parse_host_and_port(bind_preference))
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

/// `NetworkManager.ParseHostAndPort` (v12.2): a published-server override's
/// host and explicit port. A full URL (`://`) is returned whole, its port
/// left in it. Otherwise the value is read as the authority of
/// `any://{value}`, the subset of `Uri.TryCreate` an override uses: a
/// bracketed IPv6 literal or a host with one colon, followed by a port,
/// gives the host (`DnsSafeHost`: unbracketed, lowercased) and the port;
/// anything else — an unbracketed IPv6 literal, no port, a port that is not a
/// number in range — is returned whole, with no port. Not read as `Uri`
/// would: an IPv6 zone id (`[fe80::1%eth0]:80`, returned whole), userinfo
/// (`user@host:80`), a path after the host, and a host with characters `Uri`
/// refuses.
fn parse_host_and_port(value: String) -> (String, Option<u16>) {
    fn port(text: &str) -> Option<u16> {
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse().ok())
            .flatten()
    }
    if value.contains("://") {
        return (value, None);
    }
    let parsed = if let Some(rest) = value.strip_prefix('[') {
        rest.split_once(']').and_then(|(host, tail)| {
            let port = port(tail.strip_prefix(':')?)?;
            let host: std::net::Ipv6Addr = host.parse().ok()?;
            Some((host.to_string(), port))
        })
    } else {
        value
            .split_once(':')
            .filter(|(host, _)| !host.is_empty())
            .and_then(|(host, tail)| Some((host.to_ascii_lowercase(), port(tail)?)))
    };
    match parsed {
        Some((host, port)) => (host, Some(port)),
        None => (value, None),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::parse_host_and_port;

    /// What `Uri.TryCreate("any://" + value)` refuses, or gives no port for,
    /// is returned whole; a parsed host is `DnsSafeHost`'s.
    #[rstest]
    #[case("Internal.Jellyfin:8097", "internal.jellyfin", Some(8097))]
    #[case("[FD00:1234::1]:8097", "fd00:1234::1", Some(8097))]
    #[case("[fd00:1234::1]", "[fd00:1234::1]", None)]
    #[case("[not-ipv6]:8097", "[not-ipv6]:8097", None)]
    #[case("host:", "host:", None)]
    #[case("host:+80", "host:+80", None)]
    #[case("host:70000", "host:70000", None)]
    #[case(":8097", ":8097", None)]
    #[case("host", "host", None)]
    #[case("http://host:8097", "http://host:8097", None)]
    #[case("host:0080", "host", Some(80))]
    #[case("Host.LAN", "Host.LAN", None)]
    #[case("[fe80::1%eth0]:80", "[fe80::1%eth0]:80", None)]
    fn parse_host_and_port_reads_the_override_as_a_uri_authority(
        #[case] value: &str,
        #[case] host: &str,
        #[case] port: Option<u16>,
    ) {
        assert_eq!(
            parse_host_and_port(value.to_owned()),
            (host.to_owned(), port)
        );
    }
}
