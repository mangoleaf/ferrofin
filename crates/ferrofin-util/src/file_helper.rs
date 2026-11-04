//! Port of `FileHelper.cs` — create-or-truncate a file at a path, plus the
//! directory-writability probe the extraction paths pre-flight with — and the
//! atomic and private (`0600` on Unix) writes Ferrofin's config, state and
//! backup files go through.

use std::io;
use std::path::Path;

/// Replaces a file only after its complete new contents have reached disk.
///
/// The temporary file is private and lives beside the destination so rename
/// is atomic. A failed write or rename leaves the previous file intact.
/// Call from a blocking worker when used by an async service.
///
/// The result is private (`0600` on Unix), for config and state files. A file
/// written into a media folder uses [`atomic_write_media`].
///
/// # Errors
///
/// Returns a filesystem error without truncating the destination.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    atomic_write_with(path, |file| file.write_all(contents))
}

/// [`atomic_write`] with the contents written by `write`, for contents that
/// come from a stream (a file restored from a backup archive).
///
/// The result is private (`0600` on Unix), whatever mode a file it replaces
/// had.
///
/// # Errors
///
/// Returns the error `write` or the filesystem raised, without truncating the
/// destination.
pub fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    atomic_replace_with_permissions(path, true, write)
}

/// Creates, or truncates, a private file at `path` and opens it for writing:
/// `0600` on Unix whether the file is new or already there, for a file that
/// holds secrets but is written in place (a backup archive, which holds the
/// configuration tree). On other platforms it is [`std::fs::File::create`].
///
/// # Errors
///
/// Returns the error from opening the file, or from narrowing the mode of one
/// already there (on Unix, a file another user owns).
pub fn create_private(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    private_mode(&mut options);
    let file = options.open(path)?;
    // The creation mode applies only to a new file: one already there keeps
    // its own until narrowed here.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if file.metadata()?.permissions().mode() & 0o777 != PRIVATE_MODE {
            file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_MODE))?;
        }
    }
    Ok(file)
}

/// The mode of a private file: owner read and write only.
#[cfg(unix)]
const PRIVATE_MODE: u32 = 0o600;

/// Makes `options` create its file private (`0600` on Unix; a no-op elsewhere).
fn private_mode(options: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(PRIVATE_MODE);
    }
    #[cfg(not(unix))]
    let _ = options;
}

/// Replaces media-adjacent artwork atomically with ordinary file permissions.
///
/// New files honor the process umask. Existing files retain their permissions,
/// clearing the read-only owner flag as ImageSaver does before replacement.
///
/// # Errors
/// Returns a filesystem error without truncating the destination.
pub fn atomic_write_media(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    atomic_replace_with_permissions(path, false, |file| file.write_all(contents))
}

fn atomic_replace_with_permissions(
    path: &Path,
    private: bool,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".ferrofin-config-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    if private {
        private_mode(&mut options);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        if !private
            && let Ok(metadata) = std::fs::metadata(path)
            && metadata.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(
                    metadata.permissions().mode() | 0o200,
                ))?;
            }
            #[cfg(not(unix))]
            {
                let mut permissions = metadata.permissions();
                // Not Unix, so this clears only the read-only attribute: the
                // lint's world-writable risk is the Unix meaning.
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                file.set_permissions(permissions)?;
            }
        }
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Creates, or truncates, a file at the specified path.
///
/// # Errors
///
/// Returns any I/O error raised while creating/truncating the file.
pub fn create_empty<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::File::create(path)?;
    Ok(())
}

