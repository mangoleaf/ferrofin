//! Filesystem trait — the DI seam over server-side directory browsing.
//!
//! Port of the browse-facing subset of `MediaBrowser.Model.IO.IFileSystem` the
//! `EnvironmentController` uses: enumerate a directory's entries, list the
//! system drives, and validate a path (exists / is-file / writable). The full
//! `IFileSystem` (stream helpers, shortcut handling, metadata) is server-side
//! plumbing and stays out of the trait surface.
//!
//! Methods are synchronous (the controller calls into a blocking filesystem)
//! and the trait is object-safe.

use chrono::{DateTime, Utc};
use ferrofin_model::io::FileSystemEntryInfo;

use crate::error::ServiceError;

/// Metadata for a single file (C# `FileSystemMetadata`, browse subset).
///
/// Carries the fields the `SystemController.GetServerLogs` action projects into
/// a [`LogFile`](ferrofin_model::system::LogFile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    /// The file's base name (no directory component).
    pub name: String,

    /// The file's absolute path.
    pub full_name: String,

    /// The file size in bytes.
    pub length: i64,

    /// When the file was created (UTC).
    pub date_created: DateTime<Utc>,

    /// When the file was last written (UTC).
    pub date_modified: DateTime<Utc>,
}

/// Browses the server's local filesystem.
///
/// Port of the `EnvironmentController`-facing members of `IFileSystem`.
pub trait FileSystem: Send + Sync {
    /// Lists the direct children (files and directories) of a path
    /// (C# `IFileSystem.GetFileSystemEntries`). Returns an empty list — not an
    /// error — when the path cannot be read, matching the controller's
    /// swallow-and-return behaviour.
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo>;

    /// [`get_file_system_entries`](Self::get_file_system_entries), telling a
    /// directory that could not be listed (a network mount that dropped, a
    /// permission error, an entry that failed to read) apart from an empty
    /// one — what the library scan needs so that a listing failure never
    /// reads as every file under it being gone (`Folder.ValidateChildren`
    /// stops on an `IOException` rather than removing children).
    ///
    /// The default reports every listing as successful.
    ///
    /// # Errors
    ///
    /// [`ServiceError::Backend`] when the directory could not be listed
    /// completely.
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        Ok(self.get_file_system_entries(path))
    }

    /// Lists the available drives / root mounts (C# `IFileSystem.GetDrives`).
    fn get_drives(&self) -> Vec<FileSystemEntryInfo>;

    /// Whether a file or a directory exists at `path`. The default asks
    /// [`file_exists`](Self::file_exists), then
    /// [`directory_exists`](Self::directory_exists); a real filesystem
    /// answers with one stat.
    fn path_exists(&self, path: &str) -> bool {
        self.file_exists(path) || self.directory_exists(path)
    }

    /// Whether a regular file exists at the path.
    fn file_exists(&self, path: &str) -> bool;

    /// Whether a directory exists at the path.
    fn directory_exists(&self, path: &str) -> bool;

    /// Attempts to prove a directory is writable by creating and deleting a
    /// throwaway file (C# `EnvironmentController.ValidatePath` writable check).
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the throwaway file cannot be created (i.e. the
    /// directory is not writable).
    fn validate_writable(&self, path: &str) -> Result<(), ServiceError>;

    /// Lists the files in a directory whose extension matches one of
    /// `extensions` (each like `".log"`; empty means "all"), with metadata
    /// (C# `IFileSystem.GetFiles`). Returns an empty list when the directory
    /// cannot be read.
    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata>;

    /// Reads a file's full contents (C# `SystemController.GetLogFile` file
    /// stream). `404`-worthy absence surfaces as [`ServiceError::not_found`].
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::not_found`] if the file does not exist, or a
    /// backend [`ServiceError`] for any other read failure.
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError>;
}

fn _assert_object_safe_file_system(_: &dyn FileSystem) {}
