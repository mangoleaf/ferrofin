//! `xbmcmetadata.json` as Jellyfin's `config/xbmcmetadata.xml` (`XbmcMetadataOptions`).

use super::Document;

/// The NFO reader and saver options.
pub(super) const DOCUMENT: Document = Document {
    file: "config/xbmcmetadata.xml",
    root: "XbmcMetadataOptions",
    written_as_default: &[],
    ferrofin_only: &[],
    entries: &[],
};
