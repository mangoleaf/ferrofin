//! `network.json` as Jellyfin's `config/network.xml` (`NetworkConfiguration`).

use super::Document;
use crate::config_import::NETWORK_XML_DENY;

/// The ports, subnets, proxies and remote-access policy.
pub(super) const DOCUMENT: Document = Document {
    file: "config/network.xml",
    root: "NetworkConfiguration",
    written_as_default: NETWORK_XML_DENY,
    ferrofin_only: &[],
    entries: &[],
};
