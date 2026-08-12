//! Transliteration of `Jellyfin.Networking.Tests.NetworkExtensionsTests`.
//!
//! The two FsCheck `[Property]` generators (`TryParse_IPv4Address_True`,
//! `TryParse_IPv6Address_True`) are represented by a representative sample of
//! addresses rather than a randomized property, matching the port charter's
//! FsCheck note.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ferrofin_networking::net_utils;
use rstest::rstest;

/// Checks valid host strings (`TryParse_ValidHostStrings_True`).
#[rstest]
#[case("127.0.0.1")]
#[case("127.0.0.1:123")]
#[case("localhost")]
#[case("localhost:1345")]
#[case("fd23:184f:2029:0:3139:7386:67d7:d517")]
#[case("fd23:184f:2029:0:3139:7386:67d7:d517/56")]
#[case("[fd23:184f:2029:0:3139:7386:67d7:d517]:124")]
#[case("fe80::7add:12ff:febb:c67b%16")]
#[case("[fe80::7add:12ff:febb:c67b%16]:123")]
#[case("fe80::7add:12ff:febb:c67b%16:123")]
#[case("[fe80::7add:12ff:febb:c67b%16]")]
#[case("192.168.1.2/255.255.255.0")]
#[case("192.168.1.2/24")]
fn try_parse_valid_host_strings_true(#[case] address: &str) {
    assert!(net_utils::try_parse_host(address, true, true).is_some());
}

/// Representative sample for the FsCheck `TryParse_IPv4Address_True` property.
#[rstest]
#[case(Ipv4Addr::UNSPECIFIED)]
#[case(Ipv4Addr::LOCALHOST)]
#[case(Ipv4Addr::new(192, 168, 1, 1))]
#[case(Ipv4Addr::new(8, 8, 8, 8))]
#[case(Ipv4Addr::BROADCAST)]
fn try_parse_ipv4_address_true(#[case] address: Ipv4Addr) {
    assert!(net_utils::try_parse_host(&IpAddr::V4(address).to_string(), true, true).is_some());
}

/// Representative sample for the FsCheck `TryParse_IPv6Address_True` property.
#[rstest]
#[case(Ipv6Addr::UNSPECIFIED)]
#[case(Ipv6Addr::LOCALHOST)]
#[case(Ipv6Addr::new(0xfd23, 0x184f, 0x2029, 0, 0x3139, 0x7386, 0x67d7, 0xd517))]
#[case(Ipv6Addr::new(0x2001, 0xdb8, 0xabcd, 0x12, 0, 0, 0, 0))]
fn try_parse_ipv6_address_true(#[case] address: Ipv6Addr) {
    assert!(net_utils::try_parse_host(&IpAddr::V6(address).to_string(), true, true).is_some());
}

/// Checks invalid host strings (`TryParse_InvalidAddressString_False`).
#[rstest]
#[case("256.128.0.0.0.1")]
#[case("127.0.0.1#")]
#[case("localhost!")]
#[case("fd23:184f:2029:0:3139:7386:67d7:d517:1231")]
#[case("[fd23:184f:2029:0:3139:7386:67d7:d517:1231]")]
fn try_parse_invalid_address_string_false(#[case] address: &str) {
    assert!(net_utils::try_parse_host(address, true, true).is_none());
}
