//! Port of the wire-facing DTOs in `MediaBrowser.Model.IO`.
//!
//! Only [`FileSystemEntryInfo`] and [`FileSystemEntryType`] are part of the JSON
//! contract (exposed by the environment endpoints). The rest of the namespace —
//! `IFileSystem`, `IShortcutHandler`, `IStreamHelper`, `AsyncFile`,
//! `FileSystemMetadata` and the `IODefaults` buffer-size constants — is
//! server-side filesystem plumbing and is dropped from this port.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The type of a file system entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum FileSystemEntryType {
    /// A file.
    #[default]
    File,
    /// A directory.
    Directory,
    /// A network computer.
    NetworkComputer,
    /// A network share.
    NetworkShare,
}

/// Information about a single file system entry.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct FileSystemEntryInfo {
    /// Gets the name.
    pub name: String,

    /// Gets the path.
    pub path: String,

    /// Gets the type.
    #[serde(rename = "Type")]
    pub type_: FileSystemEntryType,
}
