//! `KnownProxies` and the forwarded-header walk — C# `AddProxyAddresses` and
//! what ASP.NET's `ForwardedHeadersMiddleware` does with the options
//! `ConfigureForwardHeaders` builds.
//!
//! The API layer has its own end-to-end tests for the same behaviour through a
//! real router; these pin the rule where it is implemented, because that is
//! where someone changing it will look.

use ferrofin_model::net::IpNetwork;
use ferrofin_networking::{KnownProxies, NetworkConfiguration, NetworkManager};
use rstest::rstest;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `Dns.GetHostAddresses` without the network: `localhost` is both loopbacks,
/// every other name fails to resolve.
fn resolve(host: &str) -> Option<Vec<IpAddr>> {
    (host == "localhost").then(|| vec![ip("127.0.0.1"), ip("::1")])
}

/// A manager on [`resolve`] whose `KnownProxies` are `proxies`, with the
/// address families switched as given.
fn manager_with(proxies: &[&str], enable_ipv4: bool, enable_ipv6: bool) -> NetworkManager {
    let mut nm =
        NetworkManager::with_defaults(NetworkConfiguration::default(), "").with_resolver(resolve);
    nm.update_settings(&NetworkConfiguration {
        known_proxies: proxies.iter().map(|p| (*p).to_owned()).collect(),
        enable_ipv4,
        enable_ipv6,
        ..NetworkConfiguration::default()
    });
    nm
}

/// A single-host network, as `AddIPAddress` files a bare address.
fn host(s: &str) -> IpNetwork {
    let address = ip(s);
    IpNetwork::new(address, if address.is_ipv4() { 32 } else { 128 })
}

/// `ParseNetworkTests.TestNetworks` (v12.2): what `AddProxyAddresses` makes of
/// each `KnownProxies` list. Literals and host names alike are filed as
/// single-host proxies, filtered by the enabled address families, and an
/// entry that is neither an address, a subnet nor a resolvable name is
/// dropped.
#[rstest]
#[case(true, true, &["192.168.t", "127.0.0.1", "::1", "1234.1232.12.1234"], &["127.0.0.1", "::1"])]
#[case(true, false, &["192.168.x", "127.0.0.1", "1234.1232.12.1234"], &["127.0.0.1"])]
#[case(true, true, &["::1"], &["::1"])]
#[case(false, false, &["localhost"], &[])]
#[case(true, false, &["localhost"], &["127.0.0.1"])]
#[case(false, true, &["localhost"], &["::1"])]
#[case(true, true, &["localhost"], &["127.0.0.1", "::1"])]
fn known_proxies_parse_as_add_proxy_addresses_does(
    #[case] enable_ipv4: bool,
    #[case] enable_ipv6: bool,
    #[case] entries: &[&str],
    #[case] expected: &[&str],
) {
    let nm = manager_with(entries, enable_ipv4, enable_ipv6);
    let expected: Vec<IpNetwork> = expected.iter().map(|e| host(e)).collect();
    assert_eq!(nm.known_proxies(), expected.as_slice());
}

/// A proxy named by host name is trusted for the clients it vouches for:
/// before the host-name branch, such an entry was dropped and the proxy itself
/// looked like the client.
#[test]
fn a_proxy_named_by_host_name_vouches_for_its_client() {
    let nm = manager_with(&["localhost"], true, true);
    assert_eq!(
        nm.client_address(ip("127.0.0.1"), &[ip("203.0.113.7")]),
        ip("203.0.113.7")
    );
}

/// `AddIPAddress` unmaps an IPv4-mapped entry before filing it, so it matches
/// the IPv4 peer it names.
#[test]
fn an_ipv4_mapped_entry_is_filed_as_ipv4() {
    let nm = manager_with(&["::ffff:10.0.0.5"], true, false);
    assert_eq!(nm.known_proxies(), [host("10.0.0.5")]);
    assert!(nm.is_known_proxy(ip("10.0.0.5")));
}

/// A mapped subnet within the mapped range unmaps with its prefix translated;
/// a wider one names more than IPv4 space and stays IPv6.
#[test]
fn a_mapped_subnet_is_unmapped_only_within_the_mapped_range() {
    let nm = manager_with(&["::ffff:10.0.0.0/104", "::ffff:0:0/80"], true, true);
    assert_eq!(
        nm.known_proxies(),
        [
            IpNetwork::new(ip("10.0.0.0"), 8),
            IpNetwork::new(ip("::"), 80)
        ]
    );
    assert!(nm.is_known_proxy(ip("10.4.0.9")));
    assert!(!nm.is_known_proxy(ip("192.0.2.1")));
}

