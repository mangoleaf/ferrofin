//! Port of `MediaBrowser.Model.Net.IPData`.
//!
//! The C# original uses `System.Net.IPAddress`/`IPNetwork`; here an IP address
//! maps to [`std::net::IpAddr`] and a subnet to an [`IpNetwork`] CIDR pair.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The address family of a network object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    /// Neither IPv4 nor IPv6 (`AddressFamily.Unspecified`).
    Unspecified,
    /// IPv4 (`AddressFamily.InterNetwork`).
    InterNetwork,
    /// IPv6 (`AddressFamily.InterNetworkV6`).
    InterNetworkV6,
}

impl AddressFamily {
    /// The address family of an [`IpAddr`].
    #[must_use]
    pub fn of(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(_) => Self::InterNetwork,
            IpAddr::V6(_) => Self::InterNetworkV6,
        }
    }
}

/// A CIDR network: a base address plus a prefix length. Mirrors
/// `System.Net.IPNetwork`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNetwork {
    /// The base address of the network.
    pub base_address: IpAddr,
    /// The prefix length, in bits.
    pub prefix_length: u8,
}

impl IpNetwork {
    /// Creates a new network from a base address and prefix length.
    #[must_use]
    pub fn new(base_address: IpAddr, prefix_length: u8) -> Self {
        Self {
            base_address,
            prefix_length,
        }
    }
}

/// The .NET `IPAddress.None` sentinel (`255.255.255.255`).
const IP_NONE: IpAddr = IpAddr::V4(Ipv4Addr::BROADCAST);

/// Base network object — an address, its subnet, and interface metadata.
///
/// Mirrors `MediaBrowser.Model.Net.IPData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpData {
    /// The object's IP address.
    pub address: IpAddr,
    /// The object's subnet.
    pub subnet: IpNetwork,
    /// The interface index.
    pub index: i32,
    /// Whether the network supports multicast.
    pub supports_multicast: bool,
    /// The interface name.
    pub name: String,
}

impl IpData {
    /// Creates a new [`IpData`] from an address, optional subnet and name.
    ///
    /// When `subnet` is `None`, a host subnet is derived from the address (`/32`
    /// for IPv4, `/128` for IPv6), matching the C# constructor.
    #[must_use]
    pub fn new(address: IpAddr, subnet: Option<IpNetwork>, name: impl Into<String>) -> Self {
        let subnet = subnet.unwrap_or_else(|| match address {
            IpAddr::V4(_) => IpNetwork::new(address, 32),
            IpAddr::V6(_) => IpNetwork::new(address, 128),
        });
        Self {
            address,
            subnet,
            index: 0,
            supports_multicast: false,
            name: name.into(),
        }
    }

    /// Gets the address family of the object, mirroring the C# `AddressFamily`
    /// property (falling back to the subnet's base address when the address is
    /// `IPAddress.None`).
    #[must_use]
    pub fn address_family(&self) -> AddressFamily {
        if self.address == IP_NONE {
            if self.subnet.base_address == IP_NONE {
                AddressFamily::Unspecified
            } else {
                AddressFamily::of(self.subnet.base_address)
            }
        } else {
            AddressFamily::of(self.address)
        }
    }
}

/// The IPv6 unspecified address (`::`), provided for convenience alongside the
/// IPv4 sentinel used by the port.
#[must_use]
pub fn ipv6_unspecified() -> IpAddr {
    IpAddr::V6(Ipv6Addr::UNSPECIFIED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_family_of_matches_ip_version() {
        assert_eq!(
            AddressFamily::of(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            AddressFamily::InterNetwork
        );
        assert_eq!(
            AddressFamily::of(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddressFamily::InterNetworkV6
        );
    }

    #[test]
    fn new_derives_host_subnet_for_ipv4() {
        let addr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let data = IpData::new(addr, None, "eth0");
        assert_eq!(data.subnet, IpNetwork::new(addr, 32));
        assert_eq!(data.name, "eth0");
        assert_eq!(data.index, 0);
        assert!(!data.supports_multicast);
    }

    #[test]
    fn new_derives_host_subnet_for_ipv6() {
        let addr = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let data = IpData::new(addr, None, "eth1");
        assert_eq!(data.subnet, IpNetwork::new(addr, 128));
    }

    #[test]
    fn new_keeps_explicit_subnet() {
        let addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let subnet = IpNetwork::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)), 24);
        let data = IpData::new(addr, Some(subnet), "lan");
        assert_eq!(data.subnet, subnet);
    }

    #[test]
    fn address_family_uses_address_when_present() {
        let addr = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        let data = IpData::new(addr, None, "wan");
        assert_eq!(data.address_family(), AddressFamily::InterNetwork);
    }

    #[test]
    fn address_family_falls_back_to_subnet_when_address_is_none() {
        let subnet = IpNetwork::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 128);
        let data = IpData::new(IP_NONE, Some(subnet), "sentinel");
        assert_eq!(data.address_family(), AddressFamily::InterNetworkV6);
    }

    #[test]
    fn address_family_unspecified_when_both_none() {
        let subnet = IpNetwork::new(IP_NONE, 32);
        let data = IpData::new(IP_NONE, Some(subnet), "sentinel");
        assert_eq!(data.address_family(), AddressFamily::Unspecified);
    }

    #[test]
    fn ipv6_unspecified_is_all_zeroes() {
        assert_eq!(ipv6_unspecified(), IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }
}
