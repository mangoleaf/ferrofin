//! Port of the portable types in `MediaBrowser.Model.Net`.
//!
//! `MimeTypes` (extension↔mime lookup tables), `IPData`/`IpNetwork`,
//! `EndPointInfo`, and `PublishedServerUriOverride`. The socket-factory
//! interfaces are runtime concerns and are not ported here.

mod end_point_info;
mod ip_data;
pub mod mime_types;
mod published_server_uri_override;

pub use end_point_info::EndPointInfo;
pub use ip_data::{AddressFamily, IpData, IpNetwork, ipv6_unspecified};
pub use published_server_uri_override::PublishedServerUriOverride;
