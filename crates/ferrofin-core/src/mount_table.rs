//! The drive table (.NET's `DriveInfo`): the Unix mount table or the Windows
//! drive letters, each drive's `DriveType`, and a filesystem's size.
//!
//! Two endpoints need this: `GET /Environment/Drives` (a port of
//! `ManagedFileSystem.GetDrives`, which is `DriveInfo.GetDrives()` filtered to
//! `Fixed | Network | Removable`) and `GET /System/Info/Storage` (a port of
//! `StorageHelper.GetFreeSpaceOf`, which reports the mount point and
//! `DriveType.ToString()` of the deepest drive containing a folder).
//!
//! Port notes — how .NET reaches the same answer:
//! - `DriveInfo.GetDrives()` on Unix is `Interop.Sys.GetAllMountPoints()`, i.e.
//!   `getmntent_r` over `/proc/mounts`, in file order, with no sorting and no
//!   de-duplication. [`parse_mount_table`] reads the same file the same way.
//! - `DriveInfo.DriveType` is `Interop.Sys.GetDriveTypeForMountPoint(Name)`. On
//!   Linux the native side (`pal_mount.c`) returns `statfs(path).f_type` as a
//!   magic number and the managed side maps it to a filesystem *name*, which is
//!   then classified by the table transliterated into [`drive_type`] from
//!   `Interop.MountPoints.FormatInfo.cs`. We resolve the name from the mount
//!   table instead of from the `statfs` magic; for a path inside a mount, the
//!   longest matching mount point is the filesystem `statfs` would report.
//! - On Windows, `DriveInfo.GetDrives()` is one `X:\` root per bit of
//!   `GetLogicalDrives()`, in letter order, and `DriveType` is
//!   `(DriveType)GetDriveType(Name)`; [`read_mount_table`] does the same.
//!   `AvailableFreeSpace` and `TotalSize` are `GetDiskFreeSpaceEx`'s caller
//!   and total byte counts ([`disk_space`]).

/// .NET's `System.IO.DriveType`.
///
/// `ToString()` on this enum is what `FolderStorageInfo.StorageType` carries on
/// the wire, so the [`Display`](std::fmt::Display) impl must spell the variants
/// exactly as .NET does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveType {
    /// The type could not be determined (the table's default arm).
    Unknown,
    /// The path has no root directory.
    NoRootDirectory,
    /// Removable media (floppy, USB stick, `vfat`).
    Removable,
    /// A fixed local disk.
    Fixed,
    /// A network share.
    Network,
    /// Optical media.
    CdRom,
    /// A RAM disk / kernel pseudo-filesystem.
    Ram,
}

impl std::fmt::Display for DriveType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Unknown => "Unknown",
            Self::NoRootDirectory => "NoRootDirectory",
            Self::Removable => "Removable",
            Self::Fixed => "Fixed",
            Self::Network => "Network",
            Self::CdRom => "CDRom",
            Self::Ram => "Ram",
        };
        f.write_str(s)
    }
}

impl DriveType {
    /// Whether `ManagedFileSystem.GetDrives` keeps a drive of this type
    /// (`d.DriveType == Fixed || Network || Removable`).
    #[must_use]
    pub const fn is_browsable_drive(self) -> bool {
        matches!(self, Self::Fixed | Self::Network | Self::Removable)
    }
}

/// Filesystem names .NET classifies as `DriveType.CDRom`.
const CDROM: &[&str] = &[
    "cddafs",
    "cd9660",
    "iso",
    "isofs",
    "iso9660",
    "fuseiso",
    "fuseiso9660",
    "udf",
    "umview-mod-umfuseiso9660",
];

