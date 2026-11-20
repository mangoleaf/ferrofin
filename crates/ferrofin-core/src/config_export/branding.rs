//! `branding.json` as Jellyfin's `config/branding.xml` (`BrandingOptions`).

use super::Document;
use crate::config_import::BRANDING_XML_DENY;

/// Where Jellyfin reads it, relative to its program data directory.
pub(super) const FILE: &str = "config/branding.xml";

/// The login disclaimer, custom CSS and splash screen.
pub(super) const DOCUMENT: Document = Document {
    root: "BrandingOptions",
    written_as_default: BRANDING_XML_DENY,
    ferrofin_only: &[],
    entries: &[],
};
