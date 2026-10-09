//! Directory roots that follow a live configuration without moving in-flight work.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Creates and checks a transcode directory's Jellyfin ownership marker.
///
/// Matches `GetTranscodePath` / `CreateAndCheckMarker`: conflicting marker
/// files anywhere below the directory reject its use; marker enumeration
/// failures are ignored, and creating the directory or marker may still fail.
///
/// # Errors
/// Returns a filesystem error or a conflicting directory marker.
pub fn prepare_transcode_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    if let Ok(Some(conflict)) = conflicting_transcode_marker(path) {
        return Err(std::io::Error::other(format!(
            "Expected to find only .jellyfin-transcode but found marker for {}.",
            conflict.display()
        )));
    }
    let marker = path.join(".jellyfin-transcode");
    if !transcode_marker_exists(&marker) {
        crate::file_helper::create_empty(marker)?;
    }
    Ok(())
}

/// File.Exists on Unix retains every existing non-directory entry, including
/// dangling links and special files. Opening a FIFO to create a marker would
/// wait for a reader, so this check must not require a regular resolved file.
fn transcode_marker_exists(path: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return std::fs::metadata(path).map_or(true, |target| !target.is_dir());
        }
        !metadata.is_dir()
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn conflicting_transcode_marker(path: &Path) -> std::io::Result<Option<PathBuf>> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            if let Some(conflict) = conflicting_transcode_marker(&child)? {
                return Ok(Some(conflict));
            }
        } else {
            // Directory.EnumerateFiles includes non-directory entries, also
            // a dangling symlink whose target does not satisfy is_file().
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".jellyfin-") && !name.eq_ignore_ascii_case(".jellyfin-transcode") {
                return Ok(Some(child));
            }
        }
    }
    Ok(None)
}

/// A fixed directory or a resolver consulted when an operation starts.
/// Resolve once and retain that snapshot for all I/O belonging to the operation.
#[derive(Clone)]
pub struct DirectoryPath(Arc<dyn Fn() -> PathBuf + Send + Sync>);

impl DirectoryPath {
    /// Resolves a directory from the current application configuration.
    pub fn live(resolve: impl Fn() -> PathBuf + Send + Sync + 'static) -> Self {
        Self(Arc::new(resolve))
    }

    /// Takes a directory snapshot for an operation.
    #[must_use]
    pub fn resolve(&self) -> PathBuf {
        (self.0)()
    }

    /// Resolves a child path now.
    #[must_use]
    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.resolve().join(path)
    }

    /// Creates a child directory that continues to follow its parent's resolver.
    #[must_use]
    pub fn child(&self, path: impl Into<PathBuf>) -> Self {
        let parent = self.clone();
        let path = path.into();
        Self::live(move || parent.join(&path))
    }
}

impl Default for DirectoryPath {
    fn default() -> Self {
        Self::from(PathBuf::new())
    }
}

impl std::fmt::Debug for DirectoryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.resolve().fmt(f)
    }
}