/// Filesystem names .NET classifies as `DriveType.Fixed`.
const FIXED: &[&str] = &[
    "aafs",
    "adfs",
    "affs",
    "anoninode",
    "anon-inode FS",
    "apfs",
    "balloon-kvm-fs",
    "bdevfs",
    "befs",
    "bfs",
    "bootfs",
    "bpf_fs",
    "btrfs",
    "btrfs_test",
    "coh",
    "daxfs",
    "drvfs",
    "efivarfs",
    "efs",
    "exfat",
    "exofs",
    "ext",
    "ext2",
    "ext2_old",
    "ext3",
    "ext2/ext3",
    "ext4",
    "ext4dev",
    "f2fs",
    "fat",
    "fuseext2",
    "fusefat",
    "hfs",
    "hfs+",
    "hfsplus",
    "hfsx",
    "hostfs",
    "hpfs",
    "inodefs",
    "inotifyfs",
    "jbd",
    "jbd2",
    "jffs",
    "jffs2",
    "jfs",
    "lofs",
    "logfs",
    "lxfs",
    "minix (30 char.)",
    "minix v2 (30 char.)",
    "minix v2",
    "minix",
    "minix_old",
    "minix2",
    "minix2v2",
    "minix2 v2",
    "minix3",
    "mlfs",
    "msdos",
    "nilfs",
    "nsfs",
    "ntfs",
    "ntfs-3g",
    "ocfs2",
    "omfs",
    "overlay",
    "overlayfs",
    "pstorefs",
    "qnx4",
    "qnx6",
    "reiserfs",
    "rpc_pipefs",
    "sffs",
    "smackfs",
    "squashfs",
    "swap",
    "sysv",
    "sysv2",
    "sysv4",
    "tracefs",
    "ubifs",
    "ufs",
    "ufscigam",
    "ufs2",
    "umsdos",
    "umview-mod-umfuseext2",
    "v9fs",
    "vagrant",
    "vboxfs",
    "vxfs",
    "vxfs_olt",
    "vzfs",
    "wslfs",
    "xenix",
    "xfs",
    "xia",
    "xiafs",
    "xmount",
    "zfs",
    "zfs-fuse",
    "zsmallocfs",
];

/// Filesystem names .NET classifies as `DriveType.Network`.
const NETWORK: &[&str] = &[
    "9p",
    "acfs",
    "afp",
    "afpfs",
    "afs",
    "aufs",
    "autofs",
    "autofs4",
    "beaglefs",
    "ceph",
    "cifs",
    "coda",
    "coherent",
    "curlftpfs",
    "davfs2",
    "dlm",
    "ecryptfs",
    "eCryptfs",
    "fhgfs",
    "flickrfs",
    "ftp",
    "fuse",
    "fuseblk",
    "fusedav",
    "fusesmb",
    "gfsgfs2",
    "gfs/gfs2",
    "gfs2",
    "glusterfs-client",
    "gmailfs",
    "gpfs",
    "ibrix",
    "k-afs",
    "kafs",
    "kbfuse",
    "ltspfs",
    "lustre",
    "ncp",
    "ncpfs",
    "nfs",
    "nfs4",
    "nfsd",
    "novell",
    "obexfs",
    "panfs",
    "prl_fs",
    "s3ql",
    "samba",
    "smb",
    "smb2",
    "smbfs",
    "snfs",
    "sshfs",
    "vmhgfs",
    "webdav",
    "wikipediafs",
    "xenfs",
];

/// Filesystem names .NET classifies as `DriveType.Ram`.
const RAM: &[&str] = &[
    "anon_inode",
    "anon_inodefs",
    "aptfs",
    "avfs",
    "bdev",
    "bpf",
    "binfmt_misc",
    "cgroup",
    "cgroup2",
    "cgroupfs",
    "cgroup2fs",
    "configfs",
    "cpuset",
    "cramfs",
    "cramfs-wend",
    "cryptkeeper",
    "ctfs",
    "debugfs",
    "dev",
    "devfs",
    "devpts",
    "devtmpfs",
    "encfs",
    "fd",
    "fdesc",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "fusectl",
    "futexfs",
    "hugetlbfs",
    "libpam-encfs",
    "ibpam-mount",
    "mntfs",
    "mqueue",
    "mtpfs",
    "mythtvfs",
    "objfs",
    "openprom",
    "openpromfs",
    "pipefs",
    "plptools",
    "proc",
    "pstore",
    "pytagsfs",
    "ramfs",
    "rofs",
    "romfs",
    "rootfs",
    "securityfs",
    "selinux",
    "selinuxfs",
    "sharefs",
    "sockfs",
    "sysfs",
    "tmpfs",
    "udev",
    "usbdev",
    "usbdevfs",
];

