//! `system.json` as Jellyfin's `config/system.xml` (`ServerConfiguration`).

use std::path::Path;

use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_traits::error::ServiceError;

use super::Document;
use crate::config_import::SYSTEM_XML_DENY;
use crate::configuration_manager::FerrofinServerConfigurationManager as Manager;

/// Where Jellyfin reads it, relative to its program data directory.
pub(super) const FILE: &str = "config/system.xml";

/// The main server configuration.
pub(super) const DOCUMENT: Document = Document {
    root: "ServerConfiguration",
    written_as_default: SYSTEM_XML_DENY,
    // The 4:4:4 trickplay tiles opt-in (owner decision 2026-11-01, gl-1128).
    ferrofin_only: &["TrickplayOptions.FullChroma"],
    entries: &[
        ("MetadataOptions", "MetadataOptions"),
        ("ContentTypes", "NameValuePair"),
        ("PathSubstitutions", "PathSubstitution"),
        ("CastReceiverApplications", "CastReceiverApplication"),
        ("PluginRepositories", "RepositoryInfo"),
    ],
};

/// Reads `system.json` as the server loads it.
///
/// It must exist: a data directory without it has never been served by
/// Ferrofin, and the server itself refuses one that does not parse.
pub(super) fn read(config_dir: &Path) -> Result<ServerConfiguration, ServiceError> {
    let path = config_dir.join(Manager::CONFIG_FILE_NAME);
    let bytes = std::fs::read(&path)
        .map_err(|e| ServiceError::Backend(format!("read {}: {e}", path.display())))?;
    let mut configuration: ServerConfiguration = serde_json::from_slice(&bytes)
        .map_err(|e| ServiceError::Backend(format!("parse {}: {e}", path.display())))?;
    // The server backfills an empty list at load (`FerrofinServerConfigurationManager::load`),
    // so that is what it serves, and what Jellyfin is given.
    if configuration.metadata_options.is_empty() {
        configuration.metadata_options = crate::configuration_manager::default_metadata_options();
    }
    Ok(configuration)
}