/// Creates `dir` if missing and proves the current process can write into it,
/// by creating and removing a uniquely-named probe file.
///
/// Mode bits are deliberately not inspected: they lie under ACLs, mapped uids,
/// and read-only mounts. Only a real write answers the question.
///
/// This exists because a directory the server cannot write is invisible at the
/// point it hurts. A container that once ran as root leaves root-owned
/// subdirectories on its volume; after it drops to an unprivileged uid, every
/// ffmpeg frame extraction into that directory silently produces no file while
/// ffmpeg itself is perfectly healthy. Probing turns that into one clear error
/// naming the path.
///
/// # Errors
///
/// Returns the I/O error from creating the directory, or from the probe write,
/// so the caller can report the real reason (typically `PermissionDenied`).
pub fn ensure_writable_dir<P: AsRef<Path>>(dir: P) -> io::Result<()> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".ferrofin-write-probe-{}", uuid::Uuid::new_v4()));
    std::fs::File::create(&probe)?;
    // Best-effort cleanup: the write already answered the question, and a
    // failed unlink must not turn a writable directory into an error.
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    #[cfg(unix)]
    #[test]
    fn media_writes_honor_umask_and_preserve_existing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir().join(format!("ferrofin-media-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let expected = dir.join("ordinary");
        std::fs::write(&expected, b"ordinary").unwrap();
        let path = dir.join("artwork");
        atomic_write_media(&path, b"new artwork").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode(),
            std::fs::metadata(&expected).unwrap().permissions().mode()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o440)).unwrap();
        atomic_write_media(&path, b"replacement").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// gl-1233: a private file is `0600` whether it is new or replaces, or
    /// truncates, one with the umask mode.
    #[cfg(unix)]
    #[test]
    fn private_files_are_owner_only_new_or_not() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = temp_dir().join(format!("ferrofin-private-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("archive.zip");
        create_private(&path).unwrap().write_all(b"new").unwrap();
        assert_eq!(mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        create_private(&path).unwrap().write_all(b"again").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"again", "truncated");

        let restored = dir.join("config.json");
        std::fs::write(&restored, b"old").unwrap();
        std::fs::set_permissions(&restored, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_with(&restored, |file| {
            std::io::copy(&mut &b"streamed"[..], file).map(drop)
        })
        .unwrap();
        assert_eq!(mode(&restored), 0o600);
        assert_eq!(std::fs::read(&restored).unwrap(), b"streamed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_preserves_previous_file_when_a_write_fails() {
        use std::io::Write as _;
        let dir = temp_dir().join(format!("ferrofin-atomic-{}", uuid::Uuid::new_v4()));
        let path = dir.join("settings.json");
        atomic_write(&path, b"original").unwrap();
        let result = atomic_write_with(&path, |file| {
            file.write_all(b"incomplete")?;
            Err(io::Error::other("disk full"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        atomic_write(&path, b"replacement").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        // A rename failure also cleans up its temporary file.
        assert!(atomic_write(&dir, b"cannot replace a directory").is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ensure_writable_dir_creates_and_probes() {
        let dir = temp_dir().join(format!("ferrofin-probe-{}", uuid::Uuid::new_v4()));
        ensure_writable_dir(&dir).expect("a fresh directory is writable");
        assert!(dir.is_dir());
        // The probe file must not be left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    // The case that cost days to diagnose: the directory exists, the process
    // cannot write into it, and nothing about `create_dir_all` says so.
    #[cfg(unix)]
    #[test]
    fn ensure_writable_dir_rejects_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir().join(format!("ferrofin-probe-ro-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        // Running as root ignores the mode bits. Establish whether the mode is
        // meaningful for this uid FIRST, then require the error — asserting
        // only inside an `if let Err` would pass just as happily against a
        // probe that never checks anything.
        let writable_anyway = std::fs::File::create(dir.join("root-can-write")).is_ok();
        if writable_anyway {
            let _ = std::fs::remove_file(dir.join("root-can-write"));
        } else {
            let err = ensure_writable_dir(&dir).expect_err("a read-only directory must be refused");
            assert_eq!(
                err.kind(),
                io::ErrorKind::PermissionDenied,
                "the probe must surface the real reason"
            );
        }

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("restore");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn create_empty_valid_correct() {
        let path = temp_dir().join(format!("ferrofin-{}.tmp", uuid::Uuid::new_v4()));
        assert!(!path.exists());

        create_empty(&path).expect("create should succeed");
        assert!(path.exists());

        std::fs::remove_file(&path).expect("cleanup");
    }
}