/// `ConfigureForwardHeaders` keeps ASP.NET's default `KnownProxies` (`::1`) and
/// `KnownIPNetworks` (`127.0.0.0/8`) whenever the setting is not empty, even
/// when no entry parses; an empty setting turns forwarding off altogether.
#[test]
fn a_configured_list_also_trusts_the_loopback_defaults() {
    for proxies in [&["10.0.0.0/8"][..], &["no-such-proxy"][..]] {
        let nm = manager_with(proxies, true, true);
        for loopback in ["127.0.0.1", "127.8.9.10", "::1"] {
            assert_eq!(
                nm.client_address(ip(loopback), &[ip("203.0.113.7")]),
                ip("203.0.113.7"),
                "{proxies:?} {loopback}"
            );
        }
    }
    let nm = manager_with(&[], true, true);
    assert_eq!(
        nm.client_address(ip("127.0.0.1"), &[ip("203.0.113.7")]),
        ip("127.0.0.1")
    );
}

/// The entries that parse to nothing are kept for the manager to warn about.
#[test]
fn unparsable_entries_are_reported() {
    let config = NetworkConfiguration {
        known_proxies: vec!["no-such-proxy".to_owned(), "10.0.0.1".to_owned()],
        ..NetworkConfiguration::default()
    };
    let proxies = KnownProxies::resolve(&config, resolve);
    assert_eq!(proxies.unresolved(), ["no-such-proxy"]);
    assert_eq!(proxies.networks(), [host("10.0.0.1")]);
}

/// A settings reload that leaves the entries and the address families alone
/// (every live interface refresh) looks no host name up again; a changed
/// family does.
#[test]
fn host_names_are_resolved_again_only_when_their_inputs_change() {
    static LOOKUPS: AtomicUsize = AtomicUsize::new(0);
    fn counting(host: &str) -> Option<Vec<IpAddr>> {
        LOOKUPS.fetch_add(1, Ordering::SeqCst);
        resolve(host)
    }
    let config = NetworkConfiguration {
        known_proxies: vec!["localhost".to_owned()],
        ..NetworkConfiguration::default()
    };
    let mut nm =
        NetworkManager::with_defaults(NetworkConfiguration::default(), "").with_resolver(counting);
    nm.update_settings(&config);
    let after_first = LOOKUPS.load(Ordering::SeqCst);
    assert!(after_first > 0);
    nm.update_settings(&config);
    assert_eq!(LOOKUPS.load(Ordering::SeqCst), after_first);

    nm.update_settings(&NetworkConfiguration {
        enable_ipv6: !config.enable_ipv6,
        ..config.clone()
    });
    assert!(LOOKUPS.load(Ordering::SeqCst) > after_first);
}

/// Proxies resolved outside the manager's lock are taken as they are; ones
/// resolved from another configuration are parsed again.
#[test]
fn proxies_resolved_ahead_are_used_only_for_their_configuration() {
    let config = NetworkConfiguration {
        known_proxies: vec!["localhost".to_owned()],
        ..NetworkConfiguration::default()
    };
    let mut nm =
        NetworkManager::with_defaults(NetworkConfiguration::default(), "").with_resolver(resolve);
    nm.update_settings_with_known_proxies(&config, KnownProxies::resolve(&config, resolve));
    assert_eq!(nm.known_proxies(), [host("127.0.0.1")]);

    let stale = KnownProxies::resolve(&NetworkConfiguration::default(), resolve);
    let other = NetworkConfiguration {
        known_proxies: vec!["10.0.0.1".to_owned()],
        ..NetworkConfiguration::default()
    };
    nm.update_settings_with_known_proxies(&other, stale);
    assert_eq!(nm.known_proxies(), [host("10.0.0.1")]);
}

/// `AddIPAddress` drops an entry of a disabled address family, literal or
/// subnet alike.
#[test]
fn an_entry_of_a_disabled_family_is_dropped() {
    let nm = manager_with(&["::1", "fd00::/8", "10.0.0.0/8"], true, false);
    assert_eq!(nm.known_proxies(), [IpNetwork::new(ip("10.0.0.0"), 8)]);
}

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
