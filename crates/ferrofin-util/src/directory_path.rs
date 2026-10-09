//! Directory roots that follow a live configuration without moving in-flight work.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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
