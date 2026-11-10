//! The file half of an export: copies a data directory's files into the
//! output, and digests the source so the export can show it left the source
//! as it found it.
//!
//! Symbolic links are followed: the output holds what a link points at, never
//! a link, so nothing written into the output (by the export, or later by
//! Jellyfin) can reach the source through one. A directory reached twice, by
//! a link loop or by two links to one directory, is refused.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::failed;
use crate::{DbError, Result};

/// What one path is, as two digests compare it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A directory. Its contents are entries of their own.
    Dir,
    /// A regular file, by the SHA-256 of its bytes.
    File([u8; 32]),
}

/// Paths with what each is: relative to a directory for [`digest`] and
/// [`copy`], as given for [`digest_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TreeDigest(BTreeMap<PathBuf, Entry>);

impl TreeDigest {
    /// The paths that differ between `self` and `after`: added, removed, or
    /// holding something else. Sorted.
    pub(super) fn changes(&self, after: &Self) -> Vec<String> {
        let mut changed: Vec<&PathBuf> = self
            .0
            .iter()
            .filter(|(path, entry)| after.0.get(*path) != Some(entry))
            .map(|(path, _)| path)
            .collect();
        changed.extend(after.0.keys().filter(|path| !self.0.contains_key(*path)));
        changed.sort();
        changed
            .into_iter()
            .map(|path| path.display().to_string())
            .collect()
    }
}

/// What [`copy`] read and wrote.
#[derive(Debug, Default)]
pub(super) struct Copied {
    /// The source as it was read.
    pub(super) digest: TreeDigest,
    /// The regular files written to the output.
    pub(super) files: u64,
    /// Their size in bytes.
    pub(super) bytes: u64,
    /// (copy, source) of every directory written, for [`finish`].
    pub(super) dirs: Vec<(PathBuf, PathBuf)>,
}

/// The files [`copy`] digests but does not copy, matched by path and, for a
/// file of the same name, by real path: a database file is left out whether
/// the walk reaches it by the path given or through a linked directory. A
/// link of another name to it is copied as a file.
#[derive(Debug, Default)]
pub(super) struct Skip {
    /// Their file names: only a file of one of these names is resolved.
    names: HashSet<OsString>,
    /// The paths as given and, for those that exist, their real paths.
    paths: HashSet<PathBuf>,
}

impl Skip {
    /// The files `files` name, each of which may or may not exist.
    pub(super) fn new(files: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut skip = Self::default();
        for file in files {
            if let Some(name) = file.file_name() {
                skip.names.insert(name.to_owned());
            }
            if file.try_exists().map_err(failed(&file))? {
                skip.paths
                    .insert(std::fs::canonicalize(&file).map_err(failed(&file))?);
            }
            skip.paths.insert(file);
        }
        Ok(skip)
    }

    /// Whether the file at `path` is one of them.
    fn holds(&self, path: &Path) -> Result<bool> {
        if self.paths.contains(path) {
            return Ok(true);
        }
        if !path
            .file_name()
            .is_some_and(|name| self.names.contains(name))
        {
            return Ok(false);
        }
        Ok(self
            .paths
            .contains(&std::fs::canonicalize(path).map_err(failed(path))?))
    }
}

/// A path's kind, links followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
}

/// The size of the buffer a file is read through.
const BUFFER: usize = 1 << 16;

/// Digests every path under `root`.
pub(super) fn digest(root: &Path) -> Result<TreeDigest> {
    Ok(walk(root, None, &Skip::default())?.digest)
}

/// Digests each of `files` that exists, keyed by the path as given.
pub(super) fn digest_files(files: &[PathBuf]) -> Result<TreeDigest> {
    let mut digest = TreeDigest::default();
    let mut buffer = vec![0_u8; BUFFER];
    for file in files {
        if file.try_exists().map_err(failed(file))? {
            let (sha, _) = read_file(file, None, &mut buffer)?;
            digest.0.insert(file.clone(), Entry::File(sha));
        }
    }
    Ok(digest)
}

