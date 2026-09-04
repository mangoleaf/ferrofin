//! [`FerrofinFileSystem`] — the concrete [`FileSystem`] over `std::fs`.
//!
//! Port of the `EnvironmentController`-facing subset of
//! `Emby.Server.Implementations.IO.ManagedFileSystem`: enumerate a directory's
//! entries, list the root mounts, and validate a path. Only the browse surface
//! is ported (stream helpers, shortcut resolution, and metadata caching are
//! server-side plumbing outside this batch).
//!
//! Port notes:
//! - `GetDrives` is a port of `ManagedFileSystem.GetDrives`
//!   (`Emby.Server.Implementations/IO/ManagedFileSystem.cs`): every mount
//!   `DriveInfo.GetDrives()` reports, kept when its `DriveType` is
//!   `Fixed | Network | Removable`, it `IsReady` (on Unix: the mount point is a
//!   directory) and its `TotalSize != 0`. Order is `getmntent` order — never
//!   sorted, never de-duplicated. See [`crate::mount_table`] for the filesystem
//!   classification table. Windows lettered-volume enumeration is a separate
//!   platform concern; the Linux path is fully ported.
//! - Enumeration errors are swallowed to an empty list, matching the controller
//!   (`try { … } catch { return Array.Empty }`).
//! - `ValidatePath`'s writable check writes a `Guid`-named throwaway file and
//!   deletes it (C# `File.WriteAllText` in a `finally` cleanup).

use std::fs;
use std::path::Path;

use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};

use crate::mount_table::{self, MountEntry};
use uuid::Uuid;

/// The concrete `std::fs`-backed filesystem browser.
#[derive(Debug, Clone, Copy, Default)]
pub struct FerrofinFileSystem;

impl FerrofinFileSystem {
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

/// The filesystem root, the fallback entry when the mount table cannot be read.
fn root_drive() -> FileSystemEntryInfo {
    FileSystemEntryInfo {
        name: "/".to_owned(),
        path: "/".to_owned(),
        type_: FileSystemEntryType::Directory,
    }
}

/// Applies `ManagedFileSystem.GetDrives`' three filters to a parsed mount table.
///
/// `is_dir` stands in for `DriveInfo.IsReady`, which on Unix is
/// `Directory.Exists(Name)` — this is what drops Docker's `/etc/hosts`-style
/// single-file bind mounts. `total_size` stands in for `DriveInfo.TotalSize`,
/// whose `!= 0` clause drops mounts with no capacity. Both are injected so the
/// rule is unit-testable without a real mount table.
///
/// The order of the mount table is preserved: C# enumerates `getmntent` output
/// and never sorts. `Name` and `FullName` are both the mount point on Unix
/// (`DriveInfo.Name` is the normalized ctor argument and
/// `RootDirectory.FullName` resolves to the same string).
fn drives_from(
    mounts: &[MountEntry],
    is_dir: &dyn Fn(&str) -> bool,
    total_size: &dyn Fn(&str) -> u64,
) -> Vec<FileSystemEntryInfo> {
    mounts
        .iter()
        .filter(|m| mount_table::drive_type(&m.fs_type).is_browsable_drive())
        .filter(|m| is_dir(&m.target))
        .filter(|m| total_size(&m.target) != 0)
        .map(|m| FileSystemEntryInfo {
            name: m.target.clone(),
            path: m.target.clone(),
            type_: FileSystemEntryType::Directory,
        })
        .collect()
}

/// Total bytes of the filesystem at `path` (C# `DriveInfo.TotalSize`), or `0`
/// when it cannot be queried — which is also what the `!= 0` filter drops.
#[cfg(unix)]
fn total_size(path: &str) -> u64 {
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = std::ffi::CString::new(Path::new(path).as_os_str().as_bytes()) else {
        return 0;
    };
    // SAFETY: `stat` is fully written by `statvfs` before it is read, and
    // `c_path` is a valid NUL-terminated C string that outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), std::ptr::from_mut(&mut stat)) } != 0 {
        return 0;
    }
    // statvfs field widths differ per platform (u64 on Linux, u32 on macOS).
    #[allow(clippy::useless_conversion)]
    u64::from(stat.f_blocks).saturating_mul(u64::from(stat.f_frsize))
}

/// Non-unix fallback: no `statvfs`, so the size filter cannot reject anything.
#[cfg(not(unix))]
fn total_size(_path: &str) -> u64 {
    u64::MAX
}

impl FileSystem for FerrofinFileSystem {
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
        let mounts = mount_table::read_mount_table();
        if mounts.is_empty() {
            // No readable mount table (no /proc, no /etc/mtab). The filesystem
            // root is always a drive, so the directory browser is never empty.
            return vec![root_drive()];
        }
        let drives = drives_from(&mounts, &|p| Path::new(p).is_dir(), &total_size);
        if drives.is_empty() {
            vec![root_drive()]
        } else {
            drives
        }
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

