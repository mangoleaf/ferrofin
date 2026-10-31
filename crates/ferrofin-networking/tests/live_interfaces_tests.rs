//! Deterministic interface snapshot and advertised-listener regression tests.

use std::net::IpAddr;

use ferrofin_model::net::{IpData, IpNetwork};
use ferrofin_networking::{NetworkConfiguration, NetworkManager};

fn interface(address: &str, prefix: u8, name: &str, index: i32) -> IpData {
    let address = address.parse().expect("test address");
    let mut interface = IpData::new(address, Some(IpNetwork::new(address, prefix)), name);
    interface.index = index;
    interface
}

#[test]
fn empty_inputs_never_enumerate_the_host() {
    let peer = "192.168.1.50".parse().expect("peer");
    for mut manager in [
        NetworkManager::with_defaults(NetworkConfiguration::default(), ""),
        NetworkManager::with_interfaces(NetworkConfiguration::default(), Vec::new()),
    ] {
        manager.refresh_interfaces();
        let all_bind_interfaces = manager.get_all_bind_interfaces(true);
        assert!(all_bind_interfaces.is_empty(), "{all_bind_interfaces:?}");
        assert_eq!(manager.get_bind_address_for_peer(peer, None).0, "127.0.0.1");
    }
}

#[test]
fn next_resolution_uses_replaced_snapshot_and_real_prefix() {
    let peer = "192.168.1.50".parse().expect("peer");
    let mut manager = NetworkManager::with_interfaces(
        NetworkConfiguration::default(),
        vec![interface("192.168.1.2", 24, "lan", 2)],
    );
    assert_eq!(
        manager.get_bind_address_for_peer(peer, None).0,
        "192.168.1.2"
    );
    assert_eq!(
        manager.get_all_bind_interfaces(true)[0]
            .subnet
            .prefix_length,
        24
    );
    manager.set_interfaces(vec![interface("192.168.1.3", 24, "lan", 2)]);
    assert_eq!(
        manager.get_bind_address_for_peer(peer, None).0,
        "192.168.1.3"
    );
    manager.set_interfaces(Vec::new());
    assert_eq!(manager.get_bind_address_for_peer(peer, None).0, "127.0.0.1");
}

#[test]
fn explicit_http_bind_does_not_remove_other_policy_interfaces() {
    let peer = "192.168.1.50".parse().expect("peer");
    let manager = NetworkManager::with_interfaces(
        NetworkConfiguration::default(),
        vec![
            interface("192.168.1.2", 24, "lan", 2),
            interface("10.1.1.2", 24, "other", 3),
        ],
    );
    let bind: IpAddr = "10.1.1.2".parse().expect("bind");
    assert_eq!(
        manager.get_bind_address_for_peer(peer, Some(bind)).0,
        "10.1.1.2"
    );
    assert_eq!(manager.get_all_bind_interfaces(true).len(), 2);
    assert_eq!(
        manager
            .get_bind_address_for_peer(peer, Some("0.0.0.0".parse().expect("wildcard")))
            .0,
        "192.168.1.2"
    );
    // The already-bound HTTP address stays usable even if no snapshot reports it.
    let manager = NetworkManager::with_interfaces(NetworkConfiguration::default(), Vec::new());
    assert_eq!(
        manager.get_bind_address_for_peer(peer, Some(bind)).0,
        "10.1.1.2"
    );
}

#[test]
fn published_override_precedes_http_bind_and_updates_on_save() {
    let mut config = NetworkConfiguration {
        published_server_uri_by_subnet: vec!["all=https://public.example/jellyfin".to_owned()],
        ..Default::default()
    };
    let mut manager = NetworkManager::with_interfaces(
        config.clone(),
        vec![interface("192.168.1.2", 24, "lan", 2)],
    );
    let peer = "192.168.1.50".parse().expect("peer");
    let bind = Some("127.0.0.1".parse().expect("bind"));
    assert_eq!(
        manager.get_bind_address_for_peer(peer, bind).0,
        "https://public.example/jellyfin"
    );
    config.published_server_uri_by_subnet = vec!["all=public.example:9000".to_owned()];
    manager.update_settings(&config);
    assert_eq!(
        manager.get_bind_address_for_peer(peer, bind),
        ("public.example".to_owned(), Some(9000))
    );
}

#[test]
fn snapshot_filters_virtual_interfaces_and_disabled_families() {
    let config = NetworkConfiguration {
        enable_ipv6: false,
        ignore_virtual_interfaces: true,
        virtual_interface_names: vec!["veth*".to_owned()],
        ..Default::default()
    };
    let manager = NetworkManager::with_interfaces(
        config,
        vec![
            interface("192.168.1.2", 24, "lan", 2),
            interface("10.1.1.2", 24, "veth123", 3),
            interface("fd00::2", 64, "lan", 2),
        ],
    );
    assert_eq!(manager.get_all_bind_interfaces(true).len(), 1);
    assert_eq!(manager.get_all_bind_interfaces(true)[0].name, "lan");
    assert!(manager.auto_discovery());
}

#[test]
fn base_path_normalization_is_idempotent() {
    for input in ["", "/", "///", "jellyfin", "/jellyfin///"] {
        let normalized = ferrofin_networking::normalize_base_url(input);
        assert_eq!(
            ferrofin_networking::normalize_base_url(&normalized),
            normalized
        );
    }
    assert_eq!(
        ferrofin_networking::normalize_base_url("/jellyfin///"),
        "/jellyfin"
    );
}