/// Copies every path under `root` to the same place under `out`, keeping each
/// file's permissions and modification time, except the files in `skip`,
/// which are digested only.
///
/// Directories are made private to the owner. [`finish`] gives them their
/// source's permissions and time once nothing more is written into them, so a
/// copy that stops part-way never leaves a private file in an open directory.
///
/// The whole tree is listed before anything is written, so an `out` inside
/// `root` is never copied into itself (a later digest finds it as added).
pub(super) fn copy(root: &Path, out: &Path, skip: &Skip) -> Result<Copied> {
    walk(root, Some(out), skip)
}

/// Gives each copied directory its source's permissions and, on Unix,
/// modification time: deepest first, since writing into a directory changes
/// its time, and a read-only one would refuse what it holds.
pub(super) fn finish(dirs: &[(PathBuf, PathBuf)]) -> Result<()> {
    for (to, from) in dirs.iter().rev() {
        finish_dir(from, to)?;
    }
    Ok(())
}

/// Makes `dir`, private to the owner on Unix, and any missing parent with the
/// default permissions.
pub(super) fn private_dir(dir: &Path) -> std::io::Result<()> {
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        created => created,
    }
}

/// [`digest`], and with `out` [`copy`].
fn walk(root: &Path, out: Option<&Path>, skip: &Skip) -> Result<Copied> {
    let listing = list(root)?;
    let mut copied = Copied::default();
    let mut buffer = vec![0_u8; BUFFER];
    if let Some(out) = out {
        private_dir(out).map_err(failed(out))?;
        copied.dirs.push((out.to_owned(), root.to_owned()));
    }
    for (path, kind) in listing {
        let from = root.join(&path);
        let to = out.map(|out| out.join(&path));
        let entry = match kind {
            Kind::Dir => {
                if let Some(to) = to {
                    private_dir(&to).map_err(failed(&to))?;
                    copied.dirs.push((to, from));
                }
                Entry::Dir
            }
            Kind::File => {
                let to = if to.is_some() && skip.holds(&from)? {
                    None
                } else {
                    to
                };
                let (sha, len) = read_file(&from, to.as_deref(), &mut buffer)?;
                if to.is_some() {
                    copied.files += 1;
                    copied.bytes += len;
                }
                Entry::File(sha)
            }
        };
        copied.digest.0.insert(path, entry);
    }
    Ok(copied)
}

