//! Local port of `MediaBrowser.Model.IO.FileSystemMetadata`.
//!
//! The naming resolvers accept a lightweight file-system POCO with no actual
//! I/O; only `FullName` and `IsDirectory` are consulted. `ferrofin-model` does
//! not expose this exact type, so we define the minimal shape here.

use crate::path;

/// Minimal file-system metadata carrier used by the list resolvers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSystemMetadata {
    /// The full path of the entry.
    pub full_name: String,
    /// Whether the entry is a directory.
    pub is_directory: bool,
}

impl FileSystemMetadata {
    /// Creates a new [`FileSystemMetadata`].
    #[must_use]
    pub fn new(full_name: impl Into<String>, is_directory: bool) -> Self {
        Self {
            full_name: full_name.into(),
            is_directory,
        }
    }

    /// Returns the leaf name of [`Self::full_name`] (mirrors `.NET` `Name`).
    #[must_use]
    pub fn name(&self) -> &str {
        path::file_name(&self.full_name)
    }
}
