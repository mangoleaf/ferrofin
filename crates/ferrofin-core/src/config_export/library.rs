//! A library's `options.json` as Jellyfin's `root/default/<library>/options.xml`
//! (`LibraryOptions`, saved by `CollectionFolder.SaveLibraryOptions`).

use super::Document;

/// A library's options. All of them cross: the import of `options.xml` keeps
/// every member, so the export does too.
pub(super) const DOCUMENT: Document = Document {
    root: "LibraryOptions",
    written_as_default: &[],
    ferrofin_only: &[],
    entries: &[
        ("PathInfos", "MediaPathInfo"),
        ("TypeOptions", "TypeOptions"),
        ("ImageOptions", "ImageOption"),
    ],
};
