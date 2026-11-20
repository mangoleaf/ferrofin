//! The conditions under which an export refuses to run (gl-1550): an output
//! it would mix into or nest with the source, an interrupted earlier export, a
//! database another process holds open, too little free space, and a database
//! it cannot export (damaged, inconsistent, not Ferrofin's, from a Jellyfin
//! release Ferrofin does not know, or with a write-ahead log it cannot fold
//! in whole).
//!
//! The first four are checked before anything is written ([`super::refuse_unsafe`]).
//! The database is checked on the copy the snapshot stages, never on the
//! source: a connection to a database in WAL mode, even a read-only one, may
//! create files beside it.

use std::path::{Component, Path, PathBuf};

use crate::{DbError, Result};

/// A refusal naming its cause.
fn refused(reason: String) -> DbError {
    DbError::ExportRefused { reason }
}

/// Refuses an output that is not empty, is the source, holds it or sits in
/// it, is a mount point, and an export interrupted earlier (the `.partial`
/// beside it is still there). Returns the output's resolved path, which the
/// export writes beside and renames to: a trailing `/`, a `.`, `..` or a link
/// in `out` would otherwise put the partial somewhere else.
pub(super) fn paths(source: &Path, out: &Path) -> Result<PathBuf> {
    let source_real = std::fs::canonicalize(source).map_err(super::failed(source))?;
    let target = resolved(out)?;
    let partial = super::partial_output(&target);
    if partial.try_exists().map_err(super::failed(&partial))? {
        return Err(refused(format!(
            "{} is left from an earlier export that did not finish (or another export is writing it); \
             delete it, then export again",
            partial.display()
        )));
    }
    if target == source_real {
        return Err(refused(format!(
            "the output {} is the source; an export never converts in place",
            out.display()
        )));
    }
    if target.starts_with(&source_real) {
        return Err(refused(format!(
            "the output {} is inside the source {}",
            out.display(),
            source.display()
        )));
    }
    if source_real.starts_with(&target) {
        return Err(refused(format!(
            "the source {} is inside the output {}",
            source.display(),
            out.display()
        )));
    }
    if target.is_symlink() && !target.try_exists().map_err(super::failed(&target))? {
        return Err(refused(format!(
            "the output {} is a link to a directory that does not exist",
            out.display()
        )));
    }
    if target.try_exists().map_err(super::failed(&target))? {
        if !target.is_dir() {
            return Err(refused(format!(
                "the output {} is not a directory",
                out.display()
            )));
        }
        let mut entries = std::fs::read_dir(&target).map_err(super::failed(&target))?;
        if entries.next().is_some() {
            return Err(refused(format!(
                "the output {} is not empty; an export only writes into a new or empty directory",
                out.display()
            )));
        }
        if is_mount_point(&target).map_err(super::failed(&target))? {
            return Err(refused(format!(
                "the output {} is a mount point; the export is written beside it and renamed into \
                 place, which cannot cross filesystems: give a new directory inside it instead (for \
                 example {})",
                out.display(),
                target.join("jellyfin").display()
            )));
        }
    }
    Ok(target)
}

/// Whether `dir` sits on another filesystem than its parent.
#[cfg(unix)]
fn is_mount_point(dir: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    let Some(parent) = dir.parent() else {
        return Ok(true);
    };
    Ok(std::fs::metadata(dir)?.dev() != std::fs::metadata(parent)?.dev())
}

/// `path` with links resolved as far as it exists: the real path of its
/// longest existing prefix, then the rest of its components, `..` applied.
fn resolved(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).map_err(super::failed(path))?;
    let components: Vec<Component> = absolute.components().collect();
    let mut split = components.len();
    let mut prefix: PathBuf = components.iter().collect();
    while split > 1 && !prefix.exists() {
        split -= 1;
        prefix = components[..split].iter().collect();
    }
    let mut real = std::fs::canonicalize(&prefix).map_err(super::failed(&prefix))?;
    for component in &components[split..] {
        match component {
            Component::ParentDir => {
                real.pop();
            }
            Component::Normal(name) => real.push(name),
            _ => {}
        }
    }
    Ok(real)
}

