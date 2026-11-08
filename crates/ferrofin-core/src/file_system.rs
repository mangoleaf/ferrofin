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
//!   platform concern; the Linux path is fully ported. One deliberate
//!   difference: a mount that does not answer in time is left out
//!   (see `drive_probe`).
//! - `IFileSystem.GetFiles` skips only directories, and it gives a link the
//!   length of what it points at: `DirectoryInfo.EnumerateFiles`, and
//!   `ManagedFileSystem.GetFileSystemMetadata` on the entries.
//! - `ValidatePath`'s writable check writes a `Guid`-named throwaway file and
//!   deletes it (C# `File.WriteAllText` in a `finally` cleanup).

use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use ferrofin_model::io::{FileSystemEntryInfo, FileSystemEntryType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem, OpenedFile};

use crate::drive_probe::{DRIVE_PROBE_TIMEOUT, DriveProbes};
use crate::mount_table::{self, MountEntry};
use uuid::Uuid;

/// The process's probes of the mount table. One set for every
/// [`FerrofinFileSystem`] (a unit struct, built where it is needed): the bound
/// of one thread per mount is a bound on the process.
static DRIVE_PROBES: LazyLock<DriveProbes> =
    LazyLock::new(|| DriveProbes::new(DRIVE_PROBE_TIMEOUT));

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

/// When a file was created (C# `FileSystemInfo.CreationTimeUtc`). A filesystem
/// with no birth time (NFS, for one) answers as .NET's `FileStatus` does: with
/// the older of the change and the modification time.
fn creation_time(meta: &fs::Metadata) -> chrono::DateTime<chrono::Utc> {
    #[cfg(unix)]
    fn fallback(meta: &fs::Metadata) -> std::io::Result<std::time::SystemTime> {
        use std::os::unix::fs::MetadataExt as _;
        let changed = std::time::UNIX_EPOCH
            + std::time::Duration::new(
                u64::try_from(meta.ctime()).unwrap_or_default(),
                u32::try_from(meta.ctime_nsec()).unwrap_or_default(),
            );
        meta.modified().map(|modified| modified.min(changed))
    }
    #[cfg(not(unix))]
    fn fallback(meta: &fs::Metadata) -> std::io::Result<std::time::SystemTime> {
        meta.modified()
    }
    system_time_to_utc(meta.created().or_else(|_| fallback(meta)))
}

/// Whether a `get_files` entry named `name` is one of `extensions`, compared
/// ignoring case. The extension is what follows the last dot, as C#
/// `Path.GetExtension` has it, so the file `.log` has one; `Path::extension`
/// says it has none.
fn has_extension(name: &str, extensions: &[&str]) -> bool {
    name.rfind('.')
        .map(|dot| &name[dot..])
        .is_some_and(|ext| extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

/// A listing failure, as `DirectoryInfo.EnumerateFileSystemInfos` tells them
/// apart: `path` is no directory (`DirectoryNotFoundException`, for ENOTDIR as
/// well), or the server may not read it (what `IgnoreInaccessible` skips).
fn listing_error(path: &str, err: &std::io::Error) -> ServiceError {
    let message = format!("listing {path} failed: {err}");
    match err.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            ServiceError::not_found(message)
        }
        std::io::ErrorKind::PermissionDenied => ServiceError::Forbidden(message),
        _ => ServiceError::backend(message),
    }
}