    /// The non-pseudo rows of `ferrofin-dv4-jellyfin-1:/proc/mounts`, verbatim.
    /// Jellyfin's live `GET /Environment/Drives` on this container returned
    /// exactly the six directory mounts below, in this order.
    const LIVE_MOUNTS: &str = "\
overlay / overlay rw,lowerdir=/a:/b 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
tmpfs /dev tmpfs rw,nosuid,size=65536k,mode=755 0 0
devpts /dev/pts devpts rw,nosuid,noexec,relatime 0 0
sysfs /sys sysfs ro,nosuid,nodev,noexec,relatime 0 0
cgroup /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime 0 0
mqueue /dev/mqueue mqueue rw,nosuid,nodev,noexec,relatime 0 0
shm /dev/shm tmpfs rw,nosuid,nodev,noexec,relatime,size=65536k 0 0
/dev/mapper/root /config btrfs rw,relatime,ssd 0 0
/dev/mapper/root /cache btrfs rw,relatime,ssd 0 0
/dev/mapper/root /media/tv-real btrfs rw,relatime,ssd 0 0
/dev/mapper/root /media/synth btrfs rw,relatime,ssd 0 0
/dev/mapper/root /media/movies-real btrfs rw,relatime,ssd 0 0
/dev/mapper/root /etc/resolv.conf btrfs rw,relatime,ssd 0 0
/dev/mapper/root /etc/hostname btrfs rw,relatime,ssd 0 0
/dev/mapper/root /etc/hosts btrfs rw,relatime,ssd 0 0
";

    /// The `/etc/*` rows are single-file bind mounts, so `IsReady` (a directory
    /// test on Unix) rejects them; everything else in the fixture is a directory.
    fn fake_is_dir(path: &str) -> bool {
        !path.starts_with("/etc/")
    }

    #[test]
    fn get_drives_reproduces_jellyfins_live_answer() {
        let mounts = crate::mount_table::parse_mount_table(LIVE_MOUNTS);
        let drives = drives_from(&mounts, &fake_is_dir, &|_| 4_000_000_000_000);
        let paths: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/",
                "/config",
                "/cache",
                "/media/tv-real",
                "/media/synth",
                "/media/movies-real",
            ],
            "pseudo filesystems and file bind-mounts dropped, getmntent order kept"
        );
        // C# passes d.Name and d.RootDirectory.FullName, both the mount point.
        assert!(drives.iter().all(|d| d.name == d.path));
        assert!(
            drives
                .iter()
                .all(|d| d.type_ == FileSystemEntryType::Directory)
        );
    }

    #[test]
    fn get_drives_drops_zero_total_size_mounts() {
        let mounts = crate::mount_table::parse_mount_table(LIVE_MOUNTS);
        // C# `d.TotalSize != 0`: "some drives on linux have no actual size".
        let drives = drives_from(&mounts, &fake_is_dir, &|p| {
            if p == "/cache" { 0 } else { 1024 }
        });
        let paths: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        assert!(!paths.contains(&"/cache"));
        assert_eq!(paths.len(), 5);
    }

    #[test]
    fn get_drives_keeps_network_and_removable_but_not_ram_or_cdrom() {
        let mounts = crate::mount_table::parse_mount_table(
            "srv:/e /mnt/nas nfs4 rw 0 0
             /dev/sdb1 /mnt/stick vfat rw 0 0
             /dev/sr0 /mnt/disc iso9660 ro 0 0
             tmpfs /mnt/ram tmpfs rw 0 0
",
        );
        let drives = drives_from(&mounts, &|_| true, &|_| 1024);
        let paths: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["/mnt/nas", "/mnt/stick"]);
    }

    #[test]
    fn get_drives_on_this_host_lists_real_directory_mounts() {
        // The live call: never empty, every entry an existing directory, and the
        // root is present (it is a mount on every Unix).
        let drives = FerrofinFileSystem::new().get_drives();
        assert!(!drives.is_empty());
        assert!(drives.iter().all(|d| Path::new(&d.path).is_dir()));
        assert!(drives.iter().all(|d| d.name == d.path));
        assert!(drives.iter().any(|d| d.path == "/"));
    }

    #[test]
    fn lists_files_and_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("a.txt"), b"hi").expect("write");

        let fs = FerrofinFileSystem::new();
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

        let fs = FerrofinFileSystem::new();
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
        let fs = FerrofinFileSystem::new();
        assert!(fs.directory_exists(&tmp.path().to_string_lossy()));
        fs.validate_writable(&tmp.path().to_string_lossy())
            .expect("writable");

        let err = fs
            .read_file("/no/such/file/here")
            .expect_err("should be missing");
        assert!(matches!(err, ServiceError::NotFound(_)));
    }
}
