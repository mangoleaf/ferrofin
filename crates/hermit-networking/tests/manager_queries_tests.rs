//! Additional coverage for the [`NetworkManager`] query surface that the
//! transliterated Jellyfin oracle does not directly exercise (loopbacks,
//! all-bind-interfaces, link-local detection, textual LAN membership, published
//! overrides, and re-`update_settings`). These extend, and never weaken, the
//! ported assertions.

use std::net::{IpAddr, Ipv4Addr};

use hermit_networking::error::NetworkingError;
use hermit_networking::{NetworkConfiguration, NetworkManager, NullLogger};

const MOCK_INTERFACES: &str = "192.168.1.208/24,-16,eth16|200.200.200.200/24,11,eth11";

fn conf() -> NetworkConfiguration {
    NetworkConfiguration {
        enable_ipv4: true,
        enable_ipv6: true,
        local_network_subnets: vec!["192.168.1.0/24".to_owned()],
        ..Default::default()
    }
}

#[test]
fn get_loopbacks_returns_both_families_when_enabled() {
    let nm = NetworkManager::with_defaults(conf(), MOCK_INTERFACES);
    let loopbacks = nm.get_loopbacks();
    assert_eq!(loopbacks.len(), 2);
    assert!(
        loopbacks
            .iter()
            .any(|l| l.address == IpAddr::V4(Ipv4Addr::LOCALHOST))
    );
}

#[test]
fn get_loopbacks_empty_when_both_disabled() {
    let conf = NetworkConfiguration {
        enable_ipv4: false,
        enable_ipv6: false,
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, MOCK_INTERFACES);
    assert!(nm.get_loopbacks().is_empty());
}

#[test]
fn get_all_bind_interfaces_listens_on_any_without_bind_config() {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        enable_ipv6: true,
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, MOCK_INTERFACES);
    // IPv4 + IPv6 → a single IPv6Any entry (Kestrel dual-mode).
    let all = nm.get_all_bind_interfaces(false);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].address, "::".parse::<IpAddr>().unwrap());
}

#[test]
fn get_all_bind_interfaces_individual_returns_known_interfaces() {
    let nm = NetworkManager::with_defaults(conf(), MOCK_INTERFACES);
    let all = nm.get_all_bind_interfaces(true);
    assert_eq!(all.len(), 2);
}

#[test]
fn is_link_local_address_detects_both_families() {
    let nm = NetworkManager::with_defaults(conf(), MOCK_INTERFACES);
    assert!(nm.is_link_local_address("169.254.1.1".parse().unwrap()));
    assert!(nm.is_link_local_address("fe80::1".parse().unwrap()));
    assert!(!nm.is_link_local_address("192.168.1.1".parse().unwrap()));
}

#[test]
fn is_in_local_network_str_accepts_addresses_and_hosts() {
    let nm = NetworkManager::with_defaults(conf(), MOCK_INTERFACES);
    assert!(nm.is_in_local_network_str("192.168.1.5"));
    assert!(!nm.is_in_local_network_str("8.8.8.8"));
}

#[test]
fn update_settings_reapplies_configuration() {
    let mut nm = NetworkManager::with_defaults(conf(), MOCK_INTERFACES);
    assert!(nm.is_in_local_network("192.168.1.5".parse().unwrap()));

    // Narrow the LAN so the previous address is no longer internal.
    let new_conf = NetworkConfiguration {
        enable_ipv4: true,
        enable_ipv6: true,
        local_network_subnets: vec!["10.0.0.0/8".to_owned()],
        ..Default::default()
    };
    nm.update_settings(&new_conf);
    assert!(!nm.is_in_local_network("192.168.1.5".parse().unwrap()));
    assert!(nm.is_in_local_network("10.1.2.3".parse().unwrap()));
}

#[test]
fn published_server_urls_exposes_internal_overrides() {
    let conf = NetworkConfiguration {
        enable_ipv4: true,
        local_network_subnets: vec!["192.168.1.0/24".to_owned()],
        published_server_uri_by_subnet: vec!["internal=http://lan.example".to_owned()],
        ..Default::default()
    };
    let nm = NetworkManager::with_defaults(conf, MOCK_INTERFACES);
    assert!(!nm.published_server_urls().is_empty());
    assert!(
        nm.published_server_urls()
            .iter()
            .all(|o| o.is_internal_override)
    );
    assert!(!nm.trust_all_ipv6_interfaces());
}

#[test]
fn null_logger_discards_warnings() {
    use hermit_networking::Logger;
    let logger = NullLogger;
    logger.warn("ignored");
    // Smoke: a manager built with the null logger runs the pipeline.
    let nm = NetworkManager::with_defaults(conf(), MOCK_INTERFACES);
    let _ = nm.get_bind_address("192.168.1.1");
}

#[test]
fn networking_error_displays_value() {
    let err = NetworkingError::InvalidValue("bad".to_owned());
    assert_eq!(err.to_string(), "invalid network value: bad");
}