/// A failure to open or read `path`: `404`-worthy absence, or a backend error.
fn read_error(path: &str, err: &std::io::Error) -> ServiceError {
    if err.kind() == std::io::ErrorKind::NotFound {
        ServiceError::not_found(format!("file not found: {path}"))
    } else {
        ServiceError::Backend(format!("read file ({path}): {err}"))
    }
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
/// The `DriveType` filter is here; `ready` answers the other two for the mounts
/// it leaves, in their order ([`mount_is_ready`]). It is injected so the rule is
/// unit-testable without a real mount table.
///
/// The order of the mount table is preserved: C# enumerates `getmntent` output
/// and never sorts. `Name` and `FullName` are both the mount point on Unix
/// (`DriveInfo.Name` is the normalized ctor argument and
/// `RootDirectory.FullName` resolves to the same string).
fn drives_from(
    mounts: &[MountEntry],
    ready: impl FnOnce(&[&str]) -> Vec<bool>,
) -> Vec<FileSystemEntryInfo> {
    let browsable: Vec<&str> = mounts
        .iter()
        .filter(|m| mount_table::drive_type(&m.fs_type).is_browsable_drive())
        .map(|m| m.target.as_str())
        .collect();
    let ready = ready(&browsable);
    browsable
        .into_iter()
        .zip(ready)
        .filter(|(_, ready)| *ready)
        .map(|(target, _)| FileSystemEntryInfo {
            name: target.to_owned(),
            path: target.to_owned(),
            type_: FileSystemEntryType::Directory,
        })
        .collect()
}

/// `d.IsReady && d.TotalSize != 0`. `IsReady` on Unix is
/// `Directory.Exists(Name)`, which is what drops Docker's `/etc/hosts`-style
/// single-file bind mounts; the size clause drops mounts with no capacity.
fn mount_is_ready(target: &str) -> bool {
    Path::new(target).is_dir() && total_size(target) != 0
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

/// Whether the symlink at `path` lists as a directory.
///
/// `ManagedFileSystem.GetFileSystemMetadata` stats a link's target, so a
/// link to a directory is a directory (a linked series or movie folder is
/// scanned); a link to a file, or one that dangles, is a file. A link back
/// to a directory already on the way to it (see [`leads_back`]) is not
/// descended, or the walk would nest until the kernel's `ELOOP` limit.
/// Upstream appears to have no such guard, so this is a deliberate divergence.
fn symlink_is_directory(path: &Path) -> bool {
    let Ok(target) = fs::metadata(path) else {
        return false;
    };
    if !target.is_dir() {
        return false;
    }
    if leads_back(path, &target) {
        // Not an error, but the reason a linked folder never shows up.
        tracing::debug!(path = %path.display(), "symlink leads back to a directory on its own path; not descending");
        return false;
    }
    true
}

/// Whether `target`, the directory the link at `path` resolves to, is one of
/// the directories `path` lies in (compared by device and inode, along the
/// listed path, links followed).
#[cfg(unix)]
fn leads_back(path: &Path, target: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.ancestors()
        .skip(1)
        .filter_map(|ancestor| fs::metadata(ancestor).ok())
        .any(|dir| dir.dev() == target.dev() && dir.ino() == target.ino())
}

/// Non-unix fallback: no inode identity to compare, so no link is cut.
#[cfg(not(unix))]
fn leads_back(_path: &Path, _target: &fs::Metadata) -> bool {
    false
}

impl FileSystem for FerrofinFileSystem {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }

    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        let listing_failed = |err: std::io::Error| listing_error(path, &err);
        let read_dir = fs::read_dir(path).map_err(listing_failed)?;
        let mut entries = Vec::new();
        for entry in read_dir {
            // An entry that fails to read makes the listing incomplete: what
            // it is cannot be told, so the whole listing is a failure.
            let entry = entry.map_err(listing_failed)?;
            let full_path = entry.path();
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => symlink_is_directory(&full_path),
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
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
        Ok(entries)
    }

    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        let mounts = mount_table::read_mount_table();
        if mounts.is_empty() {
            // No readable mount table (no /proc, no /etc/mtab). The filesystem
            // root is always a drive, so the directory browser is never empty.
            return vec![root_drive()];
        }
        let drives = drives_from(&mounts, |targets| {
            DRIVE_PROBES.answers(targets, &mount_is_ready)
        });
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

    fn path_exists(&self, path: &str) -> bool {
        stat_says_exists(path, fs::metadata(path).map(|m| m.is_file() || m.is_dir()))
    }

    /// The same device and inode (Unix), or the same canonical path
    /// elsewhere. A path that cannot be read is not known to be the same.
    ///
    /// A mount that makes up inode numbers per lookup (CIFS with
    /// `noserverino`) answers `false` for two spellings of one file, so the
    /// scan keeps an old spelling's row there as it did before.
    fn same_file(&self, a: &str, b: &str) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            match (fs::metadata(a), fs::metadata(b)) {
                (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            match (fs::canonicalize(a), fs::canonicalize(b)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        }
    }

    fn validate_writable(&self, path: &str) -> Result<(), ServiceError> {
        let probe = Path::new(path).join(Uuid::new_v4().to_string());
        let result = fs::write(&probe, b"");
        // Always attempt cleanup (C# `finally`), then surface the write result.
        if probe.exists() {
            let _ = fs::remove_file(&probe);
        }
        result.map_err(|e| {
            let message = format!("path is not writable: {e}");
            match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    ServiceError::not_found(message)
                }
                _ => ServiceError::backend(message),
            }
        })
    }

    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        let Ok(read_dir) = fs::read_dir(path) else {
            return Vec::new();
        };
        let mut files = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !extensions.is_empty() && !has_extension(&name, extensions) {
                continue;
            }
            let full_path = entry.path();
            // A link has its target's type and length; one that dangles is a
            // file of no length, which a stat of the link itself tells.
            let target = fs::metadata(&full_path);
            let dangling = target.is_err();
            let Ok(meta) = target.or_else(|_| entry.metadata()) else {
                continue;
            };
            if meta.is_dir() {
                continue;
            }
            files.push(FileMetadata {
                name,
                full_name: full_path.to_string_lossy().into_owned(),
                length: if dangling {
                    0
                } else {
                    i64::try_from(meta.len()).unwrap_or(i64::MAX)
                },
                date_created: creation_time(&meta),
                date_modified: system_time_to_utc(meta.modified()),
            });
        }
        files
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        fs::read(path).map_err(|e| read_error(path, &e))
    }

    fn open_file(&self, path: &str) -> Result<OpenedFile, ServiceError> {
        let file = fs::File::open(path).map_err(|e| read_error(path, &e))?;
        let meta = file.metadata().map_err(|e| read_error(path, &e))?;
        // Opens fine and fails on the first read, after the response's headers.
        if meta.is_dir() {
            let is_a_directory = std::io::Error::from(std::io::ErrorKind::IsADirectory);
            return Err(read_error(path, &is_a_directory));
        }
        Ok(OpenedFile {
            path: path.to_owned(),
            length: meta.len(),
            reader: Box::new(file),
        })
    }
}

