//! The Unix mount table, and .NET's `DriveType` classification of a filesystem.
//!
//! Two endpoints need this: `GET /Environment/Drives` (a port of
//! `ManagedFileSystem.GetDrives`, which is `DriveInfo.GetDrives()` filtered to
//! `Fixed | Network | Removable`) and `GET /System/Info/Storage` (a port of
//! `StorageHelper.GetFreeSpaceOf`, which reports `driveInfo.DriveType.ToString()`).
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

/// One row of `/proc/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// The mount point (the second `getmntent` field, `mnt_dir`).
    pub target: String,
    /// The filesystem type (the third field, `mnt_type`).
    pub fs_type: String,
}

/// Un-escapes the octal escapes `getmntent` leaves in a mount-table field
/// (`\040` space, `\011` tab, `\012` newline, `\134` backslash).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = &field[i + 1..i + 4];
            if let Ok(code) = u8::from_str_radix(digits, 8) {
                out.push(char::from(code));
                i += 4;
                continue;
            }
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
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
                fs_type: unescape(fs_type),
            })
        })
        .collect()
}

/// Reads the live mount table, preferring `/proc/mounts` and falling back to
/// `/etc/mtab` (what `getmntent` opens when `/proc` is not mounted).
#[must_use]
pub fn read_mount_table() -> Vec<MountEntry> {
    for path in ["/proc/mounts", "/etc/mtab"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            return parse_mount_table(&text);
        }
    }
    Vec::new()
}

/// The filesystem type of the mount containing `path`: the longest mount point
/// that is a path-component prefix of `path`.
///
/// This stands in for `statfs(path).f_type`, which is how .NET resolves the
/// filesystem for an arbitrary path on Linux.
#[must_use]
pub fn fs_type_for_path<'a>(mounts: &'a [MountEntry], path: &str) -> Option<&'a str> {
    let mut best: Option<&MountEntry> = None;
    for entry in mounts {
        if !path_has_prefix(path, &entry.target) {
            continue;
        }
        if best.is_none_or(|b| entry.target.len() >= b.target.len()) {
            best = Some(entry);
        }
    }
    best.map(|e| e.fs_type.as_str())
}

/// Whether `path` is `prefix` or lives beneath it (component-wise, so `/cache`
/// is not a prefix of `/cachex`).
fn path_has_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return path.starts_with('/');
    }
    if !path.starts_with(prefix) {
        return false;
    }
    matches!(path.as_bytes().get(prefix.len()), None | Some(b'/'))
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
        assert_eq!(mounts[0].fs_type, "overlay");
        assert_eq!(mounts[8].target, "/config");
        assert_eq!(mounts[8].fs_type, "btrfs");
        // No sorting: /cache follows /config as in the file.
        assert_eq!(mounts[9].target, "/cache");
    }

    #[test]
    fn unescapes_octal_fields() {
        let mounts = parse_mount_table("dev /mnt/my\\040disk\\011x ext4 rw 0 0\n");
        assert_eq!(mounts[0].target, "/mnt/my disk\tx");
    }

    #[test]
    fn skips_short_rows() {
        assert!(parse_mount_table("garbage\n\n/dev x\n").is_empty());
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
        assert_eq!(fs_type_for_path(&mounts, "/config/log"), Some("btrfs"));
        assert_eq!(fs_type_for_path(&mounts, "/usr/share/web"), Some("overlay"));
        assert_eq!(
            fs_type_for_path(&mounts, "/sys/fs/cgroup/x"),
            Some("cgroup2")
        );
        // Component-wise: /cache must not claim /cachex.
        assert_eq!(fs_type_for_path(&mounts, "/cachex"), Some("overlay"));
        assert_eq!(fs_type_for_path(&[], "/config"), None);
    }
}