/// Filesystem names .NET classifies as `DriveType.Removable`.
const REMOVABLE: &[&str] = &["gphotofs", "sdcardfs", "usbfs", "usbdevice", "vfat"];

/// Classifies a filesystem type name the way .NET's `DriveInfo.DriveType` does.
///
/// Transliterated from `Interop.MountPoints.FormatInfo.cs`; an unlisted name is
/// `Unknown`, exactly as the C# `default` arm. The comparison is ordinal and
/// case-sensitive, matching the C# `switch` on the raw name.
#[must_use]
pub fn drive_type(fs_type: &str) -> DriveType {
    if CDROM.contains(&fs_type) {
        DriveType::CdRom
    } else if FIXED.contains(&fs_type) {
        DriveType::Fixed
    } else if NETWORK.contains(&fs_type) {
        DriveType::Network
    } else if RAM.contains(&fs_type) {
        DriveType::Ram
    } else if REMOVABLE.contains(&fs_type) {
        DriveType::Removable
    } else {
        DriveType::Unknown
    }
}

/// One drive: a row of `/proc/mounts`, or a Windows drive letter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// The mount point (the second `getmntent` field, `mnt_dir`), or the drive
    /// root (`C:\`): `DriveInfo.Name`.
    pub target: String,
    /// `DriveInfo.DriveType`: the filesystem type (the third field,
    /// `mnt_type`) as [`drive_type`] classifies it, or `GetDriveTypeW`'s.
    pub drive_type: DriveType,
}

/// Un-escapes the octal escapes `getmntent` leaves in a mount-table field
/// (`\040` space, `\011` tab, `\012` newline, `\134` backslash).
fn unescape(field: &str) -> String {
    // Bytes, not chars: an escape is one byte of a possibly multi-byte UTF-8
    // name, and the name is decoded as a whole, as .NET does.
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(code) = bytes
                .get(i + 1..i + 4)
                .and_then(|d| std::str::from_utf8(d).ok())
                .and_then(|d| u8::from_str_radix(d, 8).ok())
        {
            out.push(code);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses a `/proc/mounts` (or `/etc/mtab`) body into its rows, in file order.
///
/// Mirrors `getmntent_r`: whitespace-separated fields, octal-escaped, rows with
/// fewer than three fields skipped. Order is preserved and duplicates are kept —
/// `DriveInfo.GetDrives()` neither sorts nor de-duplicates.
#[must_use]
pub fn parse_mount_table(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let _source = fields.next()?;
            let target = fields.next()?;
            let fs_type = fields.next()?;
            Some(MountEntry {
                target: unescape(target),
                drive_type: drive_type(&unescape(fs_type)),
            })
        })
        .collect()
}

/// Reads the live mount table, preferring `/proc/mounts` and falling back to
/// `/etc/mtab` (what `getmntent` opens when `/proc` is not mounted).
#[cfg(not(windows))]
#[must_use]
pub fn read_mount_table() -> Vec<MountEntry> {
    for path in ["/proc/mounts", "/etc/mtab"] {
        // .NET decodes the table with replacement, so one non-UTF-8 byte in a
        // mount point must not empty it.
        if let Ok(bytes) = std::fs::read(path) {
            return parse_mount_table(&String::from_utf8_lossy(&bytes));
        }
    }
    Vec::new()
}

/// Lists the drive letters: `DriveInfo.GetDrives()` on Windows.
#[cfg(windows)]
#[must_use]
pub fn read_mount_table() -> Vec<MountEntry> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

    fail_critical_errors();
    // SAFETY: takes no arguments and only returns a bitmask.
    let mask = unsafe { GetLogicalDrives() };
    drive_roots(mask)
        .into_iter()
        .map(|target| {
            let root: Vec<u16> = target.encode_utf16().chain([0]).collect();
            // SAFETY: `root` is a NUL-terminated UTF-16 string that outlives the call.
            let code = unsafe { GetDriveTypeW(root.as_ptr()) };
            MountEntry {
                target,
                drive_type: windows_drive_type(code),
            }
        })
        .collect()
}