/// Refuses a database another process holds open: a running server, or any
/// other program with a connection to it.
///
/// Every connection to a database in WAL mode holds a shared lock on the
/// "dead man switch" byte of its `-shm` index (byte 128: `UNIX_SHM_DMS` in
/// SQLite's `os_unix.c`), for as long as it is open, idle or not. Asking the
/// kernel whether a write lock on that byte would conflict (`F_GETLK`) says
/// whether another process has one, without taking a lock or writing a byte.
/// A database not in WAL mode has no `-shm`; the snapshot's copy is then only
/// unsafe while a write is under way, which a stopped server never does.
///
/// It only sees other processes: `F_GETLK` ignores the caller's own locks,
/// and opening and closing the `-shm` here would drop them. Never call it from
/// a process that has the database open.
pub(super) fn not_in_use(database: &Path) -> Result<()> {
    let shm = super::with_suffix(database, "-shm");
    if held_by_another_process(&shm).map_err(super::failed(&shm))? {
        return Err(refused(format!(
            "another process holds the database {} open; stop the server (or any program using it) first",
            database.display()
        )));
    }
    Ok(())
}

/// Whether another process holds a lock on the dead-man-switch byte of `shm`.
#[cfg(unix)]
fn held_by_another_process(shm: &Path) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    /// `UNIX_SHM_DMS`: `(22 + SQLITE_SHM_NLOCK) * 4 + SQLITE_SHM_NLOCK`.
    const DEAD_MAN_SWITCH: libc::off_t = 128;
    let file = match std::fs::File::open(shm) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    // SAFETY: an all-zero `flock` is a valid value of the plain C struct.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    // The lock-type constants fit the struct's narrower fields on every
    // platform `libc` defines them for.
    #[allow(clippy::cast_possible_truncation)]
    {
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
    }
    lock.l_start = DEAD_MAN_SWITCH;
    lock.l_len = 1;
    // SAFETY: the descriptor is open for the call, and `lock` is a valid
    // `flock` the call writes the conflicting lock (or `F_UNLCK`) into.
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_GETLK,
            std::ptr::from_mut(&mut lock),
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(i32::from(lock.l_type) != libc::F_UNLCK)
}

/// Walks what the copy will read under `source` (links followed, as the copy
/// follows them), and refuses an output inside a directory the source reaches
/// through a link (the copy would read what it writes), and an export the
/// partial's filesystem (the output's parent's) has too little free space for.
/// The export holds a copy of everything it reads and, while the snapshot is
/// taken, a second copy of the database and its log beside it.
pub(super) fn reach(source: &Path, database: &Path, target: &Path) -> Result<()> {
    let bytes = size(source, target)?;
    let database_files: u64 = ["", "-wal"]
        .iter()
        .map(|suffix| {
            std::fs::metadata(super::with_suffix(database, suffix)).map_or(0, |m| m.len())
        })
        .sum();
    let needed = bytes.saturating_add(database_files);
    let mut dir = target
        .parent()
        .map_or_else(|| target.to_owned(), Path::to_owned);
    while !dir.exists() {
        if !dir.pop() {
            break;
        }
    }
    let available = available(&dir).map_err(super::failed(&dir))?;
    if needed > available {
        return Err(refused(format!(
            "the export needs about {} ({needed} bytes: {} and a second copy of its database), and {} \
             has {} free: {} short",
            gib(needed),
            source.display(),
            dir.display(),
            gib(available),
            gib(needed - available)
        )));
    }
    Ok(())
}

/// `bytes` in GiB to one decimal, for a reader.
fn gib(bytes: u64) -> String {
    // Display only: a byte count far below 2^53 is exact as an f64.
    #[allow(clippy::cast_precision_loss)]
    let gib = bytes as f64 / f64::from(1_u32 << 30);
    format!("{gib:.1} GiB")
}

