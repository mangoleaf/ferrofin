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

    // `Path.GetFileName` is what follows the last separator. `Path::file_name`
    // would normalise `sub/` and `sub/.` to `sub`, which upstream rejects.
    let leaf = file_name
        .rsplit(std::path::is_separator)
        .next()
        .unwrap_or_default();
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        return None;
    }

    Some(leaf.to_owned())
}

/// Lexically resolves a path: collapses `.` and `..` segments without consulting
/// the filesystem (unlike `std::fs::canonicalize`, which requires existence).
#[must_use]
pub fn get_full_path(path: &str) -> PathBuf {
    let input = Path::new(path);
    let mut resolved = PathBuf::new();
    for component in input.components() {
        match component {
            Component::CurDir => {}
            // `..` cancels a name, stays put at the root, and piles up at
            // the start of a relative path.
            Component::ParentDir => match resolved.components().next_back() {
                Some(Component::Normal(_)) => {
                    resolved.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => resolved.push(".."),
            },
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// Returns whether `candidate` resolves to a path that equals or is contained
/// inside `root`.
///
/// Both arguments are anchored at the working directory when relative and
/// lexically resolved, as `Path.GetFullPath` does, so `..` segments are
/// collapsed before the comparison. The root is compared with a trailing separator to prevent
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

    let (Ok(root), Ok(candidate)) = (std::path::absolute(root), std::path::absolute(candidate))
    else {
        return false;
    };
    let full_root = get_full_path(&root.to_string_lossy());
    let full_candidate = get_full_path(&candidate.to_string_lossy());

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

/// The invalid path characters `ManagedFileSystem.GetValidFilename` replaces:
/// `ManagedFileSystem._invalidPathCharacters`, `"` `<` `>` `|` the control
/// characters 0–31, `:` `*` `?` `\` and `/`.
fn is_invalid_path_character(c: char) -> bool {
    matches!(c, '"' | '<' | '>' | '|' | ':' | '*' | '?' | '\\' | '/') || u32::from(c) <= 31
}

/// Port of `ManagedFileSystem.GetValidFilename`: every invalid path character
/// replaced by a space, nothing trimmed. Callers trim as their upstream caller
/// does (`AddVirtualFolder` trims first, the DVR paths after).
#[must_use]
pub fn get_valid_filename(filename: &str) -> String {
    filename
        .chars()
        .map(|c| if is_invalid_path_character(c) { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    /// `ManagedFileSystemTests.GetValidFilename_ReturnsValidFilename`, plus the
    /// control characters upstream's array lists.
    #[rstest::rstest]
    #[case("ValidFileName", "ValidFileName")]
    #[case("AC/DC", "AC DC")]
    #[case("Invalid\0", "Invalid ")]
    #[case("AC/DC\0KD/A", "AC DC KD A")]
    #[case("a\tb\nc\u{1f}d", "a b c d")]
    #[case("\"<>|:*?\\/", "         ")]
    fn get_valid_filename_replaces_invalid_characters(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(super::get_valid_filename(name), expected);
    }

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
    // `Path.GetFileName` takes what follows the last separator, so a trailing
    // separator or `.` leaves no usable leaf.
    #[case(Some("sub/"))]
    #[case(Some("sub/."))]
    #[case(Some("sub/.."))]
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

    #[rstest]
    #[case("/a/../../m/x", "/m/x")]
    #[case("/a/./b/../c", "/a/c")]
    #[case("../a/../b", "../b")]
    #[case("../../x", "../../x")]
    #[case("a/../../../b", "../../b")]
    fn get_full_path_resolves_lexically(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(get_full_path(input), PathBuf::from(expected));
    }

    #[test]
    fn is_contained_in_anchors_relative_paths() {
        assert!(!is_contained_in("..", "../../x"));
        assert!(is_contained_in("a", "a/b/../c"));
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
