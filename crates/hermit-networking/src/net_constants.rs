//! Networking constants — port of `MediaBrowser.Common.Net.NetworkConstants`.
//!
//! The RFC-defined address ranges are domain constants (not settings): they
//! encode the IETF definitions of loopback / private / link-local space and
//! must not drift. Each is exposed as an [`IpNetwork`] via a `const fn`-style
//! accessor because `IpAddr` cannot be built in a `const` initialiser on the
//! pinned toolchain.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use hermit_model::net::IpNetwork;

/// IPv4 mask bytes.
pub const IPV4_MASK_BYTES: usize = 4;

/// IPv6 mask bytes.
pub const IPV6_MASK_BYTES: usize = 16;

/// Minimum IPv4 prefix size (a host route: `/32`).
pub const MINIMUM_IPV4_PREFIX_SIZE: u8 = 32;

/// Minimum IPv6 prefix size (a host route: `/128`).
pub const MINIMUM_IPV6_PREFIX_SIZE: u8 = 128;

/// Whole IPv4 address space (`0.0.0.0/0`).
#[must_use]
pub fn ipv4_any() -> IpNetwork {
    IpNetwork::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
}

/// Whole IPv6 address space (`::/0`).
#[must_use]
pub fn ipv6_any() -> IpNetwork {
    IpNetwork::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
}

/// IPv4 loopback as defined in RFC 5735 (`127.0.0.0/8`).
#[must_use]
pub fn ipv4_rfc5735_loopback() -> IpNetwork {
    IpNetwork::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)), 8)
}

/// IPv4 private class A as defined in RFC 1918 (`10.0.0.0/8`).
#[must_use]
pub fn ipv4_rfc1918_private_class_a() -> IpNetwork {
    IpNetwork::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)), 8)
}

/// IPv4 private class B as defined in RFC 1918 (`172.16.0.0/12`).
#[must_use]
pub fn ipv4_rfc1918_private_class_b() -> IpNetwork {
    IpNetwork::new(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0)), 12)
}

/// IPv4 private class C as defined in RFC 1918 (`192.168.0.0/16`).
#[must_use]
pub fn ipv4_rfc1918_private_class_c() -> IpNetwork {
    IpNetwork::new(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0)), 16)
}

/// IPv4 link-local as defined in RFC 3927 (`169.254.0.0/16`).
#[must_use]
pub fn ipv4_rfc3927_link_local() -> IpNetwork {
    IpNetwork::new(IpAddr::V4(Ipv4Addr::new(169, 254, 0, 0)), 16)
}

/// IPv6 loopback as defined in RFC 4291 (`::1/128`).
#[must_use]
pub fn ipv6_rfc4291_loopback() -> IpNetwork {
    IpNetwork::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 128)
}

/// IPv6 site-local as defined in RFC 4291 (`fe80::/10`).
#[must_use]
pub fn ipv6_rfc4291_site_local() -> IpNetwork {
    IpNetwork::new(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)), 10)
}

/// IPv6 unique-local as defined in RFC 4193 (`fc00::/7`).
#[must_use]
pub fn ipv6_rfc4193_unique_local() -> IpNetwork {
    IpNetwork::new(IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0)), 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_ranges_are_zero_prefix() {
        assert_eq!(ipv4_any().prefix_length, 0);
        assert_eq!(ipv6_any().prefix_length, 0);
        assert_eq!(ipv4_any().base_address, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(ipv6_any().base_address, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn rfc_ranges_have_expected_prefixes() {
        assert_eq!(ipv4_rfc5735_loopback().prefix_length, 8);
        assert_eq!(ipv4_rfc1918_private_class_a().prefix_length, 8);
        assert_eq!(ipv4_rfc1918_private_class_b().prefix_length, 12);
        assert_eq!(ipv4_rfc1918_private_class_c().prefix_length, 16);
        assert_eq!(ipv4_rfc3927_link_local().prefix_length, 16);
        assert_eq!(ipv6_rfc4291_loopback().prefix_length, 128);
        assert_eq!(ipv6_rfc4291_site_local().prefix_length, 10);
        assert_eq!(ipv6_rfc4193_unique_local().prefix_length, 7);
    }

    #[test]
    fn rfc_ranges_have_expected_base_addresses() {
        assert_eq!(
            ipv4_rfc1918_private_class_a().base_address,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0))
        );
        assert_eq!(
            ipv4_rfc1918_private_class_b().base_address,
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 0))
        );
        assert_eq!(
            ipv4_rfc1918_private_class_c().base_address,
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 0))
        );
        assert_eq!(
            ipv6_rfc4291_loopback().base_address,
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
    }
}