/// Every path under `root`, relative to it, with its kind, links followed, in
/// path order (a directory before what it holds).
fn list(root: &Path) -> Result<Vec<(PathBuf, Kind)>> {
    let mut found = Vec::new();
    // Each directory listed, by where it really is, with the path it was reached by.
    let mut seen = HashMap::new();
    seen.insert(
        std::fs::canonicalize(root).map_err(failed(root))?,
        PathBuf::new(),
    );
    let mut pending = vec![PathBuf::new()];
    while let Some(dir) = pending.pop() {
        let at = root.join(&dir);
        for entry in std::fs::read_dir(&at).map_err(failed(&at))? {
            let entry = entry.map_err(failed(&at))?;
            let path = dir.join(entry.file_name());
            let full = root.join(&path);
            // `metadata` follows a link; a broken one fails here.
            let metadata = match std::fs::metadata(&full) {
                Ok(metadata) => metadata,
                Err(error) if entry.file_type().is_ok_and(|kind| kind.is_symlink()) => {
                    return Err(refused(
                        &full,
                        &format!("a symbolic link whose target cannot be read: {error}"),
                    ));
                }
                Err(error) => return Err(failed(&full)(error)),
            };
            let kind = if metadata.is_dir() {
                let real = std::fs::canonicalize(&full).map_err(failed(&full))?;
                if let Some(first) = seen.insert(real, path.clone()) {
                    let first = if first.as_os_str().is_empty() {
                        "the data directory itself".to_owned()
                    } else {
                        format!("`{}`", first.display())
                    };
                    return Err(refused(
                        &full,
                        &format!(
                            "the same directory as {first}, reached twice through a symbolic link"
                        ),
                    ));
                }
                pending.push(path.clone());
                Kind::Dir
            } else if metadata.is_file() {
                Kind::File
            } else {
                return Err(refused(&full, "not a file or a directory"));
            };
            found.push((path, kind));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Reads `from` once, hashing it and, with `to`, writing the same bytes there
/// with `from`'s permissions and modification time. Returns the SHA-256 and the
/// length.
fn read_file(from: &Path, to: Option<&Path>, buffer: &mut [u8]) -> Result<([u8; 32], u64)> {
    let mut input = std::fs::File::open(from).map_err(failed(from))?;
    let metadata = input.metadata().map_err(failed(from))?;
    let mut output = to
        .map(|to| create(to, &metadata.permissions()).map_err(failed(to)))
        .transpose()?;
    let mut hasher = Sha256::new();
    let mut len = 0_u64;
    loop {
        let read = input.read(buffer).map_err(failed(from))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let (Some(output), Some(to)) = (output.as_mut(), to) {
            output.write_all(&buffer[..read]).map_err(failed(to))?;
        }
        len += read as u64;
    }
    if let (Some(output), Some(to)) = (output, to) {
        let modified = metadata.modified().map_err(failed(from))?;
        output.set_modified(modified).map_err(failed(to))?;
        std::fs::set_permissions(to, metadata.permissions()).map_err(failed(to))?;
    }
    Ok((hasher.finalize().into(), len))
}

/// Creates (or truncates) `to` for writing. On Unix a new file starts with no
/// more permissions than `like`, so a private file is never readable by others
/// while it is written.
pub(super) fn create(to: &Path, like: &std::fs::Permissions) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        options.mode(like.mode());
    }
    #[cfg(not(unix))]
    let _ = like;
    options.open(to)
}

/// Gives the copied directory `to` the permissions and, on Unix, the
/// modification time of `from`.
fn finish_dir(from: &Path, to: &Path) -> Result<()> {
    let metadata = std::fs::metadata(from).map_err(failed(from))?;
    #[cfg(unix)]
    {
        let modified = metadata.modified().map_err(failed(from))?;
        std::fs::File::open(to)
            .and_then(|dir| dir.set_modified(modified))
            .map_err(failed(to))?;
    }
    std::fs::set_permissions(to, metadata.permissions()).map_err(failed(to))
}

/// [`DbError::Export`] for `path`, which the export does not copy, and why.
fn refused(path: &Path, why: &str) -> DbError {
    failed(path)(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        why.to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Skip, copy, digest, digest_files, finish};

    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn copies_files_and_skips_the_named_ones_but_digests_them() {
        let dir = tempfile::tempdir().unwrap();
        let (source, out) = (dir.path().join("source"), dir.path().join("out"));
        write(&source, "a.txt", b"one");
        write(&source, "deep/er/b.bin", &[0, 1, 2]);
        write(&source, "skipped.db", b"database");
        std::fs::create_dir(source.join("empty")).unwrap();

        let skip = Skip::new([source.join("skipped.db")]).unwrap();
        let copied = copy(&source, &out, &skip).unwrap();

        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(out.join("deep/er/b.bin")).unwrap(), [0, 1, 2]);
        assert!(out.join("empty").is_dir());
        assert!(!out.join("skipped.db").exists());
        assert_eq!((copied.files, copied.bytes), (2, 6));
        // The skipped file is in the digest all the same, so a change to it is seen.
        write(&source, "skipped.db", b"changed!");
        assert_eq!(
            copied.digest.changes(&digest(&source).unwrap()),
            ["skipped.db"]
        );
    }

    #[test]
    fn changes_name_every_added_removed_and_changed_path() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path();
        write(source, "kept", b"same");
        write(source, "changed", b"before");
        write(source, "removed", b"gone soon");
        let before = digest(source).unwrap();

        write(source, "changed", b"after");
        std::fs::remove_file(source.join("removed")).unwrap();
        write(source, "added/new", b"new");
        let after = digest(source).unwrap();

        assert_eq!(
            before.changes(&after),
            ["added", "added/new", "changed", "removed"]
        );
        assert_eq!(after.changes(&after), Vec::<String>::new());
    }

    #[test]
    fn digest_files_skips_missing_files_and_sees_a_changed_one() {
        let dir = tempfile::tempdir().unwrap();
        let (present, missing) = (dir.path().join("a-wal"), dir.path().join("a-shm"));
        std::fs::write(&present, b"log").unwrap();
        let before = digest_files(&[present.clone(), missing.clone()]).unwrap();

        std::fs::write(&present, b"longer log").unwrap();
        let after = digest_files(&[present.clone(), missing]).unwrap();

        assert_eq!(before.changes(&after), [present.display().to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_followed_and_its_target_copied() {
        let dir = tempfile::tempdir().unwrap();
        let (source, out) = (dir.path().join("source"), dir.path().join("out"));
        write(dir.path(), "elsewhere/inner.txt", b"x");
        write(&source, "real", b"y");
        std::os::unix::fs::symlink("../elsewhere", source.join("linked")).unwrap();
        std::os::unix::fs::symlink("real", source.join("file-link")).unwrap();

        copy(&source, &out, &Skip::default()).unwrap();

        let linked = std::fs::symlink_metadata(out.join("linked")).unwrap();
        assert!(linked.is_dir(), "a real directory, not a link");
        assert_eq!(std::fs::read(out.join("linked/inner.txt")).unwrap(), b"x");
        assert!(
            std::fs::symlink_metadata(out.join("file-link"))
                .unwrap()
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn skip_matches_a_file_by_its_real_path_and_spares_one_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        write(&real, "ferrofin.db", b"database");
        write(&real, "other/ferrofin.db", b"not the database");
        // The walk reaches the database through a link; the skip names its real path.
        std::os::unix::fs::symlink(&real, dir.path().join("via")).unwrap();
        let out = dir.path().join("out");

        let skip = Skip::new([real.join("ferrofin.db")]).unwrap();
        let copied = copy(&dir.path().join("via"), &out, &skip).unwrap();

        assert!(!out.join("ferrofin.db").exists());
        assert_eq!(
            std::fs::read(out.join("other/ferrofin.db")).unwrap(),
            b"not the database"
        );
        assert_eq!(copied.files, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_loop_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "sub/file", b"x");
        std::os::unix::fs::symlink("..", dir.path().join("sub/up")).unwrap();

        let error = digest(dir.path()).unwrap_err().to_string();

        assert!(
            error.contains("the same directory as the data directory itself"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn modes_and_modification_times_are_kept() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (source, out) = (dir.path().join("source"), dir.path().join("out"));
        let secret = source.join("private/secret.toml");
        write(&source, "private/secret.toml", b"password");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::options()
            .write(true)
            .open(&secret)
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(
            source.join("private"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();

        let copied = copy(&source, &out, &Skip::default()).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&out.join("private")), 0o700, "private until finished");
        finish(&copied.dirs).unwrap();

        assert_eq!(mode(&out.join("private/secret.toml")), 0o600);
        assert_eq!(mode(&out.join("private")), 0o750);
        assert_eq!(mode(&out), mode(&source));
        let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(modified(&out.join("private/secret.toml")), old);
        assert_eq!(
            modified(&out.join("private")),
            modified(&source.join("private"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_special_file_is_refused() {
        use std::os::fd::AsRawFd as _;
        let dir = tempfile::tempdir().unwrap();
        // A socket path must be short; reach the directory through its descriptor.
        let handle = std::fs::File::open(dir.path()).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(format!(
            "/proc/self/fd/{}/sock",
            handle.as_raw_fd()
        ))
        .unwrap();

        let error = digest(dir.path()).unwrap_err().to_string();

        assert!(error.contains("not a file or a directory"), "{error}");
        drop(socket);
    }
}
