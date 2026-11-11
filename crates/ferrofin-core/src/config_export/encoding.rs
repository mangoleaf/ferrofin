//! `encoding.json` as Jellyfin's `config/encoding.xml` (`EncodingOptions`).

use super::Document;
use crate::config_import::ENCODING_XML_DENY;

/// Where Jellyfin reads it, relative to its program data directory.
pub(super) const FILE: &str = "config/encoding.xml";

/// The transcoding options.
pub(super) const DOCUMENT: Document = Document {
    root: "EncodingOptions",
    written_as_default: ENCODING_XML_DENY,
    ferrofin_only: &[],
    entries: &[],
};
