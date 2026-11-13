//! Network/bind/published-URL resolution for Ferrofin — port of `Jellyfin.Networking`.
//!
//! Modules mirror the C# namespaces they came from:
//!
//! - [`net_constants`] / [`net_utils`] — `MediaBrowser.Common.Net`
//!   (`NetworkConstants` / `NetworkUtils`): RFC address ranges and the pure
//!   IP/subnet math (CIDR parsing, containment, FQDN/host parsing).
//! - [`network_configuration`] — `MediaBrowser.Common.Net.NetworkConfiguration`
//!   (the settings DTO).
//! - [`remote_access_policy_result`] —
//!   `MediaBrowser.Common.Net.RemoteAccessPolicyResult`.
//! - [`manager`] — `Jellyfin.Networking.Manager.NetworkManager`, the
//!   bind-address / published-URL resolver (First-Light target).
//! - [`network_change`] — the OS network-change notifications
//!   (`System.Net.NetworkInformation.NetworkChange`) and the monitor that
//!   refreshes the manager after them (`HandleNetworkChange`).
//! - [`config_keys`] — the startup key inlined from
//!   `MediaBrowser.Controller.Extensions.ConfigurationExtensions`.
//! - [`logger`] — a minimal `ILogger` seam so the ported warning-substring
//!   tests stay deterministic; [`TracingLogger`] writes to the server log.
//!
//! The [`ferrofin_model::net`] value types (`IpData`, `IpNetwork`,
//! `AddressFamily`, `PublishedServerUriOverride`) are reused, not redefined.
//!
//! Live interface snapshots feed address selection; the UDP responder is in
//! `ferrofin-core`. HTTP connection racing is handled by reqwest.

pub mod config_keys;
mod interfaces;
pub mod known_proxies;
pub mod logger;
#[cfg(target_os = "macos")]
mod macos_interfaces;
pub mod manager;
pub mod net_constants;
pub mod net_utils;
pub mod network_change;
pub mod network_configuration;
pub mod remote_access_policy_result;

pub use known_proxies::KnownProxies;
pub use logger::{Logger, NullLogger, TracingLogger};
pub use manager::{
    HostLookup, InterfaceEnumerator, NetworkManager, StartupConfig, published_urls_missing_base_url,
};
pub use network_configuration::{NetworkConfiguration, normalize_base_url};
pub use remote_access_policy_result::RemoteAccessPolicyResult;