/// The root of each drive in a `GetLogicalDrives` bitmask, `A:\` for bit 0, in
/// letter order (.NET's `DriveInfoInternal.GetLogicalDrives`).
#[cfg(any(windows, test))]
fn drive_roots(mask: u32) -> Vec<String> {
    (b'A'..=b'Z')
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, letter)| format!("{}:\\", char::from(letter)))
        .collect()
}

/// A `GetDriveTypeW` code as .NET's `DriveType`, whose values are those codes
/// (`DRIVE_UNKNOWN` 0, `DRIVE_NO_ROOT_DIR` 1, `DRIVE_REMOVABLE` 2,
/// `DRIVE_FIXED` 3, `DRIVE_REMOTE` 4, `DRIVE_CDROM` 5, `DRIVE_RAMDISK` 6).
#[cfg(any(windows, test))]
fn windows_drive_type(code: u32) -> DriveType {
    match code {
        1 => DriveType::NoRootDirectory,
        2 => DriveType::Removable,
        3 => DriveType::Fixed,
        4 => DriveType::Network,
        5 => DriveType::CdRom,
        6 => DriveType::Ram,
        _ => DriveType::Unknown,
    }
}

/// Stops Windows prompting to insert a disk when a drive without media is
/// queried, as .NET's `DriveInfo` does around each query
/// (`SetThreadErrorMode(SEM_FAILCRITICALERRORS)`). Set for the whole process
/// here, as Microsoft recommends for every process, so the directory test of
/// `IsReady` that follows on another thread is covered too. The boot's storage
/// check reads the drive table first, so it is set before any other drive is
/// touched.
#[cfg(windows)]
fn fail_critical_errors() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetErrorMode, SEM_FAILCRITICALERRORS, SetErrorMode,
    };

    // SAFETY: both only read or set the process's error-mode flags.
    unsafe {
        SetErrorMode(GetErrorMode() | SEM_FAILCRITICALERRORS);
    }
}

/// The space of the filesystem holding `path`: `(available, total)` bytes,
/// .NET's `DriveInfo.AvailableFreeSpace` and `TotalSize`, or `None` when the
/// query fails (where those properties throw).
///
/// `available` is the space an unprivileged caller may use (`f_bavail`), so
/// `total - available` **includes** the root-reserved blocks, as C#'s
/// `UsedSpace = TotalSize - AvailableFreeSpace` does. Subtracting `f_bfree`
/// there instead under-reports used space by the reservation (15.2 GiB on the
/// parity lab's filesystem).
#[cfg(unix)]
#[must_use]
pub fn disk_space(path: &str) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(std::path::Path::new(path).as_os_str().as_bytes()).ok()?;
    // SAFETY: `stat` is fully written by `statvfs` before it is read, and
    // `c_path` is a valid NUL-terminated C string that outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), std::ptr::from_mut(&mut stat)) } != 0 {
        return None;
    }
    // statvfs field widths differ per platform (u64 on Linux, u32 on macOS).
    #[allow(clippy::useless_conversion)]
    let bytes =
        |blocks: libc::fsblkcnt_t| u64::from(blocks).saturating_mul(u64::from(stat.f_frsize));
    Some((bytes(stat.f_bavail), bytes(stat.f_blocks)))
}

/// The space of the volume holding `path`: `(available, total)` bytes,
/// `GetDiskFreeSpaceExW`'s caller and total counts (.NET's
/// `DriveInfo.AvailableFreeSpace` and `TotalSize`), or `None` when it fails.
#[cfg(windows)]
#[must_use]
pub fn disk_space(path: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // An interior NUL would cut the path short (`CString::new` refuses it on unix).
    if path.contains('\0') {
        return None;
    }
    fail_critical_errors();
    // A UNC share must end in a separator (`\\server\share\`); any folder may.
    let separator = (!path.ends_with(['\\', '/'])).then_some(u16::from(b'\\'));
    let wide: Vec<u16> = path.encode_utf16().chain(separator).chain([0]).collect();
    let (mut available, mut total) = (0_u64, 0_u64);
    // SAFETY: `wide` is NUL-terminated and outlives the call, the two out
    // pointers are valid for writes, and the third may be null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            std::ptr::from_mut(&mut available),
            std::ptr::from_mut(&mut total),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some((available, total))
}