/// Reads a stat of `path` (`Ok(is a file or a directory)`) the way the
/// prune's safety net needs it: only "no such file" (or a path component
/// that is not a directory) means gone. Any other failure — a permission
/// error, an I/O error, a stale or disconnected network mount, a timeout —
/// says nothing about whether the file is there, so it counts as present:
/// a flaky mount must never delete an item and its user data.
fn stat_says_exists(path: &str, stat: std::io::Result<bool>) -> bool {
    match stat {
        Ok(is_file_or_dir) => is_file_or_dir,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            false
        }
        Err(err) => {
            tracing::debug!(path, %err, "could not stat a path; counting it as present");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    /// `same_file` is file identity, not path equality: one file reached
    /// through two paths is the same (a hard link here; two spellings on a
    /// case-insensitive mount), two files are not, and a missing path is not
    /// known to be anything.
    #[test]
    fn same_file_compares_identity_not_spelling() {
        use ferrofin_traits::filesystem::FileSystem as _;
        let dir = tempfile::tempdir().expect("tmp");
        let file = dir.path().join("Movie.mkv");
        let other = dir.path().join("Other.mkv");
        let link = dir.path().join("link.mkv");
        fs::write(&file, b"a").expect("write");
        fs::write(&other, b"b").expect("write");
        fs::hard_link(&file, &link).expect("hard link");
        let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let real = FerrofinFileSystem::new();

        assert!(real.same_file(&path(&file), &path(&file)));
        assert!(
            real.same_file(&path(&file), &path(&link)),
            "a hard link is the same file"
        );
        assert!(!real.same_file(&path(&file), &path(&other)), "two files");
        assert!(!real.same_file(&path(&file), &path(&dir.path().join("missing.mkv"))));
    }

    /// Only "not found" and "not a directory" mean gone; every other stat
    /// failure (EACCES, EIO, ESTALE, ENOTCONN, ETIMEDOUT) counts as present.
    #[test]
    fn only_a_missing_path_is_gone() {
        use std::io::{Error, ErrorKind};
        assert!(super::stat_says_exists("/p", Ok(true)));
        assert!(
            !super::stat_says_exists("/p", Ok(false)),
            "a socket or a fifo"
        );
        for gone in [ErrorKind::NotFound, ErrorKind::NotADirectory] {
            assert!(!super::stat_says_exists("/p", Err(Error::from(gone))));
        }
        for unknown in [
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
            ErrorKind::NotConnected,
            ErrorKind::StaleNetworkFileHandle,
            ErrorKind::Other,
        ] {
            assert!(
                super::stat_says_exists("/p", Err(Error::from(unknown))),
                "{unknown:?}"
            );
        }
    }

    use super::*;

    /// `drives_from` with the two readiness rules answered by closures.
    fn drives_with(
        mounts: &[MountEntry],
        is_dir: &dyn Fn(&str) -> bool,
        total_size: &dyn Fn(&str) -> u64,
    ) -> Vec<FileSystemEntryInfo> {
        drives_from(mounts, |targets| {
            targets
                .iter()
                .map(|target| is_dir(target) && total_size(target) != 0)
                .collect()
        })
    }

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
        let drives = drives_with(&mounts, &fake_is_dir, &|_| 4_000_000_000_000);
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
        let drives = drives_with(&mounts, &fake_is_dir, &|p| {
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
        let drives = drives_with(&mounts, &|_| true, &|_| 1024);
        let paths: Vec<&str> = drives.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["/mnt/nas", "/mnt/stick"]);
    }

    #[test]
    fn get_drives_on_this_host_lists_real_directory_mounts() {
        // The live call: never empty, every entry an existing directory, and the
        // root is present (it is a mount on every Unix).
        let drives = FerrofinFileSystem::new().get_drives();
        assert!(!drives.is_empty(), "{drives:?}");
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

    /// The type each entry of `dir` lists as, by name.
    #[cfg(unix)]
    fn listed_types(dir: &Path) -> std::collections::BTreeMap<String, FileSystemEntryType> {
        FerrofinFileSystem::new()
            .get_file_system_entries(&dir.to_string_lossy())
            .into_iter()
            .map(|entry| (entry.name, entry.type_))
            .collect()
    }

    /// `ManagedFileSystem.GetFileSystemMetadata` sets `IsDirectory` for a
    /// link to a directory (.NET stats the target of a link), so a linked
    /// series or movie folder is scanned; a link to a file stays a file and a
    /// dangling link is a plain file, never a directory.
    #[cfg(unix)]
    #[test]
    fn a_symlink_lists_as_what_it_points_at() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(outside.path().join("Show")).expect("mkdir");
        std::fs::write(outside.path().join("movie.mkv"), b"x").expect("write");
        symlink(outside.path().join("Show"), tmp.path().join("linked dir")).expect("link");
        symlink(
            outside.path().join("movie.mkv"),
            tmp.path().join("linked.mkv"),
        )
        .expect("link");
        symlink(outside.path().join("gone"), tmp.path().join("dangling")).expect("link");

        let types = listed_types(tmp.path());
        assert_eq!(types["linked dir"], FileSystemEntryType::Directory);
        assert_eq!(types["linked.mkv"], FileSystemEntryType::File);
        assert_eq!(types["dangling"], FileSystemEntryType::File);
    }

    /// A link back to a directory already on the way to it is not a
    /// directory to descend: a link to itself, to its parent or to a
    /// grandparent would otherwise nest until the kernel's `ELOOP` limit.
    /// Links that only lead somewhere else are still followed; of two links
    /// that point at each other's directories, the second is the one cut.
    #[cfg(unix)]
    #[test]
    fn a_symlink_back_to_an_ancestor_is_not_descended() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/sub")).expect("mkdir");
        std::fs::create_dir(root.join("c")).expect("mkdir");
        symlink(root, root.join("a/sub/to-root")).expect("link");
        symlink(root.join("a"), root.join("a/to-parent")).expect("link");
        symlink(root.join("a/sub"), root.join("a/sub/to-self")).expect("link");
        symlink(root.join("c"), root.join("a/b")).expect("link");
        symlink(root.join("a"), root.join("c/d")).expect("link");

        let sub = listed_types(&root.join("a/sub"));
        assert_eq!(sub["to-root"], FileSystemEntryType::File);
        assert_eq!(sub["to-self"], FileSystemEntryType::File);
        let a = listed_types(&root.join("a"));
        assert_eq!(a["to-parent"], FileSystemEntryType::File);
        assert_eq!(a["b"], FileSystemEntryType::Directory, "leads away");
        // `a/b` is `c`, whose `d` leads back to `a`: the loop is cut there.
        let through = listed_types(&root.join("a/b"));
        assert_eq!(through["d"], FileSystemEntryType::File, "back to `a`");
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

    /// Oracle: `GET /System/Logs` and `/FallbackFont/Fonts` of a Jellyfin 12.4
    /// over this folder. `EnumerateFiles` leaves out directories only, the
    /// extension follows the last dot, and a link is what it points at.
    #[cfg(unix)]
    #[test]
    fn get_files_keeps_what_upstream_keeps() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::write(dir.join("a.ttf"), b"font").expect("write");
        std::fs::write(dir.join(".ttf"), b"font").expect("write");
        std::fs::write(dir.join("noext"), b"font").expect("write");
        std::fs::write(dir.join("UPPER.TTF"), b"font").expect("write");
        std::fs::create_dir(dir.join("sub.ttf")).expect("mkdir");
        symlink(dir.join("a.ttf"), dir.join("link.ttf")).expect("link");
        symlink(dir.join("sub.ttf"), dir.join("linked-dir.ttf")).expect("link");
        symlink(dir.join("gone.ttf"), dir.join("dangling.ttf")).expect("link");

        let fs = FerrofinFileSystem::new();
        let path = dir.to_string_lossy();
        let mut fonts: Vec<(String, i64)> = fs
            .get_files(&path, &[".ttf"])
            .into_iter()
            .map(|f| (f.name, f.length))
            .collect();
        fonts.sort();
        let expected = [
            (".ttf", 4),
            ("UPPER.TTF", 4),
            ("a.ttf", 4),
            ("dangling.ttf", 0),
            ("link.ttf", 4),
        ]
        .map(|(name, length)| (name.to_owned(), length));
        assert_eq!(fonts, expected);
        assert!(fs.get_files(&path, &[]).iter().any(|f| f.name == "noext"));
    }

    /// A file's creation time is never the epoch: with no birth time it is
    /// the older of the change and modification times.
    #[test]
    fn get_files_has_a_creation_time() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.log"), b"x").expect("write");
        let files = FerrofinFileSystem::new().get_files(&tmp.path().to_string_lossy(), &[]);
        assert!(files[0].date_created.timestamp() > 0);
    }

    /// A listing failure keeps what upstream tells apart.
    #[cfg(unix)]
    #[test]
    fn a_failed_listing_says_why() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        let fs = FerrofinFileSystem::new();
        let listing = |path: &Path| fs.try_get_file_system_entries(&path.to_string_lossy());

        assert!(matches!(
            listing(&tmp.path().join("missing")),
            Err(ServiceError::NotFound(_))
        ));
        assert!(
            matches!(listing(&file), Err(ServiceError::NotFound(_))),
            "a file is no directory: upstream's DirectoryNotFoundException"
        );

        let shut = tmp.path().join("shut");
        std::fs::create_dir(&shut).expect("mkdir");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // Root reads anything, so the case only exists for another user.
        if std::fs::read_dir(&shut).is_err() {
            assert!(matches!(listing(&shut), Err(ServiceError::Forbidden(_))));
        }
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn open_file_streams_what_is_on_disk() {
        use std::io::Read as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("big.log");
        std::fs::write(&path, b"log-body").expect("write");

        let mut opened = FerrofinFileSystem::new()
            .open_file(&path.to_string_lossy())
            .expect("open");
        assert_eq!(opened.length, 8);
        let mut body = Vec::new();
        opened.reader.read_to_end(&mut body).expect("read");
        assert_eq!(body, b"log-body");
    }

    /// The same statuses as `read_file`: a font or a log that went between the
    /// listing and the open is a 404, one that cannot be opened a 500.
    #[test]
    fn open_file_fails_as_read_file_does() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fs = FerrofinFileSystem::new();
        assert!(matches!(
            fs.open_file("/no/such/file/here"),
            Err(ServiceError::NotFound(_))
        ));
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(matches!(
            fs.open_file(&file.join("under").to_string_lossy()),
            Err(ServiceError::Backend(_))
        ));
        assert!(
            matches!(
                fs.open_file(&tmp.path().to_string_lossy()),
                Err(ServiceError::Backend(_))
            ),
            "a directory fails before the headers, not on the first read"
        );
    }

    /// `File.WriteAllText` under a path that is no directory throws
    /// `DirectoryNotFoundException` (404); any other refusal is a 500.
    #[cfg(unix)]
    #[test]
    fn validate_writable_says_why_it_failed() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        let fs = FerrofinFileSystem::new();
        assert!(matches!(
            fs.validate_writable(&file.to_string_lossy()),
            Err(ServiceError::NotFound(_))
        ));

        let read_only = tmp.path().join("read-only");
        std::fs::create_dir(&read_only).expect("mkdir");
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        // Root writes anywhere, so the case only exists for another user.
        if std::fs::write(read_only.join("probe"), b"").is_err() {
            assert!(matches!(
                fs.validate_writable(&read_only.to_string_lossy()),
                Err(ServiceError::Backend(_))
            ));
        }
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
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
