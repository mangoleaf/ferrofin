//! `KnownProxies` parsed into the addresses a forwarded header is trusted
//! from — C# `ConfigureForwardHeaders` with its `AddProxyAddresses` and
//! `AddIPAddress` (`Jellyfin.Server/Extensions/ApiServiceCollectionExtensions.cs`).
//!
//! A host-name entry is resolved (`NetworkUtils.TryParseHost`), which can block
//! on DNS. [`KnownProxies`] keeps the inputs it was parsed from, so the manager
//! re-resolves only when those change, never on an interface refresh; a caller
//! holding the manager behind a lock resolves outside it with
//! [`KnownProxies::resolve`] and hands the result over.

use std::net::IpAddr;

use ferrofin_model::net::IpNetwork;

use crate::net_constants;
use crate::net_utils::{self, HostResolver};
use crate::network_configuration::NetworkConfiguration;

/// The parsed `KnownProxies` setting.
#[derive(Debug, Clone, Default)]
pub struct KnownProxies {
    /// The entries this was parsed from, as configured.
    entries: Vec<String>,
    /// `EnableIPv4` when parsed: `AddIPAddress` drops a disabled family.
    enable_ipv4: bool,
    /// `EnableIPv6` when parsed.
    enable_ipv6: bool,
    /// The trusted networks, in entry order: single hosts (upstream's
    /// `ForwardedHeadersOptions.KnownProxies`) and subnets (`KnownIPNetworks`).
    networks: Vec<IpNetwork>,
    /// The entries that were neither an address, a subnet nor a resolvable
    /// host name (upstream drops them silently).
    unresolved: Vec<String>,
}

impl KnownProxies {
    /// Parses `config.known_proxies` as `AddProxyAddresses` does: an address is
    /// a single-host proxy, a subnet is taken as written (its host bits
    /// cleared, where `System.Net.IPNetwork` would refuse them), and anything
    /// else is a host name looked up with `resolver`, each of its addresses a
    /// single-host proxy. Every address then goes through `AddIPAddress`: an
    /// IPv4-mapped one is unmapped, and one of a family `config` disables is
    /// dropped.
    ///
    /// The lookup can block on DNS; resolve outside any lock.
    #[must_use]
    pub fn resolve(config: &NetworkConfiguration, resolver: HostResolver) -> Self {
        let mut proxies = Self {
            entries: config.known_proxies.clone(),
            enable_ipv4: config.enable_ipv4,
            enable_ipv6: config.enable_ipv6,
            ..Self::default()
        };
        for entry in &config.known_proxies {
            if let Ok(address) = entry.trim().parse::<IpAddr>() {
                proxies.add_ip_address(address, single_host_prefix(address));
            } else if let Some(subnet) =
                net_utils::try_parse_to_subnet(entry, false).map(|data| data.subnet)
            {
                proxies.add_ip_address(subnet.base_address, subnet.prefix_length);
            } else if let Some(addresses) = net_utils::try_parse_host_with(
                entry,
                config.enable_ipv4,
                config.enable_ipv6,
                resolver,
            ) {
                for address in addresses {
                    proxies.add_ip_address(address, single_host_prefix(address));
                }
            } else {
                proxies.unresolved.push(entry.clone());
            }
        }
        proxies
    }

    /// Whether this was parsed from `config`'s entries and address families,
    /// so parsing `config` again would only repeat the same lookups.
    #[must_use]
    pub fn is_parsed_from(&self, config: &NetworkConfiguration) -> bool {
        self.entries == config.known_proxies
            && self.enable_ipv4 == config.enable_ipv4
            && self.enable_ipv6 == config.enable_ipv6
    }

    /// The trusted networks, in entry order.
    #[must_use]
    pub fn networks(&self) -> &[IpNetwork] {
        &self.networks
    }

    /// The entries that parsed to nothing, for the caller to report.
    #[must_use]
    pub fn unresolved(&self) -> &[String] {
        &self.unresolved
    }

    /// Whether forwarded headers are read at all: upstream turns them off
    /// (`ForwardedHeaders.None`) only when the setting is empty, not when
    /// every entry fails to parse.
    #[must_use]
    pub fn forwarding_enabled(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Whether `address` may vouch for the client behind it: one of the
    /// parsed networks, or, while forwarding is enabled, ASP.NET's default
    /// `KnownProxies` (`::1`) and `KnownIPNetworks` (`127.0.0.0/8`), which
    /// `ConfigureForwardHeaders` clears only when the setting is empty. An
    /// IPv4-mapped peer is matched as the IPv4 address it is.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let address = net_utils::unmap_v4_mapped(address);
        (self.forwarding_enabled() && address.is_loopback())
            || self
                .networks
                .iter()
                .any(|network| net_utils::subnet_contains_address(*network, address))
    }

    /// `AddIPAddress`: unmaps an IPv4-mapped address and drops one of a
    /// disabled family.
    ///
    /// Upstream keeps the IPv6 prefix on an unmapped address, which
    /// `System.Net.IPNetwork` then rejects (a /128 on an IPv4 address); the
    /// mapped prefix is translated instead (/128 to /32, /104 to /8). A subnet
    /// wider than the mapped range (a prefix under /96) stays IPv6: unmapping
    /// it would trust IPv4 space it does not name.
    fn add_ip_address(&mut self, address: IpAddr, prefix_length: u8) {
        let (address, prefix_length) = match address {
            IpAddr::V6(v6) if prefix_length >= 96 => {
                v6.to_ipv4_mapped().map_or((address, prefix_length), |v4| {
                    (IpAddr::V4(v4), prefix_length - 96)
                })
            }
            _ => (address, prefix_length),
        };
        let enabled = match address {
            IpAddr::V4(_) => self.enable_ipv4,
            IpAddr::V6(_) => self.enable_ipv6,
        };
        if enabled {
            self.networks.push(IpNetwork::new(address, prefix_length));
        }
    }
}

/// The prefix `AddProxyAddresses` files a bare address under: `/32` or `/128`.
fn single_host_prefix(address: IpAddr) -> u8 {
    match address {
        IpAddr::V4(_) => net_constants::MINIMUM_IPV4_PREFIX_SIZE,
        IpAddr::V6(_) => net_constants::MINIMUM_IPV6_PREFIX_SIZE,
    }
}
