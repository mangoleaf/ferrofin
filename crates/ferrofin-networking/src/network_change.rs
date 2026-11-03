//! Operating-system network-change notifications, and the monitor that
//! refreshes the shared [`NetworkManager`] after them.
//!
//! Port of what Jellyfin's `NetworkManager` does when `DetectNetworkChange`
//! is on: it subscribes to .NET's `NetworkChange.NetworkAddressChanged` and
//! `NetworkAvailabilityChanged`, and each handler logs at debug and calls
//! `HandleNetworkChange`, which waits 2 seconds ("as typically these events
//! fire multiple times in succession") and then runs `OnNetworkChange`
//! ([`NetworkManager::on_network_change`]).
//!
//! The events come from the same operating-system sources .NET 10 reads:
//!
//! - **Linux**: a `NETLINK_ROUTE` socket on .NET's groups, with its message
//!   filter (`pal_networkchange.c`).
//! - **macOS**: a `PF_ROUTE` routing socket, .NET's BSD reader
//!   (`pal_networkchange.c`, `HAVE_RT_MSGHDR`). .NET itself watches
//!   SCDynamicStore's IPv4/IPv6 keys on macOS; the routing socket reports the
//!   same address changes without CoreFoundation.
//! - **Windows**: `NotifyUnicastIpAddressChange` for every family, where .NET
//!   waits on `SIO_ADDRESS_LIST_CHANGE` for IPv4 and IPv6.
//!
//! Elsewhere [`os_source`] fails with [`std::io::ErrorKind::Unsupported`].
//!
//! Upstream's handler sleeps 2 seconds inside a lock on the thread that
//! delivered the event, so a burst of N events runs N refreshes one after the
//! other, 2 seconds apart (its `_eventfire` guard never sees `true` under
//! that lock). [`monitor`] waits the same 2 seconds after the first event of
//! a burst and folds every event that arrives meanwhile into one refresh. A
//! refresh still starts after every event, so the caches end up the same.

use std::io;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use crate::manager::NetworkManager;

#[cfg(target_os = "linux")]
mod netlink;
#[cfg(any(target_os = "macos", test))]
mod route_socket;
#[cfg(windows)]
mod windows;

/// How long a network change waits before the interfaces are read again:
/// upstream's `Thread.Sleep(2000)` in `NetworkManager.OnNetworkChange`.
///
/// FLAGGED as a magic number: it is upstream's literal, and a candidate
/// setting for the owner.
pub const NETWORK_CHANGE_DELAY: Duration = Duration::from_secs(2);

/// What the operating system reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkChange {
    /// An address was added or removed (`NetworkAddressChanged`).
    Address,
    /// A link or a main-table route changed (`NetworkAvailabilityChanged`).
    Availability,
}

/// Where network changes come from: the operating system ([`os_source`]), or
/// a channel in tests.
pub trait NetworkChangeSource: Send {
    /// The next change. Cancel-safe: a change is never lost when the future
    /// is dropped before it completes.
    ///
    /// # Errors
    ///
    /// The source failed and reports no more changes.
    fn recv(&mut self) -> impl Future<Output = io::Result<NetworkChange>> + Send;
}

impl NetworkChangeSource for tokio::sync::mpsc::UnboundedReceiver<NetworkChange> {
    async fn recv(&mut self) -> io::Result<NetworkChange> {
        tokio::sync::mpsc::UnboundedReceiver::recv(self)
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "the change channel closed"))
    }
}

/// The operating system's network changes.
#[cfg(target_os = "linux")]
pub type OsNetworkChanges = netlink::NetlinkChanges;
/// The operating system's network changes.
#[cfg(target_os = "macos")]
pub type OsNetworkChanges = route_socket::RouteSocketChanges;
/// The operating system's network changes.
#[cfg(windows)]
pub type OsNetworkChanges = windows::AddressChangeNotifications;
/// The operating system's network changes: none on this platform.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub type OsNetworkChanges = tokio::sync::mpsc::UnboundedReceiver<NetworkChange>;

/// Starts listening to the operating system's network changes: a netlink
/// socket. Needs a Tokio runtime.
///
/// # Errors
///
/// The socket could not be opened.
#[cfg(target_os = "linux")]
pub fn os_source() -> io::Result<OsNetworkChanges> {
    netlink::NetlinkChanges::open()
}

/// Starts listening to the operating system's network changes: a routing
/// socket. Needs a Tokio runtime.
///
/// # Errors
///
/// The socket could not be opened.
#[cfg(target_os = "macos")]
pub fn os_source() -> io::Result<OsNetworkChanges> {
    route_socket::RouteSocketChanges::open()
}

/// Starts listening to the operating system's network changes: an IP Helper
/// notification. Needs a Tokio runtime.
///
/// # Errors
///
/// The notification could not be registered.
#[cfg(windows)]
pub fn os_source() -> io::Result<OsNetworkChanges> {
    windows::AddressChangeNotifications::open()
}

/// Starts listening to the operating system's network changes, which this
/// platform does not report.
///
/// # Errors
///
/// Always [`io::ErrorKind::Unsupported`].
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn os_source() -> io::Result<OsNetworkChanges> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform reports no network changes",
    ))
}

/// Refreshes `manager` after each burst of changes from `source`
/// (`HandleNetworkChange` → `OnNetworkChange`): `delay` after a burst's first
/// change, and once for every change that arrived meanwhile. Runs until the
/// source fails, which is logged as a warning.
pub async fn monitor<S: NetworkChangeSource>(
    mut source: S,
    manager: Arc<RwLock<NetworkManager>>,
    delay: Duration,
) {
    loop {
        let failure = match source.recv().await {
            Ok(change) => {
                log_change(change);
                let deadline = tokio::time::Instant::now() + delay;
                let failure = fold_until(&mut source, deadline).await;
                tokio::time::sleep_until(deadline).await;
                manager
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .on_network_change();
                failure
            }
            Err(error) => Some(error),
        };
        if let Some(error) = failure {
            tracing::warn!(
                %error,
                "network change detection stopped: network interfaces are read again only \
                 when the network settings are saved or the server restarts"
            );
            return;
        }
    }
}

/// Takes in every change that arrives before `deadline`, returning the
/// source's error if it fails first.
async fn fold_until<S: NetworkChangeSource>(
    source: &mut S,
    deadline: tokio::time::Instant,
) -> Option<io::Error> {
    loop {
        match tokio::time::timeout_at(deadline, source.recv()).await {
            Ok(Ok(change)) => log_change(change),
            Ok(Err(error)) => return Some(error),
            Err(_elapsed) => return None,
        }
    }
}

/// Upstream's per-event debug lines (`OnNetworkAddressChanged`,
/// `OnNetworkAvailabilityChanged`).
fn log_change(change: NetworkChange) {
    match change {
        NetworkChange::Address => tracing::debug!("Network address change detected."),
        NetworkChange::Availability => tracing::debug!("Network availability changed."),
    }
}
