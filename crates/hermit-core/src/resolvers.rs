//! Free-function port of the C# `BaseItem`/`Folder`/`Video` resolver tree.
//!
//! `Emby.Server.Implementations.Library.LibraryManager` orchestrates over an OOP
//! object tree of resolvers (`ResolverHelper`, `CoreResolutionIgnoreRule`,
//! `IgnorePatterns`, `PathExtensions`) that Hermit does not port as classes.
//! Their pure, kind-keyed logic lives here as free functions the library/monitor
//! layers call — mirroring how [`crate::kinds`] holds the `Supports*` table.
//!
//! Landed here:
//! - [`should_ignore_path`] — the [`IgnorePatterns.ShouldIgnore`] glob table
//!   (sample/artwork files, metadata/trash/temp directories, hidden files);
//! - [`sort_name`] — the `BaseItem.SortName` normalization (article stripping +
//!   lower-casing) used to order by-name and library rows;
//! - [`FileSystemWatcher`] — a small trait the [`crate::library_monitor`] uses so
//!   tests can watch a fake instead of a real filesystem.
//!
//! The full metadata-driven resolution pipeline (matching a path to a concrete
//! item kind) depends on the un-ported scanner and is out of scope for this seam;
//! the naming-level path parsing it would call already lives in `hermit-naming`
//! ([`hermit_naming::path`], [`hermit_naming::video`], …) and should be reused.

use async_trait::async_trait;

use hermit_traits::error::ServiceError;

/// Leading articles stripped when computing a [`sort_name`] (C#
/// `BaseItem.SortName` uses the configurable `SortRemoveWords`; these are the
/// English defaults shipped in `ServerConfiguration.SortRemoveWords`).
const SORT_REMOVE_WORDS: &[&str] = &["the", "a", "an"];

/// File-name globs whose match means "ignore this path" (C#
/// `IgnorePatterns._patterns`). Each entry is a `(needle, kind)` rule evaluated
/// against the lower-cased path; see [`IgnoreRule`].
///
/// The C# list uses DotNet.Glob `**/…` patterns. Every pattern is one of a small
/// number of shapes, so rather than pull in a glob engine we classify each into
/// an [`IgnoreRule`] and match structurally (case-insensitively), which is both
/// faithful and dependency-free.
const IGNORE_RULES: &[IgnoreRule] = &[
    // Artwork the scanner should not treat as a media item.
    IgnoreRule::FileNameEquals("small.jpg"),
    IgnoreRule::FileNameEquals("albumart.jpg"),
    IgnoreRule::FileNameEquals("thumbs.db"),
    // "sample.<ext>" and "*.sample.<ext>" clips (any short extension).
    IgnoreRule::SampleClip("sample"),
    IgnoreRule::SampleClip("minta"),
    // bts sync artifacts.
    IgnoreRule::ExtensionEquals("bts"),
    IgnoreRule::ExtensionEquals("sync"),
    // Trickplay generated data.
    IgnoreRule::ExtensionEquals("trickplay"),
    IgnoreRule::PathSegmentSuffix(".trickplay"),
    // Directories anywhere in the path.
    IgnoreRule::PathSegmentEquals("metadata"),
    IgnoreRule::PathSegmentEquals("sample"),
    IgnoreRule::PathSegmentEquals("minta"),
    IgnoreRule::PathSegmentEquals("ps3_update"),
    IgnoreRule::PathSegmentEquals("ps3_vprm"),
    IgnoreRule::PathSegmentEquals("extrafanart"),
    IgnoreRule::PathSegmentEquals("extrathumbs"),
    IgnoreRule::PathSegmentEquals(".actors"),
    IgnoreRule::PathSegmentEquals(".wd_tv"),
    IgnoreRule::PathSegmentEquals("lost+found"),
    IgnoreRule::PathSegmentEquals("subs"),
    IgnoreRule::PathSegmentEquals(".snapshots"),
    IgnoreRule::PathSegmentEquals(".snapshot"),
    IgnoreRule::PathSegmentEquals("temprec"),
    IgnoreRule::PathSegmentEquals("tempsbe"),
    IgnoreRule::PathSegmentEquals("eadir"),
    IgnoreRule::PathSegmentEquals("@eadir"),
    IgnoreRule::PathSegmentEquals("#recycle"),
    IgnoreRule::PathSegmentEquals("@recycle"),
    IgnoreRule::PathSegmentEquals(".@__thumb"),
    IgnoreRule::PathSegmentEquals("$recycle.bin"),
    IgnoreRule::PathSegmentEquals("system volume information"),
    IgnoreRule::PathSegmentEquals(".grab"),
    IgnoreRule::PathSegmentEquals(".zfs"),
];

