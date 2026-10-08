//! Free-function port of the C# `BaseItem`/`Folder`/`Video` resolver tree.
//!
//! `Emby.Server.Implementations.Library.LibraryManager` orchestrates over an OOP
//! object tree of resolvers (`ResolverHelper`, `CoreResolutionIgnoreRule`,
//! `IgnorePatterns`, `PathExtensions`) that Ferrofin does not port as classes.
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
//! the naming-level path parsing it would call already lives in `ferrofin-naming`
//! ([`ferrofin_naming::path`], [`ferrofin_naming::video`], …) and should be reused.

use async_trait::async_trait;

use ferrofin_traits::error::ServiceError;

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
    // Files directly inside a sample folder (`**/sample/*`, `**/minta/*`):
    // neither the folder itself nor anything deeper.
    IgnoreRule::ParentSegmentEquals("sample"),
    IgnoreRule::ParentSegmentEquals("minta"),
    IgnoreRule::PathSegmentEquals("metadata"),
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
    /// The segment holding the last one equals this, case-insensitively: a
    /// direct child of that directory (C# `**/<dir>/*`, whose `*` matches one
    /// segment, so neither `<dir>` itself nor a deeper path).
    ParentSegmentEquals(&'static str),
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
            IgnoreRule::ParentSegmentEquals(dir) => {
                segments.len() >= 2 && segments[segments.len() - 2] == dir
            }
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
    use super::should_ignore_path;

    /// Upstream `IgnorePatternsTests.PathIgnored`, every `[InlineData]`.
    #[rstest::rstest]
    #[case("/media/small.jpg", true)]
    #[case("/media/albumart.jpg", true)]
    #[case("/media/movie.sample.mp4", true)]
    #[case("/media/movie/sample.mp4", true)]
    #[case("/media/movie/sample/movie.mp4", true)]
    #[case("/foo/sample/bar/baz.mkv", false)]
    #[case("/media/movies/the sample/the sample.mkv", false)]
    #[case("/media/movies/sampler.mkv", false)]
    #[case("/media/movies/#Recycle/test.txt", true)]
    #[case("/media/movies/#recycle/", true)]
    #[case("/media/movies/#recycle", true)]
    #[case("thumbs.db", true)]
    #[case(r"C:\media\movies\movie.avi", false)]
    #[case("/media/.hiddendir/file.mp4", false)]
    #[case("/media/dir/.hiddenfile.mp4", true)]
    #[case("/media/dir/._macjunk.mp4", true)]
    #[case("/volume1/video/Series/@eaDir", true)]
    #[case("/volume1/video/Series/@eaDir/file.txt", true)]
    #[case("/directory/@Recycle", true)]
    #[case("/directory/@Recycle/file.mp3", true)]
    #[case("/media/movies/.@__thumb", true)]
    #[case("/media/movies/.@__thumb/foo-bar-thumbnail.png", true)]
    #[case("/media/music/Foo B.A.R./epic.flac", false)]
    #[case("/media/music/Foo B.A.R", false)]
    #[case("/media/music/Foo B.A.R.", false)]
    #[case("/movies/.zfs/snapshot/AutoM-2023-09", true)]
    fn path_ignored(#[case] path: &str, #[case] expected: bool) {
        assert_eq!(should_ignore_path(path), expected, "{path}");
    }

    /// `**/sample/*` takes a sample folder's direct children only: the folder
    /// itself stays visible, so the scanner can see it as the `sample` extras
    /// folder (PR #17964) — whatever its case.
    #[rstest::rstest]
    #[case("/media/Movie (2020)/Sample", false)]
    #[case("/media/Movie (2020)/SAMPLE/clip.mkv", true)]
    #[case("/media/Movie (2020)/minta/clip.mkv", true)]
    #[case("/media/Movie (2020)/minta", false)]
    fn a_sample_folder_hides_its_files_not_itself(#[case] path: &str, #[case] expected: bool) {
        assert_eq!(should_ignore_path(path), expected, "{path}");
    }

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
}
