//! `metadata.json` as Jellyfin's `config/metadata.xml` (`MetadataConfiguration`).

use super::Document;

/// How new content is dated.
pub(super) const DOCUMENT: Document = Document {
    file: "config/metadata.xml",
    root: "MetadataConfiguration",
    written_as_default: &[],
    ferrofin_only: &[],
    entries: &[],
};
