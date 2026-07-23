//! [`HermitFileSystem`] — the concrete [`FileSystem`] over `std::fs`.
//!
//! Port of the `EnvironmentController`-facing subset of
//! `Emby.Server.Implementations.IO.ManagedFileSystem`: enumerate a directory's
//! entries, list the root mounts, and validate a path. Only the browse surface
//! is ported (stream helpers, shortcut resolution, and metadata caching are
//! server-side plumbing outside this batch).
//!
//! Port notes:
//! - `GetDrives` has no portable cross-platform primitive in `std`. On Unix the
//!   single filesystem root (`/`) is the one "drive"; the Windows enumeration of
//!   lettered volumes is out of scope for the Linux target and would be added in
//!   a platform module. This keeps the endpoint honest rather than faking a list.
//! - Enumeration errors are swallowed to an empty list, matching the controller
//!   (`try { … } catch { return Array.Empty }`).
//! - `ValidatePath`'s writable check writes a `Guid`-named throwaway file and
//!   deletes it (C# `File.WriteAllText` in a `finally` cleanup).

use std::fs;
use std::path::Path;

use hermit_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use hermit_traits::error::ServiceError;
use hermit_traits::filesystem::{FileMetadata, FileSystem};
use uuid::Uuid;

/// The concrete `std::fs`-backed filesystem browser.
#[derive(Debug, Clone, Copy, Default)]
pub struct HermitFileSystem;

impl HermitFileSystem {
    /// Creates a filesystem browser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Converts a `std::fs::Metadata` modification/creation time to a UTC datetime,
/// falling back to the Unix epoch when the platform does not expose it.
fn system_time_to_utc(
    time: std::io::Result<std::time::SystemTime>,
) -> chrono::DateTime<chrono::Utc> {
    time.ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or_default()
}

impl FileSystem for HermitFileSystem {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        let Ok(read_dir) = fs::read_dir(path) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for entry in read_dir.flatten() {
            let full_path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            entries.push(FileSystemEntryInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: full_path.to_string_lossy().into_owned(),
                type_: if is_dir {
                    FileSystemEntryType::Directory
                } else {
                    FileSystemEntryType::File
                },
            });
        }
        entries
    }

    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        // The Unix filesystem root is the single browsable mount; a Windows
        // volume enumeration is a platform concern deferred to the server layer.
        vec![FileSystemEntryInfo {
            name: "/".to_owned(),
            path: "/".to_owned(),
            type_: FileSystemEntryType::Directory,
        }]
    }

    fn file_exists(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn directory_exists(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }

    fn validate_writable(&self, path: &str) -> Result<(), ServiceError> {
        let probe = Path::new(path).join(Uuid::new_v4().to_string());
        let result = fs::write(&probe, b"");
        // Always attempt cleanup (C# `finally`), then surface the write result.
        if probe.exists() {
            let _ = fs::remove_file(&probe);
        }
        result.map_err(|e| ServiceError::InvalidInput(format!("path is not writable: {e}")))
    }

    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        let Ok(read_dir) = fs::read_dir(path) else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for entry in read_dir.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let full_path = entry.path();
            if !extensions.is_empty() {
                let matches = full_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| {
                        let dotted = format!(".{}", ext.to_ascii_lowercase());
                        extensions.iter().any(|e| e.eq_ignore_ascii_case(&dotted))
                    });
                if !matches {
                    continue;
                }
            }
            files.push(FileMetadata {
                name: entry.file_name().to_string_lossy().into_owned(),
                full_name: full_path.to_string_lossy().into_owned(),
                length: i64::try_from(meta.len()).unwrap_or(i64::MAX),
                date_created: system_time_to_utc(meta.created()),
                date_modified: system_time_to_utc(meta.modified()),
            });
        }
        files
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        fs::read(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ServiceError::not_found(format!("file not found: {path}"))
            } else {
                ServiceError::Backend(format!("read file ({path}): {e}"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_files_and_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("a.txt"), b"hi").expect("write");

        let fs = HermitFileSystem::new();
        let mut entries = fs.get_file_system_entries(&tmp.path().to_string_lossy());
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].type_, FileSystemEntryType::File);
        assert_eq!(entries[1].name, "sub");
        assert_eq!(entries[1].type_, FileSystemEntryType::Directory);
    }

    #[test]
    fn get_files_filters_by_extension_and_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("server.log"), b"log-body").expect("write");
        std::fs::write(tmp.path().join("notes.md"), b"nope").expect("write");

        let fs = HermitFileSystem::new();
        let logs = fs.get_files(&tmp.path().to_string_lossy(), &[".log", ".txt"]);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].name, "server.log");
        assert_eq!(logs[0].length, 8);

        let body = fs.read_file(&logs[0].full_name).expect("read");
        assert_eq!(body, b"log-body");
    }

    #[test]
    fn validate_writable_ok_and_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fs = HermitFileSystem::new();
        assert!(fs.directory_exists(&tmp.path().to_string_lossy()));
        fs.validate_writable(&tmp.path().to_string_lossy())
            .expect("writable");

        let err = fs
            .read_file("/no/such/file/here")
            .expect_err("should be missing");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }
}