/// One classified ignore rule (the shape of a C# `IgnorePatterns` glob).
#[derive(Debug, Clone, Copy)]
enum IgnoreRule {
    /// The file name (last path segment) equals this, case-insensitively.
    FileNameEquals(&'static str),
    /// The file extension equals this, case-insensitively (e.g. `bts`).
    ExtensionEquals(&'static str),
    /// A `<stem>.<ext>` or `*.<stem>.<ext>` sample-clip pattern with a short
    /// extension (C# `**/sample.?`…`**/*.sample.?????`).
    SampleClip(&'static str),
    /// Some path segment equals this, case-insensitively (a directory match).
    PathSegmentEquals(&'static str),
    /// Some path segment ends with this suffix, case-insensitively.
    PathSegmentSuffix(&'static str),
}

impl IgnoreRule {
    /// Whether this rule matches the already-lower-cased `path` (with its
    /// segments split on `/` and `\`).
    fn matches(self, lower_path: &str, segments: &[&str], file_name: &str) -> bool {
        match self {
            IgnoreRule::FileNameEquals(name) => file_name == name,
            IgnoreRule::ExtensionEquals(ext) => {
                file_name.rsplit_once('.').is_some_and(|(_, e)| e == ext)
            }
            IgnoreRule::SampleClip(stem) => is_sample_clip(file_name, stem),
            IgnoreRule::PathSegmentEquals(seg) => segments.contains(&seg),
            IgnoreRule::PathSegmentSuffix(suffix) => {
                segments.iter().any(|s| s.ends_with(suffix)) || lower_path.ends_with(suffix)
            }
        }
    }
}

/// Whether `file_name` is a `<stem>.<ext>` or `*.<stem>.<ext>` sample clip, where
/// `<ext>` is 1–5 characters (the C# `**/sample.?`…`**/*.sample.?????` shapes).
fn is_sample_clip(file_name: &str, stem: &str) -> bool {
    let Some((base, ext)) = file_name.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || ext.len() > 5 {
        return false;
    }
    base == stem || base.ends_with(&format!(".{stem}"))
}

/// Returns whether the scanner should ignore `path` (C#
/// `IgnorePatterns.ShouldIgnore`), plus the "unix hidden file" rule (`**/.*`):
/// any leading-dot file name is ignored.
///
/// The path is matched case-insensitively against the [`IGNORE_RULES`] table.
/// Application-folder and top-level-folder exemptions from
/// `CoreResolutionIgnoreRule` are the caller's concern (they need the item tree);
/// this covers the pattern table those exemptions gate.
#[must_use]
pub fn should_ignore_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let segments: Vec<&str> = lower.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let file_name = segments.last().copied().unwrap_or("");

    // Unix hidden files (**/.*), but never the "current"/"parent" markers.
    if file_name.starts_with('.') && file_name != "." && file_name != ".." {
        // A hidden *directory* in the middle is caught by its own PathSegment rule;
        // a hidden file name is ignored outright.
        if !file_name.contains('/') {
            return true;
        }
    }

    IGNORE_RULES
        .iter()
        .any(|rule| rule.matches(&lower, &segments, file_name))
}

/// Computes the sort key for a display `name` (C# `BaseItem.SortName`): strip a
/// single leading article (`the`/`a`/`an`) and lower-case the remainder.
///
/// A name that is *only* an article is returned lower-cased unchanged (dropping
/// it would yield an empty, unsortable key).
#[must_use]
pub fn sort_name(name: &str) -> String {
    let trimmed = name.trim();
    let lower = trimmed.to_lowercase();
    for word in SORT_REMOVE_WORDS {
        if let Some(rest) = lower.strip_prefix(word) {
            // Only strip when the article is a whole leading word (followed by
            // whitespace), so "theatre" is not mangled into "atre".
            if let Some(remainder) = rest.strip_prefix(' ') {
                let remainder = remainder.trim_start();
                if !remainder.is_empty() {
                    return remainder.to_owned();
                }
            }
        }
    }
    lower
}

/// A minimal filesystem-watch seam so the [`crate::library_monitor`] can be
/// tested against a fake.
///
/// Port of the slice of `IFileSystemWatcher` the library monitor relies on: it
/// only needs to start/stop watching a set of roots and report failures. The
/// real implementation (an inotify/`FileSystemWatcher` wrapper) is injected at
/// the composition root; unit tests supply an in-memory fake.
#[async_trait]
pub trait FileSystemWatcher: Send + Sync {
    /// Begins watching `path` for changes.
    async fn watch(&self, path: &str) -> Result<(), ServiceError>;

    /// Stops watching `path`.
    async fn unwatch(&self, path: &str) -> Result<(), ServiceError>;

    /// Stops watching everything.
    async fn unwatch_all(&self) -> Result<(), ServiceError>;
}

fn _assert_object_safe_file_system_watcher(_: &dyn FileSystemWatcher) {}

#[cfg(test)]
mod tests {
    use super::{should_ignore_path, sort_name};

    #[test]
    fn ignores_artwork_and_sample_files() {
        assert!(should_ignore_path("/media/Movie/small.jpg"));
        assert!(should_ignore_path("/media/Movie/AlbumArt.jpg"));
        assert!(should_ignore_path("/media/Movie/sample.mkv"));
        assert!(should_ignore_path("/media/Movie/movie.sample.webm"));
        assert!(should_ignore_path("/media/Movie/minta.mkv"));
        // A real movie file is not ignored.
        assert!(!should_ignore_path("/media/Movie/movie.mkv"));
    }

    #[test]
    fn ignores_trash_and_metadata_directories() {
        assert!(should_ignore_path("/media/metadata/poster.jpg"));
        assert!(should_ignore_path("/media/Show/extrafanart/1.jpg"));
        assert!(should_ignore_path("/media/@eaDir/thumb.jpg"));
        assert!(should_ignore_path("/media/$RECYCLE.BIN/x"));
        assert!(should_ignore_path("/media/System Volume Information/x"));
    }

    #[test]
    fn ignores_hidden_files_and_trickplay() {
        assert!(should_ignore_path("/media/Movie/.DS_Store"));
        assert!(should_ignore_path("/media/Movie/movie.trickplay"));
        assert!(!should_ignore_path("/media/Movie/movie.mkv"));
    }

    #[test]
    fn sort_name_strips_leading_articles() {
        assert_eq!(sort_name("The Matrix"), "matrix");
        assert_eq!(sort_name("A Beautiful Mind"), "beautiful mind");
        assert_eq!(sort_name("An Education"), "education");
        // Not a leading article word — left intact (lower-cased).
        assert_eq!(sort_name("Theatre of Blood"), "theatre of blood");
        // Article-only name is kept.
        assert_eq!(sort_name("The"), "the");
    }
}