/// The drive containing `path`: the longest drive root or mount point that is
/// a path-component prefix of `path` (`StorageHelper.GetFreeSpaceOf`'s
/// `bestMatch` over `DriveInfo.GetDrives()`).
///
/// Upstream matches with a raw, case-insensitive `StartsWith`, so a mount at
/// `/mnt/media` also claims `/mnt/media2`; the match here is component-wise
/// and case-sensitive, as the filesystem is, and on Windows ignores ASCII case
/// as NTFS does (`d:\x` is on `D:\`). With stacked mounts on one target the
/// last listed wins (upstream takes the first row), which is invisible: both
/// rows name the same mount point, and .NET's `DriveType` comes from `statfs`
/// on it, the topmost (last-listed) filesystem.
#[must_use]
pub fn mount_for_path<'a>(mounts: &'a [MountEntry], path: &str) -> Option<&'a MountEntry> {
    deepest_mount(mounts, path, cfg!(windows))
}

/// [`mount_for_path`] under one platform's path rules: `windows` separates
/// with `\` and ignores ASCII case.
fn deepest_mount<'a>(
    mounts: &'a [MountEntry],
    path: &str,
    windows: bool,
) -> Option<&'a MountEntry> {
    let mut best: Option<&MountEntry> = None;
    for entry in mounts {
        if !path_has_prefix(path, &entry.target, windows) {
            continue;
        }
        if best.is_none_or(|b| entry.target.len() >= b.target.len()) {
            best = Some(entry);
        }
    }
    best
}