/// The bytes in every file under `root`, links followed, each directory
/// counted once. Refuses `target` as soon as the walk reaches a directory it
/// is, or is inside, through a link (`root` itself is [`paths`]' to refuse).
fn size(root: &Path, target: &Path) -> Result<u64> {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![root.to_owned()];
    let mut total = 0_u64;
    while let Some(dir) = pending.pop() {
        let real = std::fs::canonicalize(&dir).map_err(super::failed(&dir))?;
        if dir != root && target.starts_with(&real) {
            let relation = if target == real { "is" } else { "is inside" };
            return Err(refused(format!(
                "the output {} {relation} {}, which the source reaches through {}; the export \
                 would copy into what it reads",
                target.display(),
                real.display(),
                dir.display()
            )));
        }
        if !seen.insert(real) {
            continue;
        }
        for entry in std::fs::read_dir(&dir).map_err(super::failed(&dir))? {
            let path = entry.map_err(super::failed(&dir))?.path();
            // A broken link is the copy's to refuse; it has no size here.
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(path);
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

/// The bytes an unprivileged writer can still use on the filesystem holding
/// `dir`.
#[cfg(unix)]
fn available(dir: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt as _;

    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: an all-zero `statvfs` is a valid value of the plain C struct.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the
    // call, and `stat` is a valid `statvfs` the call writes into.
    if unsafe { libc::statvfs(c_path.as_ptr(), std::ptr::from_mut(&mut stat)) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // statvfs field widths differ per platform (u64 on Linux, u32 on macOS).
    #[allow(clippy::useless_conversion)]
    Ok(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

// TODO(Windows export): the in-use check needs a `LockFileEx` probe of the
// `-shm`'s dead-man-switch byte and the space check `GetDiskFreeSpaceExW`;
// until both exist the export refuses to run there rather than skip them.
#[cfg(not(unix))]
fn is_mount_point(_dir: &Path) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(not(unix))]
fn held_by_another_process(_shm: &Path) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the export cannot tell on this platform whether a server holds the database open",
    ))
}

#[cfg(not(unix))]
fn available(_dir: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the export cannot read the free space on this platform",
    ))
}

/// A database error the export reports as a refusal: SQLite found the file
/// damaged (`SQLITE_CORRUPT`) or not a database at all (`SQLITE_NOTADB`), so
/// the export cannot vouch for it. Any other error stays what it was.
pub(super) fn damaged_or(error: sqlx::Error) -> DbError {
    /// `SQLITE_CORRUPT` and `SQLITE_NOTADB`, the primary result codes.
    const CORRUPT: i64 = 11;
    const NOTADB: i64 = 26;
    let primary = error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .and_then(|code| code.parse::<i64>().ok())
        .map(|code| code & 0xff);
    if matches!(primary, Some(CORRUPT | NOTADB)) {
        return refused(format!("the database is damaged: {error}"));
    }
    error.into()
}

/// Refuses a database the export cannot vouch for, on the staged copy the
/// snapshot opens: one that fails `integrity_check` or `foreign_key_check`,
/// one that is not Ferrofin's (no `_sqlx_migrations`), and an adopted one
/// whose Jellyfin history matches no release Ferrofin adopts, so the export
/// cannot tell which Jellyfin it came from. A Ferrofin-native database has no
/// Jellyfin history.
pub(super) async fn database(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut *conn)
        .await
        .map_err(damaged_or)?;
    if integrity != ["ok"] {
        return Err(refused(format!(
            "the database fails its integrity check ({} problems; the first: {})",
            integrity.len(),
            integrity.first().map_or("", String::as_str)
        )));
    }
    let violations: Vec<String> =
        sqlx::query_scalar(r#"SELECT "table" FROM pragma_foreign_key_check"#)
            .fetch_all(&mut *conn)
            .await
            .map_err(damaged_or)?;
    if !violations.is_empty() {
        let mut tables = violations.clone();
        tables.sort();
        tables.dedup();
        return Err(refused(format!(
            "the database fails its foreign-key check: {} rows in {}",
            violations.len(),
            tables.join(", ")
        )));
    }
    let table = |name: &'static str| {
        sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        )
        .bind(name)
    };
    if table("_sqlx_migrations")
        .fetch_optional(&mut *conn)
        .await?
        .is_none()
    {
        return Err(refused(
            "the database is not a Ferrofin database (it has no _sqlx_migrations)".to_owned(),
        ));
    }
    if table("__EFMigrationsHistory")
        .fetch_optional(&mut *conn)
        .await?
        .is_some()
    {
        let applied: Vec<String> =
            sqlx::query_scalar(r#"SELECT "MigrationId" FROM "__EFMigrationsHistory" ORDER BY 1"#)
                .fetch_all(&mut *conn)
                .await?;
        if crate::database::match_generation(&applied).is_none() {
            return Err(refused(format!(
                "the database's Jellyfin migration history ({} ids) matches no Jellyfin release \
                 Ferrofin adopts, so the export cannot tell which release it came from",
                applied.len()
            )));
        }
    }
    Ok(())
}

