//! OS network changes: what `NetworkManager.OnNetworkChange` refreshes, and
//! the monitor's 2-second wait that folds a burst into one refresh
//! (`HandleNetworkChange`).

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ferrofin_model::net::{IpData, IpNetwork};
use ferrofin_networking::network_change::{self, NETWORK_CHANGE_DELAY, NetworkChange};
use ferrofin_networking::{InterfaceEnumerator, NetworkConfiguration, NetworkManager, NullLogger};
use tokio::sync::mpsc::unbounded_channel;

fn interface(address: &str, prefix: u8, name: &str, index: i32) -> IpData {
    let address = address.parse().expect("test address");
    let mut interface = IpData::new(address, Some(IpNetwork::new(address, prefix)), name);
    interface.index = index;
    interface
}

/// The operating system's interfaces, which a test changes, and how many
/// times the manager read them.
#[derive(Clone, Default)]
struct FakeHost {
    interfaces: Arc<Mutex<Vec<IpData>>>,
    reads: Arc<AtomicUsize>,
}

impl FakeHost {
    fn with(interfaces: Vec<IpData>) -> Self {
        let host = Self::default();
        host.set(interfaces);
        host
    }

    fn set(&self, interfaces: Vec<IpData>) {
        *self.interfaces.lock().expect("interfaces") = interfaces;
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// Reads like `GetInterfacesCore`: only the enabled families.
    fn enumerator(&self) -> InterfaceEnumerator {
        let host = self.clone();
        Arc::new(move |ipv4, ipv6| {
            host.reads.fetch_add(1, Ordering::SeqCst);
            host.interfaces
                .lock()
                .expect("interfaces")
                .iter()
                .filter(|i| (ipv4 && i.address.is_ipv4()) || (ipv6 && i.address.is_ipv6()))
                .cloned()
                .collect()
        })
    }
}

fn bind_addresses(manager: &NetworkManager) -> Vec<IpAddr> {
    manager
        .get_all_bind_interfaces(true)
        .into_iter()
        .map(|i| i.address)
        .collect()
}

fn addresses(list: &[&str]) -> Vec<IpAddr> {
    list.iter().map(|a| a.parse().expect("address")).collect()
}

#[test]
fn a_network_change_reads_the_interfaces_again_and_reapplies_the_bind_settings() {
    let host = FakeHost::with(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    let mut manager = NetworkManager::live_with_enumerator(
        NetworkConfiguration::default(),
        host.enumerator(),
        Arc::new(NullLogger),
    );
    assert_eq!(bind_addresses(&manager), addresses(&["192.168.1.2"]));

    // A VPN comes up, a container's veth appears, and an IPv6 address the
    // configuration does not enable.
    host.set(vec![
        interface("192.168.1.2", 24, "eth0", 2),
        interface("10.8.0.5", 24, "tun0", 7),
        interface("172.17.0.1", 16, "veth12ab", 9),
        interface("fd00::5", 64, "eth0", 2),
    ]);
    assert_eq!(bind_addresses(&manager), addresses(&["192.168.1.2"]));
    manager.on_network_change_with(|| true);
    assert_eq!(
        bind_addresses(&manager),
        addresses(&["192.168.1.2", "10.8.0.5"]),
        "IgnoreVirtualInterfaces drops veth*, and IPv6 stays off"
    );
    let local = manager.get_internal_bind_addresses();
    assert!(
        local
            .iter()
            .any(|i| i.address == "192.168.1.2".parse::<IpAddr>().expect("ip")),
        "{local:?}"
    );

    // The VPN goes away again.
    host.set(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    manager.on_network_change_with(|| true);
    assert_eq!(bind_addresses(&manager), addresses(&["192.168.1.2"]));
}

#[test]
fn a_network_change_applies_the_bind_addresses_to_the_new_interfaces() {
    // `LocalNetworkAddresses` names an interface that does not exist yet.
    let config = NetworkConfiguration {
        local_network_addresses: vec!["tun0".to_owned()],
        ..NetworkConfiguration::default()
    };
    let host = FakeHost::with(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    let mut manager =
        NetworkManager::live_with_enumerator(config, host.enumerator(), Arc::new(NullLogger));
    assert_eq!(bind_addresses(&manager), Vec::<IpAddr>::new());

    host.set(vec![
        interface("192.168.1.2", 24, "eth0", 2),
        interface("10.8.0.5", 24, "tun0", 7),
    ]);
    manager.on_network_change_with(|| true);
    assert_eq!(bind_addresses(&manager), addresses(&["10.8.0.5"]));
}

/// Upstream's `OnNetworkChange` runs `InitializeInterfaces`, `InitializeLan`
/// and `EnforceBindSettings`, not `InitializeOverrides`: an override naming
/// an interface that appeared later stays unresolved. Only when IPv6 is
/// enabled on a host without IPv6 does it run the whole `UpdateSettings`.
#[test]
fn a_network_change_leaves_the_published_overrides_unless_the_host_lacks_ipv6() {
    let config = NetworkConfiguration {
        enable_ipv6: true,
        published_server_uri_by_subnet: vec!["tun0=https://vpn.example.org".to_owned()],
        ..NetworkConfiguration::default()
    };
    let host = FakeHost::with(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    let mut manager =
        NetworkManager::live_with_enumerator(config, host.enumerator(), Arc::new(NullLogger));
    assert_eq!(manager.published_server_urls(), []);

    host.set(vec![
        interface("192.168.1.2", 24, "eth0", 2),
        interface("10.8.0.5", 24, "tun0", 7),
    ]);
    manager.on_network_change_with(|| true);
    assert_eq!(manager.published_server_urls(), []);
    assert_eq!(
        bind_addresses(&manager),
        addresses(&["192.168.1.2", "10.8.0.5"])
    );

    manager.on_network_change_with(|| false);
    let overrides = manager.published_server_urls();
    assert_eq!(overrides.len(), 1, "{overrides:?}");
    assert_eq!(overrides[0].override_uri, "https://vpn.example.org");
}

#[test]
fn a_network_change_leaves_a_supplied_snapshot_alone() {
    let mut manager = NetworkManager::with_interfaces(
        NetworkConfiguration::default(),
        vec![interface("192.168.1.2", 24, "eth0", 2)],
    );
    manager.on_network_change_with(|| true);
    manager.on_network_change_with(|| false);
    assert_eq!(bind_addresses(&manager), addresses(&["192.168.1.2"]));
}

/// A manager over `host`, and the monitor on a channel the test sends to.
fn start_monitor(
    host: &FakeHost,
) -> (
    Arc<RwLock<NetworkManager>>,
    tokio::sync::mpsc::UnboundedSender<NetworkChange>,
    tokio::task::JoinHandle<()>,
) {
    let manager = Arc::new(RwLock::new(NetworkManager::live_with_enumerator(
        NetworkConfiguration::default(),
        host.enumerator(),
        Arc::new(NullLogger),
    )));
    let (changes, source) = unbounded_channel();
    let task = tokio::spawn(network_change::monitor(
        source,
        Arc::clone(&manager),
        NETWORK_CHANGE_DELAY,
    ));
    (manager, changes, task)
}

/// Lets the monitor run until it waits again.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_changes_is_one_refresh_two_seconds_after_its_first() {
    let host = FakeHost::with(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    let (manager, changes, task) = start_monitor(&host);
    let booted = host.reads();

    host.set(vec![
        interface("192.168.1.2", 24, "eth0", 2),
        interface("10.8.0.5", 24, "tun0", 7),
    ]);
    changes.send(NetworkChange::Availability).expect("send");
    changes.send(NetworkChange::Address).expect("send");
    settle().await;
    tokio::time::advance(Duration::from_millis(1500)).await;
    changes.send(NetworkChange::Address).expect("send");
    settle().await;
    tokio::time::advance(Duration::from_millis(499)).await;
    settle().await;
    assert_eq!(host.reads(), booted, "nothing is read before the 2 s wait");
    assert_eq!(
        bind_addresses(&manager.read().expect("manager")),
        addresses(&["192.168.1.2"])
    );

    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(host.reads(), booted + 1, "the burst is one refresh");
    assert_eq!(
        bind_addresses(&manager.read().expect("manager")),
        addresses(&["192.168.1.2", "10.8.0.5"])
    );

    // A later change waits its own 2 s.
    host.set(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    changes.send(NetworkChange::Address).expect("send");
    settle().await;
    tokio::time::advance(NETWORK_CHANGE_DELAY).await;
    settle().await;
    assert_eq!(host.reads(), booted + 2);
    assert_eq!(
        bind_addresses(&manager.read().expect("manager")),
        addresses(&["192.168.1.2"])
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_failed_source_still_refreshes_its_last_burst_then_stops() {
    let host = FakeHost::with(vec![interface("192.168.1.2", 24, "eth0", 2)]);
    let (manager, changes, task) = start_monitor(&host);
    let booted = host.reads();

    host.set(vec![interface("10.8.0.5", 24, "tun0", 7)]);
    changes.send(NetworkChange::Address).expect("send");
    drop(changes);
    settle().await;
    assert_eq!(host.reads(), booted);
    tokio::time::advance(NETWORK_CHANGE_DELAY).await;
    task.await.expect("the monitor ends when its source fails");
    assert_eq!(host.reads(), booted + 1);
    assert_eq!(
        bind_addresses(&manager.read().expect("manager")),
        addresses(&["10.8.0.5"])
    );
}

#[tokio::test(start_paused = true)]
async fn a_source_that_fails_at_once_ends_the_monitor_without_a_refresh() {
    let host = FakeHost::default();
    let (_manager, changes, task) = start_monitor(&host);
    let booted = host.reads();
    drop(changes);
    task.await.expect("the monitor ends");
    assert_eq!(host.reads(), booted);
}
