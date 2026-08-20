//! On-disk playlist files — port of
//! `MediaBrowser.Providers/Playlists/PlaylistItemsProvider` and the
//! `SavePlaylistFile` writer it pairs with.
//!
//! A playlist stored on disk (`.m3u`, `.m3u8`, `.pls`, `.wpl`, `.zpl`) lists
//! media paths, absolute or relative to the playlist's own directory. The
//! provider resolves each to a library item; the writer emits the same list back
//! so a playlist edited in Ferrofin stays readable by the player that wrote it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The playlist-file extensions Jellyfin reads.
pub const PLAYLIST_EXTENSIONS: [&str; 5] = ["m3u", "m3u8", "pls", "wpl", "zpl"];

/// Whether `path` is a playlist file this module can read.
#[must_use]
pub fn is_playlist_file(path: &str) -> bool {
    extension_of(path).is_some_and(|ext| PLAYLIST_EXTENSIONS.contains(&ext.as_str()))
}

/// The media paths a playlist file lists, in order, each resolved against the
/// playlist's own directory.
///
/// Port of `PlaylistItemsProvider.GetItems` plus its per-format readers. The
/// caller resolves each path to a library item (C# `FindByPath`), which is the
/// half that needs the item store.
#[must_use]
pub fn read_playlist_file(path: &str, contents: &str) -> Vec<PathBuf> {
    let entries = match extension_of(path).as_deref() {
        Some("m3u" | "m3u8") => parse_m3u(contents),
        Some("pls") => parse_pls(contents),
        Some("wpl" | "zpl") => parse_wpl(contents),
        _ => Vec::new(),
    };
    let dir = Path::new(path).parent().map(Path::to_path_buf);
    // Repeats are kept: `PlaylistItemsProvider` does not de-duplicate, and a
    // playlist that legitimately lists a track twice must keep both entries.
    entries
        .into_iter()
        .map(|entry| make_absolute(dir.as_deref(), &entry))
        .collect()
}

/// An `.m3u`/`.m3u8` file: one path per line, `#` lines are directives.
fn parse_m3u(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// A `.pls` file: `FileN=<path>` entries under `[playlist]`.
fn parse_pls(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            key.trim()
                .to_ascii_lowercase()
                .starts_with("file")
                .then(|| value.trim().to_owned())
        })
        .filter(|value| !value.is_empty())
        .collect()
}

/// A `.wpl`/`.zpl` file: Windows/Zune XML with `<media src="…"/>` entries.
fn parse_wpl(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = contents;
    while let Some(start) = rest.find("<media") {
        rest = &rest[start + "<media".len()..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end..];
        if let Some(src) = attribute_value(tag, "src")
            && !src.is_empty()
        {
            out.push(src);
        }
    }
    out
}