/// Whether `path` is `prefix` or lives beneath it (component-wise, so `/cache`
/// is not a prefix of `/cachex`).
fn path_has_prefix(path: &str, prefix: &str, windows: bool) -> bool {
    let separator = if windows { '\\' } else { '/' };
    let Some(head) = path.get(..prefix.len()) else {
        return false;
    };
    let same = if windows {
        head.eq_ignore_ascii_case(prefix)
    } else {
        head == prefix
    };
    let rest = &path[prefix.len()..];
    // A root (`/`, `C:\`) ends in its separator; any other mount point must end
    // the path or be followed by one.
    same && (prefix.ends_with(separator) || rest.is_empty() || rest.starts_with(separator))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copied verbatim from `ferrofin-dv4-jellyfin-1:/proc/mounts` (trimmed to
    /// the rows that matter), so the fixture is the real thing.
    const LIVE: &str = "\
overlay / overlay rw,lowerdir=/a:/b,upperdir=/c,workdir=/d 0 0
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

    #[test]
    fn parses_targets_and_types_in_file_order() {
        let mounts = parse_mount_table(LIVE);
        assert_eq!(mounts.len(), 16);
        assert_eq!(mounts[0].target, "/");
        assert_eq!(mounts[0].drive_type, DriveType::Fixed, "overlay");
        assert_eq!(mounts[1].drive_type, DriveType::Ram, "proc");
        assert_eq!(mounts[8].target, "/config");
        assert_eq!(mounts[8].drive_type, DriveType::Fixed, "btrfs");
        // No sorting: /cache follows /config as in the file.
        assert_eq!(mounts[9].target, "/cache");
        // The type is classified once unescaped.
        let mounts = parse_mount_table("none /mnt/x anon-inode\\040FS rw 0 0\n");
        assert_eq!(mounts[0].drive_type, DriveType::Fixed);
    }

    #[test]
    fn unescapes_octal_fields() {
        let mounts = parse_mount_table("dev /mnt/my\\040disk\\011x ext4 rw 0 0\n");
        assert_eq!(mounts[0].target, "/mnt/my disk\tx");
        // A UTF-8 name stays one name, whether its bytes are escaped or not.
        let mounts = parse_mount_table("dev /mnt/M\\303\\251dias\\040é ext4 rw 0 0\n");
        assert_eq!(mounts[0].target, "/mnt/Médias é");
    }

    #[test]
    fn skips_short_rows() {
        let found = parse_mount_table("garbage\n\n/dev x\n");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn classifies_filesystems_as_dotnet_does() {
        // Kept by ManagedFileSystem.GetDrives.
        assert_eq!(drive_type("btrfs"), DriveType::Fixed);
        assert_eq!(drive_type("overlay"), DriveType::Fixed);
        assert_eq!(drive_type("ext4"), DriveType::Fixed);
        assert_eq!(drive_type("nfs4"), DriveType::Network);
        assert_eq!(drive_type("cifs"), DriveType::Network);
        assert_eq!(drive_type("vfat"), DriveType::Removable);
        // Dropped by it.
        assert_eq!(drive_type("proc"), DriveType::Ram);
        assert_eq!(drive_type("sysfs"), DriveType::Ram);
        assert_eq!(drive_type("tmpfs"), DriveType::Ram);
        assert_eq!(drive_type("cgroup2"), DriveType::Ram);
        assert_eq!(drive_type("devpts"), DriveType::Ram);
        assert_eq!(drive_type("mqueue"), DriveType::Ram);
        assert_eq!(drive_type("iso9660"), DriveType::CdRom);
        assert_eq!(drive_type("udf"), DriveType::CdRom);
        assert_eq!(drive_type("not-a-real-fs"), DriveType::Unknown);
        // .NET calls these Fixed, not pseudo — a hand-written "pseudo" denylist
        // would wrongly drop them.
        assert_eq!(drive_type("squashfs"), DriveType::Fixed);
        assert_eq!(drive_type("nsfs"), DriveType::Fixed);
        assert_eq!(drive_type("tracefs"), DriveType::Fixed);
        assert_eq!(drive_type("efivarfs"), DriveType::Fixed);
        // …and autofs is Network, i.e. kept.
        assert_eq!(drive_type("autofs"), DriveType::Network);
    }

    #[test]
    fn drive_type_display_matches_dotnet_tostring() {
        assert_eq!(DriveType::Fixed.to_string(), "Fixed");
        assert_eq!(DriveType::CdRom.to_string(), "CDRom");
        assert_eq!(DriveType::NoRootDirectory.to_string(), "NoRootDirectory");
        assert_eq!(DriveType::Ram.to_string(), "Ram");
        assert_eq!(DriveType::Network.to_string(), "Network");
        assert_eq!(DriveType::Removable.to_string(), "Removable");
        assert_eq!(DriveType::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn browsable_set_is_fixed_network_removable() {
        assert!(DriveType::Fixed.is_browsable_drive());
        assert!(DriveType::Network.is_browsable_drive());
        assert!(DriveType::Removable.is_browsable_drive());
        assert!(!DriveType::Ram.is_browsable_drive());
        assert!(!DriveType::CdRom.is_browsable_drive());
        assert!(!DriveType::Unknown.is_browsable_drive());
    }

    #[test]
    fn resolves_the_longest_containing_mount() {
        let mounts = parse_mount_table(LIVE);
        let mount = |path| mount_for_path(&mounts, path).map(|m| (m.target.as_str(), m.drive_type));
        assert_eq!(mount("/config/log"), Some(("/config", DriveType::Fixed)));
        assert_eq!(mount("/usr/share/web"), Some(("/", DriveType::Fixed)));
        assert_eq!(
            mount("/sys/fs/cgroup/x"),
            Some(("/sys/fs/cgroup", DriveType::Ram))
        );
        // Component-wise: /cache must not claim /cachex.
        assert_eq!(mount("/cachex"), Some(("/", DriveType::Fixed)));
        assert_eq!(mount_for_path(&[], "/config"), None);
    }

    /// The live drive table and a drive's size, on the platform that has them.
    #[cfg(windows)]
    #[test]
    fn windows_lists_drive_roots_with_their_sizes() {
        let drives = read_mount_table();
        assert!(
            drives
                .iter()
                .all(|d| d.target.len() == 3 && d.target.ends_with(":\\"))
        );
        let temp = std::env::temp_dir().to_string_lossy().into_owned();
        let (available, total) = disk_space(&temp).expect("the temp folder's volume");
        assert!(total > 0 && available <= total);
        assert!(mount_for_path(&drives, &temp).is_some(), "{temp}");
    }

    #[rstest::rstest]
    #[case::none(0, &[])]
    #[case::a_is_bit_zero(0b1, &["A:\\"])]
    #[case::c_and_d(0b1100, &["C:\\", "D:\\"])]
    #[case::z_is_bit_25(1 << 25, &["Z:\\"])]
    #[case::bits_past_z_ignored(0b1100_0000 << 24, &[])]
    fn drive_roots_follow_the_logical_drives_bitmask(#[case] mask: u32, #[case] roots: &[&str]) {
        assert_eq!(drive_roots(mask), roots);
    }

    #[rstest::rstest]
    #[case::drive_unknown(0, DriveType::Unknown)]
    #[case::drive_no_root_dir(1, DriveType::NoRootDirectory)]
    #[case::drive_removable(2, DriveType::Removable)]
    #[case::drive_fixed(3, DriveType::Fixed)]
    #[case::drive_remote(4, DriveType::Network)]
    #[case::drive_cdrom(5, DriveType::CdRom)]
    #[case::drive_ramdisk(6, DriveType::Ram)]
    #[case::undefined(7, DriveType::Unknown)]
    fn windows_drive_codes_map_to_dotnet_drive_types(#[case] code: u32, #[case] want: DriveType) {
        assert_eq!(windows_drive_type(code), want);
    }

    /// The Windows drive match: a folder belongs to the drive whose root
    /// prefixes it, ASCII case aside. `D:\Jellyfin` on `D:\` is the case
    /// jellyfin/jellyfin#17500 fixed upstream (`ResolvePath` rebuilt it as
    /// `D:Jellyfin`, which no drive prefixed).
    #[rstest::rstest]
    #[case::drive_d(r"D:\Jellyfin\data", Some(r"D:\"))]
    #[case::drive_c(r"C:\ProgramData\Jellyfin", Some(r"C:\"))]
    #[case::the_root_itself(r"C:\", Some(r"C:\"))]
    #[case::lower_case_letter(r"d:\media", Some(r"D:\"))]
    #[case::mapped_drive(r"Z:\movies", Some(r"Z:\"))]
    #[case::unc_path(r"\\nas\media\movies", None)]
    #[case::drive_relative(r"C:media", None)]
    #[case::unlisted_letter(r"Q:\x", None)]
    fn windows_paths_resolve_to_their_drive(#[case] path: &str, #[case] want: Option<&str>) {
        let drives: Vec<MountEntry> = drive_roots(0b1100 | (1 << 25))
            .into_iter()
            .map(|target| MountEntry {
                target,
                drive_type: DriveType::Fixed,
            })
            .collect();
        let found = deepest_mount(&drives, path, true).map(|m| m.target.as_str());
        assert_eq!(found, want);
    }

    #[rstest::rstest]
    #[case::unix_root("/media", "/", false, true)]
    #[case::unix_exact("/cache", "/cache", false, true)]
    #[case::unix_beneath("/cache/x", "/cache", false, true)]
    #[case::unix_sibling("/cachex", "/cache", false, false)]
    #[case::unix_case_sensitive("/Cache/x", "/cache", false, false)]
    #[case::unix_backslash_is_a_name("/cache\\x", "/cache", false, false)]
    #[case::windows_root(r"C:\x", r"C:\", true, true)]
    #[case::windows_folder_mount(r"C:\mnt\disk\x", r"C:\mnt\disk", true, true)]
    #[case::windows_sibling(r"C:\mnt\diskx", r"C:\mnt\disk", true, false)]
    #[case::windows_case(r"c:\MNT\Disk\x", r"C:\mnt\disk", true, true)]
    #[case::shorter_than_prefix("/a", "/abc", false, false)]
    fn path_prefix_is_component_wise(
        #[case] path: &str,
        #[case] prefix: &str,
        #[case] windows: bool,
        #[case] want: bool,
    ) {
        assert_eq!(path_has_prefix(path, prefix, windows), want);
    }
}
