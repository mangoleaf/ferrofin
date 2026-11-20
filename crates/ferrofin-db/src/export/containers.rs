//! The metadata files Jellyfin keeps beside each playlist and box set
//! (gl-1903): `playlist.xml` and `collection.xml`.
//!
//! The problem: Jellyfin rewrites them on every change; Ferrofin does not
//! write a `playlist.xml` (its saver is gl-341), and wrote no `collection.xml`
//! before v1.6.0. So in a data directory Ferrofin adopted each one can hold
//! what Jellyfin last wrote, while the database holds the changes made since
//! (a share or a member removed, a rename); a file Ferrofin keeps current
//! from some release on is still stale for a container last changed before.
//! Jellyfin's metadata refresh reads the file and merges it into the item
//! (`PlaylistMetadataService.MergeData`: the file's shares, all of them when
//! the playlist has none), and `RestorePlaylistChildrenFromMetadata` refills
//! an empty playlist from it.
//!
//! Left out: the files in Jellyfin's own playlists and collections folders of
//! the data directory, which the export copies. Jellyfin resolves those
//! folders without them (`PlaylistResolver`, `BoxSetResolver` by the
//! `[boxset]` name) and writes each file again from the database on the
//! container's next save.
//!
//! Kept and reported: a `collection.xml` in the collections folder whose
//! folder's name lacks `[boxset]`, which alone makes the folder a box set
//! (`BoxSetResolver.cs`).
//!
//! Listed: the playlists and box sets in media libraries, which the export
//! never writes, so they keep their files.

use std::path::{Path, PathBuf};

use sqlx::SqliteConnection;

use super::Layout;
use crate::Result;

/// Jellyfin's playlists folder under program data (`{DataPath}/playlists`).
const PLAYLISTS: [&str; 2] = ["data", "playlists"];

/// Jellyfin's collections folder under program data
/// (`CollectionManager.GetCollectionsFolderPath`, `{DataPath}/collections`).
const COLLECTIONS: [&str; 2] = ["data", "collections"];

/// A playlist folder's metadata file (`PlaylistXmlSaver.DefaultPlaylistFilename`).
const PLAYLIST_XML: &str = "playlist.xml";

/// A box set folder's metadata file (`BoxSetXmlProvider`).
const COLLECTION_XML: &str = "collection.xml";

/// What makes a folder a box set by its name (`BoxSetResolver`, matched
/// ignoring case).
const BOXSET_TAG: &str = "[boxset]";

/// The stored types of the containers whose files Jellyfin reads, with what
/// the report calls them.
const CONTAINER_TYPES: [(&str, &str); 2] = [
    ("MediaBrowser.Controller.Playlists.Playlist", "playlist"),
    (
        "MediaBrowser.Controller.Entities.Movies.BoxSet",
        "collection",
    ),
];

/// What the export does with a container's metadata file in the data
/// directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFile {
    /// The `playlist.xml` of a folder in the playlists folder, or the
    /// `collection.xml` of a `[boxset]` folder in the collections folder:
    /// left out of the copy.
    LeftOut,
    /// The `collection.xml` of a folder in the collections folder whose name
    /// lacks `[boxset]`: copied, since Jellyfin resolves the folder as a box
    /// set by that file alone, though it may hold what Jellyfin last wrote.
    Kept,
}

/// What the export does with the file at `relative`, a path relative to the
/// data directory, when it is a container's metadata file there: matched by
/// the folder shape, so another file of the same name elsewhere is no such
/// file.
pub(super) fn classify(relative: &Path) -> Option<ContainerFile> {
    let parts: Vec<&std::ffi::OsStr> = relative.iter().collect();
    let [data, folder, container, file] = parts.as_slice() else {
        return None;
    };
    if [*data, *folder] == PLAYLISTS && *file == PLAYLIST_XML {
        return Some(ContainerFile::LeftOut);
    }
    if [*data, *folder] != COLLECTIONS || *file != COLLECTION_XML {
        return None;
    }
    let boxset = container
        .to_string_lossy()
        .to_lowercase()
        .contains(BOXSET_TAG);
    Some(if boxset {
        ContainerFile::LeftOut
    } else {
        ContainerFile::Kept
    })
}

/// A playlist or box set in a media library whose metadata file the export
/// cannot leave out, for the loss report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryContainer {
    /// `playlist` or `collection`.
    pub kind: &'static str,
    /// The file Jellyfin's metadata refresh reads for it, at the path the
    /// database stores: `playlist.xml` or `collection.xml` in its folder, or,
    /// for a playlist file, the file with the extension `.xml`
    /// (`PlaylistXmlSaver.GetSavePath`).
    pub file: PathBuf,
    /// Whether the file is there where the export ran. `false`: the
    /// container's own path is not, so whether it holds one is not known.
    pub found: bool,
}

