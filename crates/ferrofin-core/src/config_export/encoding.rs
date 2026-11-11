//! `encoding.json` as Jellyfin's `config/encoding.xml` (`EncodingOptions`).

use super::Document;
use crate::config_import::ENCODING_XML_DENY;

/// The transcoding options.
pub(super) const DOCUMENT: Document = Document {
    file: "config/encoding.xml",
    root: "EncodingOptions",
    written_as_default: ENCODING_XML_DENY,
    ferrofin_only: &[],
    entries: &[],
};
