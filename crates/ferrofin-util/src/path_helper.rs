//! Port of `PathHelper.cs` — helpers for safely composing filesystem paths from
//! untrusted input.
//!
//! `IsContainedIn` resolves `..` segments *lexically* (without touching the
//! filesystem) so that temp-path tests pass regardless of whether the paths
//! exist, mirroring the behavioral contract of `Path.GetFullPath`.

use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};

/// Reduces a possibly-untrusted file name to a safe leaf-only name with no
/// directory components.
///
/// Returns the leaf component of `file_name`, or `None` if the input has no
/// usable leaf (empty, `.`, or `..`).
#[must_use]
pub fn get_safe_leaf_file_name(file_name: Option<&str>) -> Option<String> {
    let file_name = file_name?;
    if file_name.is_empty() {
        return None;
    }

    let leaf = Path::new(file_name)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        return None;
    }

    Some(leaf)
}

/// Lexically resolves a path: collapses `.` and `..` segments without consulting
/// the filesystem (unlike `std::fs::canonicalize`, which requires existence).
fn get_full_path(path: &str) -> PathBuf {
    let input = Path::new(path);
    let mut resolved = PathBuf::new();
    for component in input.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    // Preserve leading `..` on relative paths.
                    resolved.push("..");
                }
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// Returns whether `candidate` resolves to a path that equals or is contained
/// inside `root`.
///
/// Both arguments are lexically resolved so `..` segments are collapsed before
/// the comparison. The root is compared with a trailing separator to prevent
/// prefix collisions (e.g. `/var/data` must not be accepted as a parent of
/// `/var/dataset`).
///
/// # Panics
///
/// Panics if `root` or `candidate` is empty, mirroring the C#
/// `ArgumentException.ThrowIfNullOrEmpty` guards.
#[must_use]
pub fn is_contained_in(root: &str, candidate: &str) -> bool {
    assert!(!root.is_empty(), "root must not be null or empty");
    assert!(!candidate.is_empty(), "candidate must not be null or empty");

    let full_root = get_full_path(root);
    let full_candidate = get_full_path(candidate);

    if full_candidate == full_root {
        return true;
    }

    let mut root_with_sep = full_root.into_os_string().into_string().unwrap_or_default();
    if !root_with_sep.ends_with(MAIN_SEPARATOR) {
        root_with_sep.push(MAIN_SEPARATOR);
    }

    let candidate_str = full_candidate.to_string_lossy();
    candidate_str.starts_with(&root_with_sep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::env::temp_dir;
    use std::path::PathBuf;

    #[rstest]
    #[case("file.txt", "file.txt")]
    #[case("sub/file.txt", "file.txt")]
    #[case("../../etc/passwd", "passwd")]
    fn get_safe_leaf_file_name_reduces_to_leaf(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(
            Some(expected.to_owned()),
            get_safe_leaf_file_name(Some(input))
        );
    }

    #[rstest]
    #[case(None)]
    #[case(Some(""))]
    #[case(Some("."))]
    #[case(Some(".."))]
    fn get_safe_leaf_file_name_rejects_unusable_leaf(#[case] input: Option<&str>) {
        assert_eq!(None, get_safe_leaf_file_name(input));
    }

    fn join(parts: &[&str]) -> String {
        let mut p = PathBuf::new();
        for part in parts {
            p.push(part);
        }
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn is_contained_in_child_path_returns_true() {
        let root = join(&[temp_dir().to_string_lossy().as_ref(), "root"]);
        let child = join(&[&root, "sub", "file.txt"]);
        assert!(is_contained_in(&root, &child));
    }

    #[test]
    fn is_contained_in_root_itself_returns_true() {
        let root = join(&[temp_dir().to_string_lossy().as_ref(), "root"]);
        assert!(is_contained_in(&root, &root));
    }

    #[test]
    fn is_contained_in_traversal_escape_returns_false() {
        let root = join(&[temp_dir().to_string_lossy().as_ref(), "root"]);
        let escape = join(&[&root, "..", "..", "etc", "passwd"]);
        assert!(!is_contained_in(&root, &escape));
    }

    #[test]
    fn is_contained_in_sibling_prefix_collision_returns_false() {
        // "/var/data" must not be accepted as a parent of "/var/dataset".
        let root = join(&[temp_dir().to_string_lossy().as_ref(), "data"]);
        let sibling = join(&[temp_dir().to_string_lossy().as_ref(), "dataset", "file.txt"]);
        assert!(!is_contained_in(&root, &sibling));
    }
}