/// The playlists and box sets the staged database stores outside `layout`'s
/// program data, once its paths are rewritten onto it (so outside the
/// export: in a media library), whose metadata file is there where the export
/// runs or whose path is not, sorted by file, each once. A container whose
/// path is there without a file is left out. The database has no `BaseItems`
/// table: none.
///
/// shortcut: a container stored inside the program data directory but outside
/// Jellyfin's playlists and collections folders (a media folder kept in the
/// data directory, which the export copies whole) keeps its file and is not
/// listed; list those too if such a layout is ever seen.
pub(super) async fn in_libraries(
    conn: &mut SqliteConnection,
    layout: &Layout,
) -> Result<Vec<LibraryContainer>> {
    if !super::paths::has_table(conn, "BaseItems").await? {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT "Type", "Path" FROM "BaseItems"
           WHERE "Type" IN (?1, ?2) AND "Path" IS NOT NULL"#,
    )
    .bind(CONTAINER_TYPES[0].0)
    .bind(CONTAINER_TYPES[1].0)
    .fetch_all(&mut *conn)
    .await?;
    let program_data = Path::new(layout.program_data);
    let rows: Vec<(&'static str, PathBuf)> = rows
        .into_iter()
        .filter_map(|(kind, path)| {
            let kind = CONTAINER_TYPES.iter().find(|(name, _)| *name == kind)?.1;
            let path = PathBuf::from(path);
            // A token path (`%AppDataPath%`) is no absolute one. shortcut: any other
            // relative path (or another platform's) is skipped too, never listed as
            // not known; list it if a database is ever seen to hold one.
            (path.is_absolute() && !path.starts_with(program_data)).then_some((kind, path))
        })
        .collect();
    super::blocking(move || Ok(look_for_files(rows))).await
}

/// [`in_libraries`]' look at the files, off the async runtime: a media
/// library may be on a slow mount. A path that cannot be looked at (no
/// permission) counts as not there: whether it holds a file is not known
/// either way, and the media are no reason to fail the export.
fn look_for_files(rows: Vec<(&'static str, PathBuf)>) -> Vec<LibraryContainer> {
    let mut found: Vec<LibraryContainer> = rows
        .into_iter()
        .filter_map(|(kind, path)| {
            let file = if kind == "collection" {
                path.join(COLLECTION_XML)
            } else if path.extension().is_some() && !path.is_dir() {
                // `Playlist.IsPlaylistFile`: an extension, and no directory there.
                path.with_extension("xml")
            } else {
                path.join(PLAYLIST_XML)
            };
            if file.exists() {
                Some(LibraryContainer {
                    kind,
                    file,
                    found: true,
                })
            } else {
                (!path.exists()).then_some(LibraryContainer {
                    kind,
                    file,
                    found: false,
                })
            }
        })
        .collect();
    found.sort_by(|a, b| a.file.cmp(&b.file));
    // Rows that share a path (FixIncorrectOwnerIdRelationships' case) are one container.
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ContainerFile, classify, look_for_files};

    #[test]
    fn only_the_two_folder_shapes_are_container_files() {
        for (path, wanted) in [
            (
                "data/playlists/Mix/playlist.xml",
                Some(ContainerFile::LeftOut),
            ),
            (
                "data/collections/Saga [boxset]/collection.xml",
                Some(ContainerFile::LeftOut),
            ),
            (
                "data/collections/Saga [BoxSet]/collection.xml",
                Some(ContainerFile::LeftOut),
            ),
            (
                "data/collections/Saga/collection.xml",
                Some(ContainerFile::Kept),
            ),
            // Another file of the same name, elsewhere or deeper, is no such file.
            ("data/playlists/playlist.xml", None),
            ("data/playlists/Mix/sub/playlist.xml", None),
            ("metadata/library/ab/playlist.xml", None),
            ("data/collections/Saga [boxset]/playlist.xml", None),
            ("data/playlists/Mix [boxset]/collection.xml", None),
            ("data/playlists/Mix/poster.jpg", None),
            ("data/playlists/Mix/Playlist.xml", None),
            ("root/default/Movies/collection.xml", None),
        ] {
            assert_eq!(classify(Path::new(path)), wanted, "{path}");
        }
    }

    /// A container whose file is there is listed, and one whose path is not
    /// is listed as not found; one whose path is there without a file is not.
    /// A playlist file's metadata file is the same name with `.xml`.
    #[test]
    fn library_containers_are_listed_by_what_the_export_finds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for folder in [
            "Saga [boxset]",
            "Bare [boxset]",
            "Mix [playlist]",
            "Best.of [playlist]",
        ] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        std::fs::write(root.join("Saga [boxset]/collection.xml"), b"<Item/>").unwrap();
        std::fs::write(root.join("Mix [playlist]/playlist.xml"), b"<Item/>").unwrap();
        // A folder whose name has a dot is a folder, not a playlist file.
        std::fs::write(root.join("Best.of [playlist]/playlist.xml"), b"<Item/>").unwrap();
        std::fs::write(root.join("Road.m3u"), b"#EXTM3U").unwrap();
        std::fs::write(root.join("Road.xml"), b"<Item/>").unwrap();
        std::fs::write(root.join("Plain.m3u"), b"#EXTM3U").unwrap();

        let found = look_for_files(vec![
            ("collection", root.join("Saga [boxset]")),
            ("collection", root.join("Saga [boxset]")),
            ("collection", root.join("Bare [boxset]")),
            ("playlist", root.join("Best.of [playlist]")),
            ("collection", root.join("Gone [boxset]")),
            ("playlist", root.join("Mix [playlist]")),
            ("playlist", root.join("Road.m3u")),
            ("playlist", root.join("Plain.m3u")),
        ]);

        let listed: Vec<(&str, String, bool)> = found
            .iter()
            .map(|c| {
                let file = c.file.strip_prefix(root).unwrap().display().to_string();
                (c.kind, file, c.found)
            })
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "playlist",
                    "Best.of [playlist]/playlist.xml".to_owned(),
                    true
                ),
                (
                    "collection",
                    "Gone [boxset]/collection.xml".to_owned(),
                    false
                ),
                ("playlist", "Mix [playlist]/playlist.xml".to_owned(), true),
                ("playlist", "Road.xml".to_owned(), true),
                (
                    "collection",
                    "Saga [boxset]/collection.xml".to_owned(),
                    true
                ),
            ]
        );
    }
}
