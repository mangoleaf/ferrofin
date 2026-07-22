//! Transliteration of `Jellyfin.Networking.Tests.NetworkManagerTests`.

use std::net::IpAddr;

use hermit_networking::{NetworkConfiguration, NetworkManager};
use rstest::rstest;

/// Splits a `,`-separated list the way the C# `network.Split(',')` does.
fn split(s: &str) -> Vec<String> {
    s.split(',').map(str::to_owned).collect()
}

/// `InNetwork_True_Success`.
#[rstest]
#[case("192.168.2.1/24", "192.168.2.123")]
#[case("192.168.2.1/24, !192.168.2.122/32", "192.168.2.123")]
#[case("fd23:184f:2029:0::/56", "fd23:184f:2029:0:3139:7386:67d7:d517")]
#[case(
    "fd23:184f:2029:0::/56, !fd23:184f:2029:0:3139:7386:67d7:d518/128",
    "fd23:184f:2029:0:3139:7386:67d7:d517"
)]
fn in_network_true_success(#[case] network: &str, #[case] value: &str) {
    let ip: IpAddr = value.parse().unwrap();
    let conf = NetworkConfiguration {
        enable_ipv6: true,
        enable_ipv4: true,
        local_network_subnets: split(network),
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, "");
    assert!(nm.is_in_local_network(ip));
}

/// `InNetwork_False_Success`.
#[rstest]
#[case("192.168.10.0/24", "192.168.11.1")]
#[case("192.168.10.0/24, !192.168.10.60/32", "192.168.10.60")]
#[case("192.168.10.0/24", "fd23:184f:2029:0:3139:7386:67d7:d517")]
#[case("fd23:184f:2029:0::/56", "fd24:184f:2029:0:3139:7386:67d7:d517")]
#[case(
    "fd23:184f:2029:0::/56, !fd23:184f:2029:0:3139:7386:67d7:d500/120",
    "fd23:184f:2029:0:3139:7386:67d7:d517"
)]
#[case("fd23:184f:2029:0::/56", "192.168.10.60")]
#[case("2001:abcd:abcd:6b40::0/60", "192.168.10.60")]
fn in_network_false_success(#[case] network: &str, #[case] value: &str) {
    let ip: IpAddr = value.parse().unwrap();
    let conf = NetworkConfiguration {
        enable_ipv6: true,
        enable_ipv4: true,
        local_network_subnets: split(network),
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, "");
    assert!(!nm.is_in_local_network(ip));
}
