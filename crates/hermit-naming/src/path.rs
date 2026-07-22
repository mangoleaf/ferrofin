//! Minimal port of the `System.IO.Path` helpers the naming parsers rely on.
//!
//! The Emby.Naming code calls `Path.GetFileName`, `Path.GetExtension`,
//! `Path.GetFileNameWithoutExtension` and `Path.GetDirectoryName` on raw path
//! strings that use *either* `/` or `\` as a separator (Windows paths appear in
//! the test corpus, e.g. `D:\media\Foo\Foo-S01E01`). .NET treats both as
//! directory separators regardless of host OS, so we reproduce that behaviour
//! here rather than using `std::path`, which is host-separator-specific and
//! would diverge from the C# oracle.

/// Both directory separators .NET recognises on any platform.
const SEPARATORS: [char; 2] = ['/', '\\'];

/// Returns the file name and extension of the path (everything after the last
/// separator). Mirrors `Path.GetFileName`.
#[must_use]
pub fn file_name(path: &str) -> &str {
    match path.rfind(SEPARATORS) {
        Some(idx) => &path[idx + 1..],
        None => path,
    }
}

/// Returns the extension of the path, *including* the leading `.`, or an empty
/// string when there is none. Mirrors `Path.GetExtension`.
#[must_use]
pub fn extension(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        // A leading dot with nothing before it (".disc") still counts as an
        // extension in .NET only if there is content before it; ".gitignore"
        // style names have the dot at index 0 → .NET returns the whole thing.
        Some(idx) => &name[idx..],
        None => "",
    }
}

/// Returns the file name without its extension. Mirrors
/// `Path.GetFileNameWithoutExtension`.
#[must_use]
pub fn file_name_without_extension(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(idx) => &name[..idx],
        None => name,
    }
}

/// Returns the directory portion of the path (everything before the last
/// separator), or `None` when the path has no directory. Mirrors
/// `Path.GetDirectoryName`, which returns `null` for a bare file name.
#[must_use]
pub fn directory_name(path: &str) -> Option<&str> {
    path.rfind(SEPARATORS).map(|idx| &path[..idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_handles_both_separators() {
        assert_eq!(file_name("/a/b/c.mkv"), "c.mkv");
        assert_eq!(file_name("a\\b\\c.mkv"), "c.mkv");
        assert_eq!(file_name("c.mkv"), "c.mkv");
    }

    #[test]
    fn extension_and_stem() {
        assert_eq!(extension("/a/b/c.mkv"), ".mkv");
        assert_eq!(extension("/a/b/c"), "");
        assert_eq!(file_name_without_extension("/a/b/c.mkv"), "c");
        assert_eq!(file_name_without_extension("/a/b/c"), "c");
    }

    #[test]
    fn directory_name_is_optional() {
        assert_eq!(directory_name("/a/b/c.mkv"), Some("/a/b"));
        assert_eq!(directory_name("c.mkv"), None);
    }
}
