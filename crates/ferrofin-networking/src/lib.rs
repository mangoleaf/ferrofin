//! Network/bind/published-URL resolution for Ferrofin — port of `Jellyfin.Networking`.
//!
//! Modules mirror the C# namespaces they came from:
//!
//! - [`net_constants`] / [`net_utils`] — `MediaBrowser.Common.Net`
//!   (`NetworkConstants` / `NetworkUtils`): RFC address ranges and the pure
//!   IP/subnet math (CIDR parsing, containment, mask↔CIDR, FQDN/host parsing).
//! - [`network_configuration`] — `MediaBrowser.Common.Net.NetworkConfiguration`
//!   (the settings DTO, plus its store key).
//! - [`remote_access_policy_result`] —
//!   `MediaBrowser.Common.Net.RemoteAccessPolicyResult`.
//! - [`manager`] — `Jellyfin.Networking.Manager.NetworkManager`, the
//!   bind-address / published-URL resolver (First-Light target).
//! - [`config_keys`] — the two startup keys inlined from
//!   `MediaBrowser.Controller.Extensions.ConfigurationExtensions`.
//! - [`logger`] — a minimal `ILogger` seam so the ported warning-substring
//!   tests stay deterministic without pulling in `tracing`.
//!
//! The [`ferrofin_model::net`] value types (`IpData`, `IpNetwork`,
//! `AddressFamily`, `PublishedServerUriOverride`) are reused, not redefined.
//!
//! Deferred: the UDP `AutoDiscoveryHost`, the
//! Happy-Eyeballs `HttpClientExtension` (reqwest does this itself), the UDP
//! `SocketFactory`, OS network-change event wiring, and live interface
//! enumeration.

pub mod config_keys;
pub mod error;
pub mod logger;
pub mod manager;
pub mod net_constants;
pub mod net_utils;
pub mod network_configuration;
pub mod remote_access_policy_result;

pub use error::NetworkingError;
pub use logger::{Logger, NullLogger};
pub use manager::{NetworkManager, StartupConfig};
pub use network_configuration::NetworkConfiguration;
pub use remote_access_policy_result::RemoteAccessPolicyResult;
