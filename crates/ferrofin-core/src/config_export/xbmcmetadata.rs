//! `xbmcmetadata.json` as Jellyfin's `config/xbmcmetadata.xml` (`XbmcMetadataOptions`).

use super::Document;

/// Where Jellyfin reads it, relative to its program data directory.
pub(super) const FILE: &str = "config/xbmcmetadata.xml";

/// The NFO reader and saver options.
pub(super) const DOCUMENT: Document = Document {
    root: "XbmcMetadataOptions",
    written_as_default: &[],
    ferrofin_only: &[],
    entries: &[],
};
