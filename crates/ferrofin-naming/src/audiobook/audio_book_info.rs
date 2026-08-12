//! Port of `Emby.Naming.AudioBook.AudioBookInfo`.

use crate::audiobook::AudioBookFileInfo;

/// Represents a complete audiobook, including all parts and extras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioBookInfo {
    /// The name.
    pub name: String,
    /// The year.
    pub year: Option<i32>,
    /// The files composing the actual audiobook.
    pub files: Vec<AudioBookFileInfo>,
    /// The extra files.
    pub extras: Vec<AudioBookFileInfo>,
    /// The alternate versions.
    pub alternate_versions: Vec<AudioBookFileInfo>,
}

impl AudioBookInfo {
    /// Creates a new [`AudioBookInfo`].
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        year: Option<i32>,
        files: Vec<AudioBookFileInfo>,
        extras: Vec<AudioBookFileInfo>,
        alternate_versions: Vec<AudioBookFileInfo>,
    ) -> Self {
        Self {
            name: name.into(),
            year,
            files,
            extras,
            alternate_versions,
        }
    }
}