/// The value of `name="…"` (or `name='…'`) inside an element's attribute text.
fn attribute_value(tag: &str, name: &str) -> Option<String> {
    // Match the attribute name at a word boundary: a bare `find("src=")` also
    // matches the tail of `xsrc="…"`.
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let at = lower.match_indices(&needle).find_map(|(index, _)| {
        let preceded_by_space = index == 0
            || lower[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        preceded_by_space.then_some(index)
    })? + name.len()
        + 1;
    let rest = &tag[at..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(unescape(rest[..end].trim()))
}

/// Reverses the five XML entities.
fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Serializes `paths` as the playlist format `path`'s extension implies —
/// the write side C# calls `SavePlaylistFile`.
///
/// Paths are written verbatim (absolute), which every player accepts and which
/// keeps the file valid wherever it is moved inside the library.
#[must_use]
pub fn write_playlist_file(path: &str, paths: &[String]) -> String {
    match extension_of(path).as_deref() {
        Some("pls") => {
            let mut out = String::from("[playlist]\n");
            for (index, entry) in paths.iter().enumerate() {
                let _ = writeln!(out, "File{}={entry}", index + 1);
            }
            let _ = writeln!(out, "NumberOfEntries={}\nVersion=2", paths.len());
            out
        }
        Some("wpl" | "zpl") => {
            let mut out = String::from("<?wpl version=\"1.0\"?>\n<smil>\n  <body>\n    <seq>\n");
            for entry in paths {
                let _ = writeln!(out, "      <media src=\"{}\"/>", escape(entry));
            }
            out.push_str("    </seq>\n  </body>\n</smil>\n");
            out
        }
        // `.m3u`/`.m3u8` and anything else fall back to the plain list, which is
        // what upstream's writer emits.
        _ => {
            let mut out = String::from("#EXTM3U\n");
            for entry in paths {
                out.push_str(entry);
                out.push('\n');
            }
            out
        }
    }
}

/// Escapes the five XML entities.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Resolves `entry` against the playlist's directory — port of
/// `IFileSystem.MakeAbsolutePath`. An already-absolute entry is kept.
fn make_absolute(dir: Option<&Path>, entry: &str) -> PathBuf {
    let entry_path = Path::new(entry);
    match dir {
        Some(dir) if entry_path.is_relative() => normalize(&dir.join(entry_path)),
        _ => normalize(entry_path),
    }
}

/// Collapses `.` and `..` segments without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The lowercase extension (no dot) of a path.
fn extension_of(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_extensions_are_matched_case_insensitively() {
        assert!(is_playlist_file("/p/list.M3U"));
        assert!(is_playlist_file("/p/list.pls"));
        assert!(is_playlist_file("/p/list.zpl"));
        assert!(!is_playlist_file("/p/list.txt"));
        assert!(!is_playlist_file("/p/no-extension"));
    }

    #[test]
    fn m3u_entries_resolve_against_the_playlist_directory() {
        let paths = read_playlist_file(
            "/music/lists/road.m3u",
            "#EXTM3U\n#EXTINF:123,Artist - Song\n../a.flac\nsub/b.flac\n/abs/c.flac\n",
        );
        assert_eq!(
            paths,
            [
                PathBuf::from("/music/a.flac"),
                PathBuf::from("/music/lists/sub/b.flac"),
                PathBuf::from("/abs/c.flac"),
            ]
        );
    }

    #[test]
    fn a_repeated_entry_is_kept_as_upstream_keeps_it() {
        // `PlaylistItemsProvider` does not de-duplicate; a playlist that lists
        // the same track twice plays it twice in Jellyfin.
        let paths = read_playlist_file("/m/l.m3u", "a.flac\na.flac\n./a.flac\n");
        assert_eq!(
            paths,
            [
                PathBuf::from("/m/a.flac"),
                PathBuf::from("/m/a.flac"),
                PathBuf::from("/m/a.flac")
            ]
        );
    }

    #[test]
    fn pls_reads_the_file_keys_only() {
        let paths = read_playlist_file(
            "/m/l.pls",
            "[playlist]\nFile1=/m/a.flac\nTitle1=A\nFile2=/m/b.flac\nNumberOfEntries=2\n",
        );
        assert_eq!(
            paths,
            [PathBuf::from("/m/a.flac"), PathBuf::from("/m/b.flac")]
        );
    }

    #[test]
    fn wpl_reads_the_media_src_attributes() {
        let paths = read_playlist_file(
            "/m/l.wpl",
            r#"<?wpl version="1.0"?><smil><body><seq>
                 <media src="a &amp; b.flac"/>
                 <media src='/m/c.flac'/>
               </seq></body></smil>"#,
        );
        assert_eq!(
            paths,
            [PathBuf::from("/m/a & b.flac"), PathBuf::from("/m/c.flac")]
        );
    }

    #[test]
    fn an_unknown_extension_reads_nothing() {
        assert!(read_playlist_file("/m/l.txt", "a.flac\n").is_empty());
    }

    #[test]
    fn each_format_round_trips_through_its_writer() {
        let paths = vec!["/m/a.flac".to_owned(), "/m/b & c.flac".to_owned()];
        for name in ["l.m3u", "l.m3u8", "l.pls", "l.wpl", "l.zpl"] {
            let path = format!("/m/{name}");
            let written = write_playlist_file(&path, &paths);
            let read_back = read_playlist_file(&path, &written);
            assert_eq!(
                read_back,
                [PathBuf::from("/m/a.flac"), PathBuf::from("/m/b & c.flac")],
                "{name} round trip: {written}"
            );
        }
    }

    #[test]
    fn the_pls_writer_emits_the_entry_count() {
        let written = write_playlist_file("/m/l.pls", &["/m/a.flac".to_owned()]);
        assert!(written.contains("NumberOfEntries=1"));
        assert!(written.contains("File1=/m/a.flac"));
    }
}
