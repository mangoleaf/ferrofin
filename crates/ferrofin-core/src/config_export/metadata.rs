//! `metadata.json` as Jellyfin's `config/metadata.xml` (`MetadataConfiguration`).

use super::Document;

/// Where Jellyfin reads it, relative to its program data directory.
pub(super) const FILE: &str = "config/metadata.xml";

/// How new content is dated.
pub(super) const DOCUMENT: Document = Document {
    root: "MetadataConfiguration",
    written_as_default: &[],
    ferrofin_only: &[],
    entries: &[],
};