/// Refuses a write-ahead log the snapshot would not fold in whole.
///
/// SQLite folds a log in up to its first frame that is torn or damaged (a bad
/// checksum, salts that are not the log's, no page number) and drops the
/// rest. After a crash that is only the frame being written, or the frames a
/// rolled-back transaction spilled there and a shorter commit overwrote the
/// front of: none of them was ever committed. A commit frame of the same log
/// (the header's salts, a database size) after the first bad one is a commit
/// SQLite would silently lose, so the export refuses. Frames with other salts
/// are a reused log's leftovers, which SQLite rightly ignores. The format is
/// SQLite's "WAL file format" (`walIndexRecover` in `wal.c`); the log is read
/// one frame at a time.
pub(super) fn wal(path: &Path) -> Result<()> {
    /// The WAL header and each frame header.
    const HEADER: usize = 32;
    const FRAME_HEADER: usize = 24;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(super::failed(path)(e)),
    };
    let mut log = std::io::BufReader::new(file);
    let mut header = [0_u8; HEADER];
    if !read_whole(&mut log, &mut header).map_err(super::failed(path))? {
        return Ok(());
    }
    let word = |at: &[u8]| u32::from_be_bytes([at[0], at[1], at[2], at[3]]);
    let big_endian = match word(&header) {
        0x377f_0682 => false,
        0x377f_0683 => true,
        // Not a log SQLite would read either.
        _ => return Ok(()),
    };
    let page_size = usize::try_from(word(&header[8..])).unwrap_or(0);
    let salts = (&header[16..20], &header[20..24]);
    let mut sums = checksum(&header[..24], big_endian, (0, 0));
    if !page_size.is_power_of_two()
        || !(512..=65536).contains(&page_size)
        || sums != (word(&header[24..]), word(&header[28..]))
    {
        return Ok(());
    }
    let mut frame = vec![0_u8; FRAME_HEADER + page_size];
    let mut index = 0_u64;
    let mut first_bad = None;
    while read_whole(&mut log, &mut frame).map_err(super::failed(path))? {
        index += 1;
        let ours = (&frame[8..12], &frame[12..16]) == salts;
        match first_bad {
            None => {
                sums = checksum(
                    &frame[FRAME_HEADER..],
                    big_endian,
                    checksum(&frame[..8], big_endian, sums),
                );
                if !ours
                    || word(&frame[..4]) == 0
                    || sums != (word(&frame[16..]), word(&frame[20..]))
                {
                    first_bad = Some(index);
                }
            }
            Some(bad) if ours && word(&frame[4..]) != 0 => {
                return Err(refused(format!(
                    "the database's write-ahead log is damaged at frame {bad}, and the commit in frame \
                     {index} (and any after it) would be lost, so the export cannot fold the log in. \
                     Restore a backup, or accept the loss: start the server on it once and stop it \
                     cleanly, then export again"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Fills `buf` from `reader`; `false` when the input ends first (a torn
/// frame, which SQLite ignores too).
fn read_whole(reader: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<bool> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// SQLite's WAL checksum of `data` (a multiple of 8 bytes) from `sums`.
fn checksum(data: &[u8], big_endian: bool, (mut s0, mut s1): (u32, u32)) -> (u32, u32) {
    for pair in data.as_chunks::<8>().0 {
        let read = |at: &[u8]| {
            let bytes = [at[0], at[1], at[2], at[3]];
            if big_endian {
                u32::from_be_bytes(bytes)
            } else {
                u32::from_le_bytes(bytes)
            }
        };
        s0 = s0.wrapping_add(read(&pair[..4])).wrapping_add(s1);
        s1 = s1.wrapping_add(read(&pair[4..])).wrapping_add(s0);
    }
    (s0, s1)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sqlx::ConnectOptions as _;

    use super::*;

    fn reason<T: std::fmt::Debug>(result: Result<T>) -> String {
        match result {
            Err(DbError::ExportRefused { reason }) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// Every nesting of the output and the source is refused, and so are a
    /// non-empty output, a file in its place and an interrupted export; the
    /// target returned is the output resolved, whatever its spelling.
    #[test]
    fn unsafe_outputs_are_refused() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("source");
        std::fs::create_dir_all(source.join("data")).expect("source");
        let check = |out: &Path| paths(&source, out);

        assert!(reason(check(&source)).contains("is the source"));
        assert!(reason(check(&source.join("data/out"))).contains("inside the source"));
        assert!(reason(check(&source.join("data/../export"))).contains("inside the source"));
        assert!(reason(check(scratch.path())).contains("inside the output"));
        // A path that does not exist yet but resolves to a directory holding
        // the source.
        assert!(reason(check(&scratch.path().join("missing/.."))).contains("inside the output"));
        let full = scratch.path().join("full");
        std::fs::create_dir_all(&full).expect("dir");
        std::fs::write(full.join("x"), b"x").expect("file");
        assert!(reason(check(&full)).contains("is not empty"));

        let file = scratch.path().join("a-file");
        std::fs::write(&file, b"x").expect("file");
        assert!(reason(check(&file)).contains("is not a directory"));

        let out = scratch.path().join("out");
        std::fs::create_dir_all(scratch.path().join("out.partial")).expect("partial");
        assert!(reason(check(&out)).contains("did not finish"));
        // Its spelling with a trailing `/` names the same partial.
        assert!(reason(check(&scratch.path().join("out/"))).contains("did not finish"));
        std::fs::remove_dir(scratch.path().join("out.partial")).expect("cleanup");

        // A new directory, and an empty one, are fine, and resolve the same.
        let real = std::fs::canonicalize(scratch.path())
            .expect("real")
            .join("out");
        assert_eq!(check(&out).expect("a new output"), real);
        std::fs::create_dir_all(&out).expect("empty output");
        assert_eq!(
            check(&scratch.path().join("out/")).expect("an empty output"),
            real
        );
        // A link to it resolves to it, so the rename lands on the directory.
        let link = scratch.path().join("link");
        std::os::unix::fs::symlink(&out, &link).expect("link");
        assert_eq!(check(&link).expect("a link to an empty output"), real);
        let dangling = scratch.path().join("dangling");
        std::os::unix::fs::symlink(scratch.path().join("nowhere"), &dangling).expect("link");
        assert!(reason(check(&dangling)).contains("does not exist"));
    }

    /// A directory on its own filesystem is a mount point; one beside its
    /// parent's files is not.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_mount_point_is_told_apart() {
        let scratch = tempfile::tempdir().expect("tempdir");
        assert!(is_mount_point(Path::new("/proc")).expect("/proc"));
        std::fs::create_dir(scratch.path().join("sub")).expect("dir");
        assert!(!is_mount_point(&scratch.path().join("sub")).expect("sub"));
    }

    /// No `-shm` means no WAL connection; an `-shm` nobody else holds is free.
    /// (Another process holding it is checked through the binary, since a
    /// process never conflicts with its own locks.)
    #[test]
    fn a_database_nobody_holds_is_not_in_use() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let database = scratch.path().join("jellyfin.db");
        not_in_use(&database).expect("no -shm");
        std::fs::write(
            super::super::with_suffix(&database, "-shm"),
            vec![0_u8; 32768],
        )
        .expect("shm");
        not_in_use(&database).expect("an -shm nobody holds");
    }

    /// The free-space check counts what the source holds, and the database a
    /// second time, against what the partial's filesystem has.
    #[test]
    fn the_source_size_is_counted_and_compared() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let source = scratch.path().join("source");
        std::fs::create_dir_all(source.join("a/b")).expect("dirs");
        std::fs::write(source.join("a/one"), vec![0_u8; 1000]).expect("file");
        std::fs::write(source.join("a/b/two"), vec![0_u8; 24]).expect("file");
        assert_eq!(
            size(&source, &scratch.path().join("out")).expect("size"),
            1024
        );
        let database = source.join("a/one");
        reach(&source, &database, &scratch.path().join("out")).expect("2 KiB fits");

        // An output under a directory the source links to is refused, naming
        // the link.
        let volume = scratch.path().join("volume");
        std::fs::create_dir(&volume).expect("volume");
        std::os::unix::fs::symlink(&volume, source.join("a/linked")).expect("link");
        let real = std::fs::canonicalize(&volume).expect("real");
        let message = reason(reach(&source, &database, &real.join("export")));
        assert!(
            message.contains("reaches through") && message.contains("linked"),
            "{message}"
        );
        std::fs::remove_file(source.join("a/linked")).expect("unlink");

        // A sparse file one GiB larger than the free space is counted at its
        // length, though it occupies none of it.
        let free = available(scratch.path()).expect("statvfs");
        let sparse = std::fs::File::create(source.join("sparse")).expect("file");
        // shortcut: a filesystem whose largest file is below its free space
        // (ext4 past 16 TiB free) cannot hold it; the refusal is then left to
        // the binary test, which sizes its file to the filesystem.
        if sparse.set_len(free + (1 << 30)).is_err() {
            return;
        }
        let message = reason(reach(&source, &database, &scratch.path().join("out")));
        assert!(message.contains("short"), "{message}");
        assert!(message.contains("GiB"), "{message}");
    }

    /// A log written by SQLite and truncated mid-frame folds in; one whose
    /// middle frame is damaged, with frames of the same log after it, is
    /// refused.
    #[tokio::test]
    async fn a_log_damaged_before_its_end_is_refused() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let live = scratch.path().join("live.db");
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&live)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .pragma("wal_autocheckpoint", "0")
            .connect()
            .await
            .expect("connect");
        sqlx::query("CREATE TABLE Items (Name TEXT)")
            .execute(&mut conn)
            .await
            .expect("table");
        for i in 0..20 {
            sqlx::query("INSERT INTO Items VALUES (?1)")
                .bind(format!("item {i}"))
                .execute(&mut conn)
                .await
                .expect("insert");
        }
        let log = super::super::with_suffix(&live, "-wal");
        let bytes = std::fs::read(&log).expect("log");
        sqlx::Connection::close(conn).await.expect("close");
        let page_size = usize::try_from(u32::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
        ]))
        .expect("page size");
        let frame = 24 + page_size;
        assert!((bytes.len() - 32) / frame > 4, "the log has frames");
        let copy = scratch.path().join("copy.db-wal");

        std::fs::write(&copy, &bytes).expect("write");
        wal(&copy).expect("an intact log");
        std::fs::write(&copy, &bytes[..bytes.len() - frame / 2]).expect("write");
        wal(&copy).expect("a log torn in its last frame");
        let mut damaged = bytes.clone();
        damaged[32 + frame + 24 + 100] ^= 0xff;
        std::fs::write(&copy, &damaged).expect("write");
        let message = reason(wal(&copy));
        assert!(message.contains("damaged at frame 2"), "{message}");
        wal(&scratch.path().join("missing-wal")).expect("no log");
    }

    /// The frames a rolled-back transaction spilled into the log, whose front
    /// a shorter commit then overwrote, fail the checksum chain but were never
    /// committed: SQLite folds the log in with nothing lost, and so does the
    /// export.
    #[tokio::test]
    async fn a_rolled_back_transactions_frames_are_not_a_loss() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let live = scratch.path().join("live.db");
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&live)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .pragma("wal_autocheckpoint", "0")
            .pragma("cache_size", "10")
            .connect()
            .await
            .expect("connect");
        for statement in [
            "CREATE TABLE Items (Name TEXT)",
            "BEGIN",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000) \
             INSERT INTO Items SELECT printf('%0200d', i) FROM n",
            "ROLLBACK",
            "INSERT INTO Items VALUES ('kept')",
        ] {
            sqlx::query(statement)
                .execute(&mut conn)
                .await
                .expect(statement);
        }
        let log = super::super::with_suffix(&live, "-wal");
        let bytes = std::fs::read(&log).expect("log");
        sqlx::Connection::close(conn).await.expect("close");
        let page_size = usize::try_from(u32::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
        ]))
        .expect("page size");
        assert!(
            (bytes.len() - 32) / (24 + page_size) > 20,
            "the rollback spilled into the log"
        );
        let copy = scratch.path().join("copy.db-wal");
        std::fs::write(&copy, &bytes).expect("write");
        wal(&copy).expect("a log with a rolled-back tail");
    }
}
