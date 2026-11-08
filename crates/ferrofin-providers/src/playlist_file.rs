//! On-disk playlist files — port of
//! `MediaBrowser.Providers/Playlists/PlaylistItemsProvider` and the
//! `SavePlaylistFile` writer it pairs with.
//!
//! A playlist stored on disk (`.m3u`, `.m3u8`, `.pls`, `.wpl`, `.zpl`) lists
//! media paths, absolute or relative to the playlist's own directory. The
//! provider resolves each to a library item; the writer emits the same list back
//! so a playlist edited in Ferrofin stays readable by the player that wrote it.
//!
//! The library scan resolves a playlist file in a music or untyped library as
//! a `Playlist` row (`PlaylistResolver`) and fills its members from
//! [`read_playlist_file`]. The writer has no caller: upstream runs
//! `SavePlaylistFile` only from `PlaylistManager.UpdatePlaylistInternal`, and
//! the playlists controller lets only the owner or an editing share update a
//! playlist, which a scanned one — owned by nobody, shared with nobody — has
//! neither of.

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

/// Decodes a playlist file's bytes — port of `PlaylistItemsProvider.
/// DetectEncoding`, which reads the file in the encoding `UtfUnknown`
/// detects and in UTF-8 when it detects none.
///
/// A BOM decides first; then bytes that are valid UTF-8 are UTF-8, and
/// anything else is decoded in the legacy encoding [`chardetng`] guesses
/// (an old Winamp `.m3u` is usually Windows-1252).
#[must_use]
pub fn decode_playlist(bytes: &[u8]) -> String {
    if let Some((encoding, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding.decode_with_bom_removal(bytes).0.into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    detector.guess(None, true).decode(bytes).0.into_owned()
}

/// Where a playlist's `playlist.xml` metadata lives — port of
/// `PlaylistXmlSaver.GetSavePath`: beside a playlist FILE with its extension
/// changed to `.xml` (`Road Trip.m3u` → `Road Trip.xml`), inside a playlist
/// FOLDER as `playlist.xml`. `PlaylistXmlProvider` reads it before the
/// playlist file itself.
#[must_use]
pub fn playlist_xml_path(path: &str, is_file: bool) -> PathBuf {
    if is_file {
        Path::new(path).with_extension("xml")
    } else {
        Path::new(path).join("playlist.xml")
    }
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

/// One playlist entry: the media path plus the tags `PlaylistManager` fills in
/// before handing the list to PlaylistsNET.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// The item's absolute path on disk.
    pub path: String,
    /// `TrackTitle` — the item's name.
    pub title: Option<String>,
    /// `AlbumTitle` — the item's album, for the audio kinds that have one.
    pub album: Option<String>,
    /// `AlbumArtist` — the first album artist, when the item has any.
    pub album_artist: Option<String>,
    /// `TrackArtist` — the first artist, when the item has any.
    pub artist: Option<String>,
    /// `Duration` in whole seconds, from `RunTimeTicks`.
    pub duration_seconds: Option<i64>,
}

impl PlaylistEntry {
    /// A bare entry carrying only its path — what a caller with no item
    /// metadata to hand can still write.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// Serializes `entries` as the playlist format `path`'s extension implies —
/// the write side C# calls `SavePlaylistFile`.
///
/// Paths are written **relative to the playlist's own directory**, as
/// `PlaylistManager.NormalizeItemPath` does via `Uri.MakeRelativeUri`: an item
/// outside that directory gets `../` segments rather than staying absolute,
/// because two local paths always share the `file` scheme and the absolute
/// fallback there only fires when the schemes differ.
///
/// Every format is written in its EXTENDED form, as `PlaylistManager` does by
/// setting `IsExtended` and filling each entry's title/album/artist/duration:
/// rewriting a playlist must not strip the tags the player wrote into it.
#[must_use]
pub fn write_playlist_file(path: &str, entries: &[PlaylistEntry]) -> String {
    let dir = Path::new(path).parent().map(Path::to_path_buf);
    let rel: Vec<String> = entries
        .iter()
        .map(|entry| relative_to(dir.as_deref(), &entry.path))
        .collect();
    let rows: Vec<(&PlaylistEntry, &String)> = entries.iter().zip(rel.iter()).collect();
    match extension_of(path).as_deref() {
        Some("pls") => {
            let mut out = String::from("[playlist]\n");
            for (index, (entry, rel)) in rows.iter().enumerate() {
                let n = index + 1;
                let _ = writeln!(out, "File{n}={rel}");
                let title = display_title(entry);
                if !title.is_empty() {
                    let _ = writeln!(out, "Title{n}={title}");
                }
                if let Some(seconds) = entry.duration_seconds {
                    let _ = writeln!(out, "Length{n}={seconds}");
                }
            }
            let _ = writeln!(out, "NumberOfEntries={}\nVersion=2", rows.len());
            out
        }
        Some(kind @ ("wpl" | "zpl")) => {
            // `ZplContent` declares `zpl version="2.0"`; only `.wpl` is `wpl 1.0`.
            let header = if kind == "zpl" {
                "<?zpl version=\"2.0\"?>"
            } else {
                "<?wpl version=\"1.0\"?>"
            };
            let mut out = format!("{header}\n<smil>\n  <body>\n    <seq>\n");
            for (entry, rel) in &rows {
                let _ = write!(out, "      <media src=\"{}\"", escape(rel));
                let duration = entry
                    .duration_seconds
                    .map(|seconds| seconds.saturating_mul(1000).to_string());
                for (attribute, value) in [
                    ("albumTitle", entry.album.as_deref()),
                    ("albumArtist", entry.album_artist.as_deref()),
                    ("trackArtist", entry.artist.as_deref()),
                    ("trackTitle", entry.title.as_deref()),
                    // `Wpl`/`ZplContent` write the duration in milliseconds.
                    ("duration", duration.as_deref()),
                ] {
                    if let Some(value) = value.filter(|v| !v.is_empty()) {
                        let _ = write!(out, " {attribute}=\"{}\"", escape(value));
                    }
                }
                out.push_str("/>\n");
            }
            out.push_str("    </seq>\n  </body>\n</smil>\n");
            out
        }
        // `.m3u`/`.m3u8` and anything else fall back to the M3U writer.
        _ => {
            let mut out = String::from("#EXTM3U\n");
            for (entry, rel) in &rows {
                if let Some(album) = entry.album.as_deref().filter(|a| !a.is_empty()) {
                    let _ = writeln!(out, "#EXTALB:{album}");
                }
                if let Some(artist) = entry.album_artist.as_deref().filter(|a| !a.is_empty()) {
                    let _ = writeln!(out, "#EXTART:{artist}");
                }
                // `#EXTINF:<seconds>,<title>`. PlaylistsNET writes the line for
                // every entry of an extended playlist, with
                // `(int)TimeSpan.Zero.TotalSeconds` — i.e. 0 — when the item
                // reported no runtime.
                let _ = writeln!(
                    out,
                    "#EXTINF:{},{}",
                    entry.duration_seconds.unwrap_or(0),
                    display_title(entry)
                );
                out.push_str(rel);
                out.push('\n');
            }
            out
        }
    }
}

/// The label an extended playlist gives an entry.
///
/// `PlaylistManager` sets `Title = child.Name` and PlaylistsNET writes it
/// verbatim — deriving an `Artist - Title` here would mislabel every line
/// against what Jellyfin wrote.
fn display_title(entry: &PlaylistEntry) -> &str {
    entry.title.as_deref().unwrap_or_default()
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

/// `entry` expressed relative to `dir` — the write-side twin of
/// [`make_absolute`], and a port of `PlaylistManager.MakeRelativePath`.
///
/// `Uri.MakeRelativeUri` walks up out of the folder with `../` segments when
/// the target lies outside it; it returns the absolute path only when the two
/// URIs have different SCHEMES, which two local files never do. A relative
/// entry is also only meaningful when both paths are absolute — a relative
/// `entry` is already relative to the same directory and is left alone.
fn relative_to(dir: Option<&Path>, entry: &str) -> String {
    let path = Path::new(entry);
    let (Some(dir), true) = (
        dir,
        path.is_absolute() && dir.is_some_and(Path::is_absolute),
    ) else {
        return entry.to_owned();
    };
    let mut from = dir.components().peekable();
    let mut to = path.components().peekable();
    while from.peek().is_some() && from.peek() == to.peek() {
        from.next();
        to.next();
    }
    let mut out = PathBuf::new();
    for _ in from {
        out.push("..");
    }
    out.extend(to);
    if out.as_os_str().is_empty() {
        return entry.to_owned();
    }
    out.to_string_lossy().into_owned()
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
    fn playlist_bytes_decode_by_bom_then_utf8_then_a_legacy_guess() {
        assert_eq!(decode_playlist("Café.mp3\n".as_bytes()), "Café.mp3\n");
        assert_eq!(decode_playlist(b"\xEF\xBB\xBFa.mp3"), "a.mp3");
        assert_eq!(
            decode_playlist(b"\xFF\xFEa\x00.\x00m\x00p\x003\x00"),
            "a.mp3"
        );
        // Windows-1252, as an old Winamp playlist writes it.
        assert_eq!(
            decode_playlist(b"Beyonc\xE9 - D\xE9j\xE0 Vu.mp3\n"),
            "Beyoncé - Déjà Vu.mp3\n"
        );
    }

    #[test]
    fn the_xml_sits_beside_a_file_and_inside_a_folder() {
        assert_eq!(
            playlist_xml_path("/m/Road Trip.m3u", true),
            PathBuf::from("/m/Road Trip.xml")
        );
        assert_eq!(
            playlist_xml_path("/m/Road Trip [playlist]", false),
            PathBuf::from("/m/Road Trip [playlist]/playlist.xml")
        );
    }

    #[test]
    fn an_unknown_extension_reads_nothing() {
        let found = read_playlist_file("/m/l.txt", "a.flac\n");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn each_format_round_trips_through_its_writer() {
        // Including one entry OUTSIDE the playlist's directory, which
        // `MakeRelativeUri` writes with `../` segments — the reader has to
        // resolve them back.
        let entries = vec![
            PlaylistEntry::new("/m/a.flac"),
            PlaylistEntry::new("/m/b & c.flac"),
            PlaylistEntry::new("/other/d.flac"),
        ];
        for name in ["l.m3u", "l.m3u8", "l.pls", "l.wpl", "l.zpl"] {
            let path = format!("/m/{name}");
            let written = write_playlist_file(&path, &entries);
            let read_back = read_playlist_file(&path, &written);
            assert_eq!(
                read_back,
                [
                    PathBuf::from("/m/a.flac"),
                    PathBuf::from("/m/b & c.flac"),
                    PathBuf::from("/other/d.flac")
                ],
                "{name} round trip: {written}"
            );
        }
    }

    #[test]
    fn an_entry_outside_the_playlist_directory_walks_up() {
        // C# `MakeRelativePath` returns the absolute path only when the two
        // URIs' SCHEMES differ, which two local files never do — so a sibling
        // directory is reached with `../`, not left absolute.
        let written = write_playlist_file("/m/l.pls", &[PlaylistEntry::new("/other/b.flac")]);
        assert!(written.contains("File1=../other/b.flac"), "{written}");
        // An entry under the playlist's own directory is a bare relative path.
        let written = write_playlist_file("/m/l.pls", &[PlaylistEntry::new("/m/sub/a.flac")]);
        assert!(written.contains("File1=sub/a.flac"), "{written}");
    }

    #[test]
    fn the_pls_writer_emits_the_entry_count_and_tags() {
        let written = write_playlist_file(
            "/m/l.pls",
            &[PlaylistEntry {
                path: "/m/a.flac".to_owned(),
                title: Some("Airbag".to_owned()),
                artist: Some("Radiohead".to_owned()),
                duration_seconds: Some(284),
                ..PlaylistEntry::default()
            }],
        );
        assert!(written.contains("NumberOfEntries=1"));
        assert!(written.contains("File1=a.flac"), "{written}");
        // `Title = child.Name`, verbatim — no composed "Artist - Title".
        assert!(written.contains("Title1=Airbag"), "{written}");
        assert!(written.contains("Length1=284"), "{written}");
    }

    #[test]
    fn the_m3u_writer_emits_the_extended_directives() {
        // C# sets `IsExtended` and fills the per-entry tags, so PlaylistsNET
        // writes #EXTALB/#EXTART/#EXTINF. Emitting a bare path list would strip
        // them from any playlist Ferrofin rewrites.
        let written = write_playlist_file(
            "/m/l.m3u",
            &[PlaylistEntry {
                path: "/m/a.flac".to_owned(),
                title: Some("Airbag".to_owned()),
                album: Some("OK Computer".to_owned()),
                album_artist: Some("Radiohead".to_owned()),
                artist: Some("Radiohead".to_owned()),
                duration_seconds: Some(284),
            }],
        );
        assert!(written.contains("#EXTALB:OK Computer"), "{written}");
        assert!(written.contains("#EXTART:Radiohead"), "{written}");
        assert!(written.contains("#EXTINF:284,Airbag"), "{written}");
        // An entry with no tags still gets its #EXTINF line, with the zero
        // duration PlaylistsNET writes for an unknown runtime.
        let bare = write_playlist_file("/m/l.m3u", &[PlaylistEntry::new("/m/a.flac")]);
        assert_eq!(bare, "#EXTM3U\n#EXTINF:0,\na.flac\n");
    }

    #[test]
    fn a_zpl_declares_its_own_version_and_carries_track_tags() {
        // `ZplContent` writes `<?zpl version="2.0"?>`, not the wpl header.
        let written = write_playlist_file(
            "/m/l.zpl",
            &[PlaylistEntry {
                path: "/m/a.flac".to_owned(),
                title: Some("Airbag".to_owned()),
                album: Some("OK Computer".to_owned()),
                ..PlaylistEntry::default()
            }],
        );
        assert!(written.starts_with("<?zpl version=\"2.0\"?>"), "{written}");
        assert!(written.contains("albumTitle=\"OK Computer\""), "{written}");
        assert!(written.contains("trackTitle=\"Airbag\""), "{written}");
        let wpl = write_playlist_file("/m/l.wpl", &[PlaylistEntry::new("/m/a.flac")]);
        assert!(wpl.starts_with("<?wpl version=\"1.0\"?>"), "{wpl}");
    }
}