impl<T: Into<PathBuf>> From<T> for DirectoryPath {
    fn from(path: T) -> Self {
        let path = path.into();
        Self::live(move || path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    #[test]
    fn transcode_directory_marker_is_created_and_matching_nested_markers_are_allowed() {
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).expect("nested directory");
        std::fs::write(nested.join(".jellyfin-TRANSCODE"), b"").expect("nested marker");
        prepare_transcode_directory(&root).expect("prepare");
        assert!(root.join(".jellyfin-transcode").is_file());
        prepare_transcode_directory(&root).expect("idempotent");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn transcode_directory_rejects_conflicting_markers_recursively() {
        for nested in [false, true] {
            let root =
                std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
            let owner = if nested {
                root.join("a/b")
            } else {
                root.clone()
            };
            std::fs::create_dir_all(&owner).expect("directory");
            let marker = owner.join(".jellyfin-data");
            std::fs::write(&marker, b"").expect("conflicting marker");
            let error = prepare_transcode_directory(&root).expect_err("conflicting owner");
            assert_eq!(
                error.to_string(),
                format!(
                    "Expected to find only .jellyfin-transcode but found marker for {}.",
                    marker.display(),
                )
            );
            assert!(!root.join(".jellyfin-transcode").exists());
            assert!(marker.is_file());
            std::fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[cfg(unix)]
    #[test]
    fn transcode_directory_rejects_a_dangling_conflicting_marker() {
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("directory");
        let marker = root.join(".jellyfin-data");
        std::os::unix::fs::symlink(root.join("missing-target"), &marker).expect("dangling marker");
        assert!(!marker.is_file());
        assert!(prepare_transcode_directory(&root).is_err());
        assert!(!root.join(".jellyfin-transcode").exists());
        assert!(
            marker
                .symlink_metadata()
                .expect("symlink retained")
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn transcode_directory_retains_a_matching_dangling_marker_without_creating_its_target() {
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("directory");
        let marker = root.join(".jellyfin-transcode");
        let target = root.join("missing-target");
        std::os::unix::fs::symlink(&target, &marker).expect("dangling matching marker");
        assert!(transcode_marker_exists(&marker));
        prepare_transcode_directory(&root).expect("matching marker is already present");
        assert!(
            std::fs::symlink_metadata(&marker)
                .expect("retained link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&marker).expect("retained target"),
            target
        );
        assert!(
            !target.exists(),
            "the matching marker must not create its missing target"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn transcode_directory_retains_a_matching_fifo_without_opening_it() {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("directory");
        let marker = root.join(".jellyfin-transcode");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&marker)
                .status()
                .expect("create FIFO")
                .success()
        );
        let original = std::fs::symlink_metadata(&marker).expect("FIFO metadata");
        assert!(original.file_type().is_fifo());
        assert!(transcode_marker_exists(&marker));
        // No reader is attached. Correct preparation only stats this marker;
        // opening it for writing would block, which these source guards avoid.
        prepare_transcode_directory(&root).expect("matching FIFO is already present");
        let retained = std::fs::symlink_metadata(&marker).expect("retained FIFO");
        assert!(retained.file_type().is_fifo());
        assert_eq!(retained.dev(), original.dev());
        assert_eq!(retained.ino(), original.ino());
        assert_eq!(retained.len(), original.len());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transcode_directory_retains_a_matching_socket_without_opening_it() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        use std::os::unix::net::UnixListener;
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("directory");
        let marker = root.join(".jellyfin-transcode");
        let directory = std::fs::File::open(&root).expect("owned directory fd");
        // AF_UNIX has a short pathname limit. Resolve this short Linux fd path
        // into the same owned directory, without changing the process cwd.
        let bind_path = format!(
            "/proc/self/fd/{}/.jellyfin-transcode",
            directory.as_raw_fd()
        );
        let listener = UnixListener::bind(bind_path).expect("matching socket marker");
        let original = std::fs::symlink_metadata(&marker).expect("socket metadata");
        assert!(original.file_type().is_socket());
        assert!(transcode_marker_exists(&marker));
        prepare_transcode_directory(&root).expect("matching socket is already present");
        let retained = std::fs::symlink_metadata(&marker).expect("retained socket");
        assert!(retained.file_type().is_socket());
        assert_eq!(retained.dev(), original.dev());
        assert_eq!(retained.ino(), original.ino());
        assert_eq!(retained.len(), original.len());
        drop(listener);
        drop(directory);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn transcode_directory_does_not_follow_a_matching_directory_link_as_a_file() {
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        let target = root.join("directory-target");
        std::fs::create_dir_all(&target).expect("directory target");
        std::fs::write(target.join("keep"), b"retained contents").expect("directory contents");
        let marker = root.join(".jellyfin-transcode");
        std::os::unix::fs::symlink(&target, &marker).expect("directory link marker");
        assert!(!transcode_marker_exists(&marker));
        assert!(prepare_transcode_directory(&root).is_err());
        assert_eq!(
            std::fs::read(target.join("keep")).expect("retained contents"),
            b"retained contents"
        );
        assert_eq!(
            std::fs::read_link(&marker).expect("retained target"),
            target
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transcode_marker_search_keeps_the_native_case_sensitive_glob_prefix() {
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("directory");
        let ignored = root.join(".JELLYFIN-data");
        std::fs::write(&ignored, b"keep").expect("uppercase prefix");
        prepare_transcode_directory(&root)
            .expect("case-sensitive wildcard does not find this entry");
        assert_eq!(std::fs::read(&ignored).expect("retained"), b"keep");
        assert!(root.join(".jellyfin-transcode").is_file());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn transcode_directory_reports_a_file_as_root_and_a_directory_as_marker() {
        let root =
            std::env::temp_dir().join(format!("ferrofin-transcode-{}", uuid::Uuid::new_v4()));
        std::fs::write(&root, b"file").expect("root file");
        assert!(prepare_transcode_directory(&root).is_err());
        std::fs::remove_file(&root).expect("remove root file");
        std::fs::create_dir_all(root.join(".jellyfin-transcode")).expect("marker directory");
        assert!(prepare_transcode_directory(&root).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn children_follow_updates_but_operation_snapshots_stay_put() {
        let current = Arc::new(RwLock::new(PathBuf::from("first")));
        let source = Arc::clone(&current);
        let root = DirectoryPath::live(move || source.read().expect("path").clone());
        let child = root.child("images");
        let operation = child.join("poster.jpg");
        *current.write().expect("path") = PathBuf::from("second");
        assert_eq!(operation, PathBuf::from("first/images/poster.jpg"));
        assert_eq!(child.resolve(), PathBuf::from("second/images"));
        assert_eq!(format!("{root:?}"), "\"second\"");
        assert_eq!(
            DirectoryPath::from("fixed").resolve(),
            PathBuf::from("fixed")
        );
    }
}
