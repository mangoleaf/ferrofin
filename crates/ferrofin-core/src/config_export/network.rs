//! `network.json` as Jellyfin's `config/network.xml` (`NetworkConfiguration`).

use super::Document;
use crate::config_import::NETWORK_XML_DENY;

/// Where Jellyfin reads it, relative to its program data directory.
pub(super) const FILE: &str = "config/network.xml";

/// The ports, subnets, proxies and remote-access policy.
pub(super) const DOCUMENT: Document = Document {
    root: "NetworkConfiguration",
    written_as_default: NETWORK_XML_DENY,
    ferrofin_only: &[],
    entries: &[],
};
