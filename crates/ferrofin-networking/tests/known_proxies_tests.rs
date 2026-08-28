//! `KnownProxies` and the forwarded-header walk — C# `AddProxyAddresses` and
//! what ASP.NET's `ForwardedHeadersMiddleware` does with the options
//! `ConfigureForwardHeaders` builds.
//!
//! The API layer has its own end-to-end tests for the same behaviour through a
//! real router; these pin the rule where it is implemented, because that is
//! where someone changing it will look.

use ferrofin_networking::{NetworkConfiguration, NetworkManager};
use std::net::IpAddr;

/// A manager whose `KnownProxies` are `proxies`.
fn manager(proxies: &[&str]) -> NetworkManager {
    NetworkManager::with_defaults(
        NetworkConfiguration {
            known_proxies: proxies.iter().map(|p| (*p).to_owned()).collect(),
            ..NetworkConfiguration::default()
        },
        "",
    )
}

fn ip(s: &str) -> IpAddr {
    s.parse().expect("valid address")
}

/// A bare address becomes a single-host subnet; a CIDR is taken as written.
#[test]
fn a_proxy_may_be_named_as_an_address_or_a_subnet() {
    let nm = manager(&["10.0.0.0/8", "203.0.113.9"]);
    assert!(nm.is_known_proxy(ip("10.4.0.9")), "inside the subnet");
    assert!(nm.is_known_proxy(ip("203.0.113.9")), "the exact address");
    assert!(
        !nm.is_known_proxy(ip("203.0.113.10")),
        "a neighbour of a bare address is NOT a proxy — it is a /32"
    );
    assert!(!nm.is_known_proxy(ip("198.51.100.4")));
}

/// An IPv4-mapped IPv6 peer is matched as the IPv4 address it is, or a
/// dual-stack listener would never recognise its own proxy.
#[test]
fn an_ipv4_mapped_proxy_is_recognised() {
    assert!(manager(&["10.0.0.0/8"]).is_known_proxy(ip("::ffff:10.4.0.9")));
}

/// With no proxies configured the header is ignored entirely — upstream sets
/// `ForwardedHeaders.None`, the only safe default when anyone can send one.
#[test]
fn without_known_proxies_the_chain_is_ignored() {
    let nm = manager(&[]);
    assert_eq!(
        nm.client_address(ip("10.4.0.9"), &[ip("203.0.113.7")]),
        ip("10.4.0.9")
    );
}

/// One hop through a known proxy yields the client it vouched for.
#[test]
fn a_known_proxy_s_client_is_the_client() {
    let nm = manager(&["10.0.0.0/8"]);
    assert_eq!(
        nm.client_address(ip("10.4.0.9"), &[ip("203.0.113.7")]),
        ip("203.0.113.7")
    );
}

/// The walk stops at the first hop that is not a known proxy, so a client
/// cannot push its real address out of view by prepending entries.
#[test]
fn the_walk_stops_at_the_first_unknown_hop() {
    let nm = manager(&["10.0.0.0/8"]);
    assert_eq!(
        nm.client_address(ip("10.4.0.9"), &[ip("198.51.100.1"), ip("203.0.113.7")]),
        ip("203.0.113.7"),
        "the rightmost entry is the one the proxy vouched for"
    );
    // A chain of proxies IS walked through, to the client behind them.
    assert_eq!(
        nm.client_address(ip("10.4.0.9"), &[ip("203.0.113.7"), ip("10.9.9.9")]),
        ip("203.0.113.7"),
        "a hop that is itself a proxy keeps the walk going"
    );
}

/// A peer that is not a proxy cannot claim an address at all.
#[test]
fn a_stranger_cannot_forge_its_own_address() {
    let nm = manager(&["10.0.0.0/8"]);
    assert_eq!(
        nm.client_address(ip("198.51.100.4"), &[ip("203.0.113.7")]),
        ip("198.51.100.4")
    );
}

/// An empty chain from a known proxy leaves the proxy as the address — there is
/// nothing else to use.
#[test]
fn a_known_proxy_with_no_chain_is_itself() {
    let nm = manager(&["10.0.0.0/8"]);
    assert_eq!(nm.client_address(ip("10.4.0.9"), &[]), ip("10.4.0.9"));
}
