//! The directory-listing abstraction the local-image providers scan through.
//!
//! Port of the two file-listing surfaces the C# image providers depend on:
//! `MediaBrowser.Controller.Providers.IDirectoryService` (directory enumeration)
//! and the subset of `MediaBrowser.Model.IO.IFileSystem.GetFiles` they call for
//! image-extension-filtered listings. Both are folded into one small
//! [`DirectoryService`] trait so the un-mockable filesystem I/O lives behind a
//! seam: production uses [`FsDirectoryService`] (real `std::fs`), and the unit
//! tests drive an in-memory / temp-dir fake.

use std::path::Path;

use crate::container_types::FileSystemMetadata;

/// The image extensions the local-image providers recognise.
///
/// Port of `BaseItem.SupportedImageExtensions`
/// (`.png .jpg .jpeg .webp .tbn .gif .svg`). Order is significant: the providers
/// stable-sort candidate files by each file's index in this array, so a `.png`
/// is preferred over a `.jpg` of the same name, etc.
pub const SUPPORTED_IMAGE_EXTENSIONS: [&str; 7] =
    [".png", ".jpg", ".jpeg", ".webp", ".tbn", ".gif", ".svg"];

/// Returns the index of `extension` (leading dot, any case) in
/// [`SUPPORTED_IMAGE_EXTENSIONS`], or `usize::MAX` when it is not a supported
/// image extension.
///
/// Port of `Array.IndexOf(BaseItem.SupportedImageExtensions, ext)`, which
/// returns `-1` for a miss; the providers stable-sort by this value, so a miss
/// sorts last (hence `usize::MAX`).
#[must_use]
pub fn supported_image_extension_index(extension: &str) -> usize {
    SUPPORTED_IMAGE_EXTENSIONS
        .iter()
        .position(|e| e.eq_ignore_ascii_case(extension))
        .unwrap_or(usize::MAX)
}

/// Returns whether `extension` (leading dot, any case) is a supported image
/// extension.
///
/// Port of
/// `BaseItem.SupportedImageExtensions.Contains(ext, StringComparison.OrdinalIgnoreCase)`.
#[must_use]
pub fn is_supported_image_extension(extension: &str) -> bool {
    SUPPORTED_IMAGE_EXTENSIONS
        .iter()
        .any(|e| e.eq_ignore_ascii_case(extension))
}

/// The directory-listing seam the local-image providers scan through.
///
/// Port of the union of `IDirectoryService` (folder enumeration) and the
/// image-filtered `IFileSystem.GetFiles` calls the providers make. All methods
/// take absolute paths and must tolerate a non-existent path by returning an
/// empty list (the C# providers guard every call site with a `Directory.Exists`
/// check and treat a missing directory as "no images").
pub trait DirectoryService {
    /// Lists every entry (files *and* subdirectories) directly under `path`.
    ///
    /// Port of `IDirectoryService.GetFileSystemEntries(path)`. Returns an empty
    /// list when `path` does not exist or is not a directory.
    fn file_system_entries(&self, path: &str) -> Vec<FileSystemMetadata>;

    /// Lists the files (not subdirectories) directly under `path`.
    ///
    /// Port of `IDirectoryService.GetFiles(path)`. Returns an empty list when
    /// `path` does not exist or is not a directory.
    fn files(&self, path: &str) -> Vec<FileSystemMetadata> {
        self.file_system_entries(path)
            .into_iter()
            .filter(|e| !e.is_directory)
            .collect()
    }

    /// Lists the subdirectories directly under `path`.
    ///
    /// Port of `IDirectoryService.GetDirectories(path)`. Returns an empty list
    /// when `path` does not exist or is not a directory.
    fn directories(&self, path: &str) -> Vec<FileSystemMetadata> {
        self.file_system_entries(path)
            .into_iter()
            .filter(|e| e.is_directory)
            .collect()
    }

    /// Lists the image files under `path`, filtered to
    /// [`SUPPORTED_IMAGE_EXTENSIONS`], optionally recursing into subdirectories.
    ///
    /// Port of `IFileSystem.GetFiles(path, SupportedImageExtensions, true, recursive)`.
    /// Returns an empty list when `path` does not exist.
    fn image_files(&self, path: &str, recursive: bool) -> Vec<FileSystemMetadata> {
        let mut out: Vec<FileSystemMetadata> = Vec::new();
        for entry in self.file_system_entries(path) {
            if entry.is_directory {
                if recursive {
                    out.extend(self.image_files(&entry.full_name, recursive));
                }
                continue;
            }
            if entry
                .extension
                .as_deref()
                .is_some_and(is_supported_image_extension)
            {
                out.push(entry);
            }
        }
        out
    }
}

/// A [`DirectoryService`] backed by the real filesystem via `std::fs`.
///
/// Port of the concrete `DirectoryService` / `ManagedFileSystem` pairing: it
/// simply reads directories off disk. This is the only place real filesystem
/// I/O happens, so the parity/coverage numbers exercise [`DirectoryService`]'s
/// default logic through an in-memory fake instead.
#[derive(Debug, Default, Clone, Copy)]
pub struct FsDirectoryService;

impl FsDirectoryService {
    /// Creates a filesystem-backed directory service.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl DirectoryService for FsDirectoryService {
    fn file_system_entries(&self, path: &str) -> Vec<FileSystemMetadata> {
        let Ok(read_dir) = std::fs::read_dir(path) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for entry in read_dir.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let full_path = entry.path();
            out.push(fs_metadata_from(&full_path, &metadata));
        }
        out
    }
}

/// Builds a [`FileSystemMetadata`] from a path and its `std::fs::Metadata`.
///
/// Mirrors how `ManagedFileSystem.GetFileSystemInfo` fills the DTO: `Extension`
/// carries the leading dot, `Name` is the final path component, `Length` is the
/// byte size (directories report `0`).
fn fs_metadata_from(path: &Path, metadata: &std::fs::Metadata) -> FileSystemMetadata {
    let is_directory = metadata.is_dir();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()));
    #[allow(clippy::cast_possible_wrap)]
    let length = if is_directory {
        0
    } else {
        metadata.len() as i64
    };
    FileSystemMetadata {
        exists: true,
        full_name: path.to_string_lossy().into_owned(),
        name,
        extension,
        length,
        last_write_time_utc: None,
        creation_time_utc: None,
        is_directory,
    }
}
