//! Filesystem trait — the DI seam over server-side directory browsing.
//!
//! Port of the browse-facing subset of `MediaBrowser.Model.IO.IFileSystem` the
//! `EnvironmentController` uses: enumerate a directory's entries, list the
//! system drives, and validate a path (exists / is-file / writable). The full
//! `IFileSystem` (stream helpers, shortcut handling, metadata) is server-side
//! plumbing and stays out of the trait surface.
//!
//! **Every method blocks.** The trait is synchronous and object-safe, and a
//! call goes to the disk or the network: on a hard-mounted NFS or CIFS export
//! whose server is gone, it sleeps uninterruptibly until the server returns.
//! A call made inline on a runtime worker takes that worker with it. A handler
//! goes through `AppState::blocking_file_system`, the blocking pool with a cap
//! on the threads a hung mount can hold; the library scan hands the worker's
//! other tasks on first, with `off_runtime`.

use std::io::Read;
use std::sync::Arc;

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

/// A file opened for reading, for a response that streams it.
pub struct OpenedFile {
    /// The path it was opened from, for the log if a read fails.
    pub path: String,

    /// The file's length when it was opened. Stop reading here: a log still
    /// being written is longer by the time the reader gets to its end, and a
    /// response that declared this length cannot carry more.
    pub length: u64,

    /// The file's contents, from the start. Each read blocks, as the methods
    /// of [`FileSystem`] do.
    pub reader: Box<dyn Read + Send>,
}

impl std::fmt::Debug for OpenedFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenedFile")
            .field("path", &self.path)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// Browses the server's local filesystem.
///
/// Port of the `EnvironmentController`-facing members of `IFileSystem`.
///
/// **Every method blocks**, for as long as the disk or the network mount
/// behind the path takes, which is forever on a hung mount. See the
/// [module docs](self) for where a call may run.
pub trait FileSystem: Send + Sync {
    /// Lists the direct children (files and directories) of a path
    /// (C# `IFileSystem.GetFileSystemEntries`). Returns an empty list when the
    /// path cannot be read, for a reader to whom that makes no difference;
    /// [`try_get_file_system_entries`](Self::try_get_file_system_entries) tells
    /// them apart.
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
    /// - [`ServiceError::NotFound`] when `path` is no directory, as
    ///   `DirectoryInfo.EnumerateFileSystemInfos` throws
    ///   `DirectoryNotFoundException`.
    /// - [`ServiceError::Forbidden`] when the server may not read it, which
    ///   upstream's `IgnoreInaccessible` reads as an empty directory.
    /// - [`ServiceError::Backend`] when the directory could not be listed
    ///   completely for any other reason.
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        Ok(self.get_file_system_entries(path))
    }

    /// [`get_file_system_entries`](Self::get_file_system_entries) as a
    /// shared slice, for a reader that only looks through the entries: a
    /// cached listing hands out its own copy without cloning every entry.
    /// The default lists the directory.
    fn shared_file_system_entries(&self, path: &str) -> Arc<[FileSystemEntryInfo]> {
        self.get_file_system_entries(path).into()
    }

    /// Lists the available drives / root mounts (C# `IFileSystem.GetDrives`).
    ///
    /// Unlike every other method, this one returns in bounded time on a real
    /// filesystem: a mount that does not answer is left out of the list.
    fn get_drives(&self) -> Vec<FileSystemEntryInfo>;

    /// Whether a file or a directory exists at `path`. The default asks
    /// [`file_exists`](Self::file_exists), then
    /// [`directory_exists`](Self::directory_exists); a real filesystem
    /// answers with one stat.
    fn path_exists(&self, path: &str) -> bool {
        self.file_exists(path) || self.directory_exists(path)
    }

    /// Whether `a` and `b` name the same file or directory: two spellings of
    /// one entry on a case-insensitive mount, say. Only a real filesystem can
    /// tell; the default answers `false`, "not known to be the same".
    fn same_file(&self, a: &str, b: &str) -> bool {
        let _ = (a, b);
        false
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
    /// [`ServiceError::NotFound`] if `path` is no directory (C#
    /// `DirectoryNotFoundException`), or [`ServiceError::Backend`] if the
    /// throwaway file cannot be created there for any other reason (i.e. the
    /// directory is not writable).
    fn validate_writable(&self, path: &str) -> Result<(), ServiceError>;

    /// Lists the files in a directory whose extension matches one of
    /// `extensions` (each like `".log"`; empty means "all"), with metadata
    /// (C# `IFileSystem.GetFiles`). Returns an empty list when the directory
    /// cannot be read.
    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata>;

    /// Reads a file's full contents into memory. A response never does this:
    /// a log has no size cap, so it goes through
    /// [`open_file`](Self::open_file). `404`-worthy absence surfaces as
    /// [`ServiceError::not_found`].
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::not_found`] if the file does not exist, or a
    /// backend [`ServiceError`] for any other read failure.
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError>;

    /// Opens a file to stream it (C# `SystemController.GetLogFile`'s
    /// `FileStream`, `SubtitleController.GetFallbackFont`'s `PhysicalFile`).
    /// The default serves [`read_file`](Self::read_file)'s bytes; a real
    /// filesystem reads no more than the caller asks for.
    ///
    /// # Errors
    ///
    /// As [`read_file`](Self::read_file).
    fn open_file(&self, path: &str) -> Result<OpenedFile, ServiceError> {
        let bytes = self.read_file(path)?;
        Ok(OpenedFile {
            path: path.to_owned(),
            length: bytes.len() as u64,
            reader: Box::new(std::io::Cursor::new(bytes)),
        })
    }
}

fn _assert_object_safe_file_system(_: &dyn FileSystem) {}
