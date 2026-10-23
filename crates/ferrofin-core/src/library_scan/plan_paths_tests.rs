//! The scoped plan ([`LibraryScanner::plan_paths`]) against the full one.
//!
//! The invariant a path-scoped scan (the library monitor, a webhook, a folder
//! or item refresh) rests on: planning only some paths resolves exactly what
//! the full plan resolves for them — the same items in the same order, with
//! the same ids, presentation keys, ancestors and rows — so a scoped scan and
//! a library scan never disagree about an item.

use std::sync::{Arc, Mutex};

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::entities_media::VirtualFolderInfo;
use ferrofin_model::io::FileSystemEntryInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::VirtualFolderManager;
use uuid::Uuid;

use super::{LibraryScanner, PlanScope, Planned, path_is_under};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// A real filesystem that records every directory the planner lists.
#[derive(Default)]
struct CountingFs {
    listed: Mutex<Vec<String>>,
}

impl CountingFs {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.listed.lock().unwrap())
    }
}

impl FileSystem for CountingFs {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.listed.lock().unwrap().push(path.to_owned());
        FerrofinFileSystem::new().get_file_system_entries(path)
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        Vec::new()
    }
    fn file_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().file_exists(path)
    }
    fn directory_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().directory_exists(path)
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        FerrofinFileSystem::new().get_files(path, extensions)
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        FerrofinFileSystem::new().read_file(path)
    }
}

/// A scanner over `fs` (planning needs no database, but the scanner wants
/// its seams).
async fn scanner(dir: &std::path::Path, fs: Arc<CountingFs>) -> LibraryScanner {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db));
    let vf: Arc<dyn VirtualFolderManager> =
        Arc::new(FerrofinVirtualFolderManager::new(dir.join("default")));
    LibraryScanner::new(vf, fs, persistence)
}

/// Creates the empty file (and its directories) `rel` under `root`.
fn touch(root: &std::path::Path, rel: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"").unwrap();
}

/// A library of `ct` over `location`.
fn library(
    n: u128,
    ct: Option<CollectionTypeOptions>,
    location: &std::path::Path,
) -> VirtualFolderInfo {
    VirtualFolderInfo {
        name: Some(format!("lib{n}")),
        locations: vec![location.to_string_lossy().into_owned()],
        collection_type: ct,
        library_options: None,
        item_id: Some(Uuid::from_u128(0xF000 + n).to_string()),
        primary_image_item_id: None,
        refresh_progress: None,
        refresh_status: None,
    }
}

/// The movies library's files: movies alone, in their own folders and in a
/// plain collection folder, extras by suffix, folder and theme song, version
/// groups (one an alternate with its own stack), stacks in a folder and at
/// the root, disc rips and a soundtrack.
const MOVIES: &[&str] = &[
    "Alien (1979).mkv",
    "Alien (1979)-trailer.mkv",
    "Heat (1995)/Heat (1995).mkv",
    "Heat (1995)/Heat (1995)-trailer.mkv",
    "Ronin (1998)/Ronin (1998) - 1080p.mkv",
    "Ronin (1998)/Ronin (1998) - 2160p.mkv",
    "Ronin (1998)/Ronin (1998) - 1080p-trailer.mkv",
    "Twins (1988)/Twins (1988).mkv",
    "Twins (1988)/Twins (1988) - 1080p part1.mkv",
    "Twins (1988)/Twins (1988) - 1080p part2.mkv",
    "Heist cd1.mkv",
    "Heist cd2.mkv",
    "Heat (1995)/trailers/Teaser.mkv",
    "Heat (1995)/extras/Behind the Scenes.mkv",
    "Heat (1995)/theme.mp3",
    "Blade Runner (1982)/VIDEO_TS/VTS_01_1.VOB",
    "Blade Runner (1982)/VIDEO_TS/VIDEO_TS.IFO",
    "Dune (2021)/BDMV/index.bdmv",
    "Dune (2021)/BDMV/STREAM/00001.m2ts",
    "Dune (2021)/trailers/Dune Trailer.mkv",
    "Jaws (1975)/VIDEO_TS.IFO",
    "Jaws (1975)/VTS_01_1.VOB",
    "Jaws (1975)/Jaws (1975)-trailer.mkv",
    "Sound of Music/Sound of Music (1965) (Disc 01)/VIDEO_TS/VTS_01_1.VOB",
    "Sound of Music/Sound of Music (1965) (Disc 02)/VIDEO_TS/VTS_01_1.VOB",
    // No extra of the set's: its owner folder is the first disc, and the
    // name is not the set's (`ExtraResolver.cs:71,85-91`).
    "Sound of Music/trailers/Sound of Music Trailer.mkv",
    // Named after the set's title and year: its trailer.
    "Sound of Music/Sound of Music (1965)-trailer.mkv",
    "Collections/Alien/Aliens (1986).mkv",
    "Collections/Alien/Alien 3 (1992).mkv",
    "Soundtrack/score.mp3",
];

/// The untyped library's files: movies, a series by its `tvshow.nfo`, one by
/// its season folder, a folder of plain videos (`season.nfo`), and audio.
const MIXED: &[&str] = &[
    "Film.mkv",
    "Folder/Other Film (2001).mkv",
    "Folder/pic.jpg",
    "Show/tvshow.nfo",
    "Show/Show S01E01.mkv",
    "Anthology/Season 1/Anthology S01E01.mkv",
    "Anthology/Season 1/Anthology S01E02.mkv",
    "Notes/season.nfo",
    "Notes/A.mkv",
    "song.mp3",
    "Mix.m3u",
    "Folder/list.m3u8",
    "Mixes/list.m3u8",
    "Show/Season 1/show.pls",
    "Road [Playlist]/a.mkv",
];

/// One fixture tree per library type — each shape a resolver treats
/// specially: movies with versions, extras (by suffix, folder and theme
/// song) and disc rips; series with season folders, specials, multi-episode
/// files and season-less layouts; music with artists, multi-disc albums,
/// loose artist tracks, an `artist.nfo` artist, a plain grouping folder and
/// loose tracks in the library root; books with single-book and
/// single-audiobook folders and loose files; home videos with photo albums,
/// nested albums, a folder without photos and a video's own artwork; and a
/// mixed library.
fn fixture(root: &std::path::Path) -> Vec<VirtualFolderInfo> {
    let movies = root.join("movies");
    for rel in MOVIES {
        touch(&movies, rel);
    }
    let tv = root.join("tv");
    for rel in [
        "Firefly (2002)/Season 01/Firefly S01E01.mkv",
        "Firefly (2002)/Season 01/Firefly S01E02-E03.mkv",
        "Firefly (2002)/Season 01/extras/Firefly S01E90.mkv",
        "Firefly (2002)/Specials/Firefly S00E01.mkv",
        "Firefly (2002)/Season 00/Firefly S00E02.mkv",
        "Flat Show/Flat Show S01E01.mkv",
        "Flat Show/Flat Show S02E01.mkv",
        "Flat Show/Flat Show Pilot.mkv",
        "Flat Show/bonus/Flat Show S01E05.mkv",
        "Other Show/Season 1/Other Show 1x01.mkv",
        "S.W.A.T. (2017)/Season 01/S.W.A.T. (2017) S01E01.mkv",
        "Mixtape [playlist]/a.mkv",
        "Firefly (2002)/Season 01/Gag Reel [playlist]/a.mkv",
        "Flat Show/bonus/deeper [playlist]/b.mkv",
    ] {
        touch(&tv, rel);
    }
    let music = root.join("music");
    for rel in [
        "root track.mp3",
        "Artist A/Album 1/01 - Song.mp3",
        "Artist A/Album 1/02 - Song.mp3",
        "Artist A/Album 2/CD1/01 - One.flac",
        "Artist A/Album 2/CD2/01 - Two.flac",
        "Artist A/loose track.mp3",
        "Artist A/Greatest Hits Vol. 2/01 - Hit.mp3",
        "Solo Artist/artist.nfo",
        "Solo Artist/single.mp3",
        "Compilations/Various/01 - Mix.mp3",
        "Compilations/Various/CD1/02 - Mix.mp3",
        "Just An Album/01 - Only.ogg",
        "Artist A/Album 1/Album 1.m3u",
        "Artist A/Album 1/Mix.pls",
        "Artist A/Album 2/CD2/disc.wpl",
        "Artist A/artist mix.m3u8",
        "Compilations/top.zpl",
        "Mixtapes/side.m3u",
        "root list.m3u",
        "Road Trip [playlist]/x.mp3",
        "Lists/playlist.xml",
        "Lists/y.mp3",
        "Artist A/Album 1/Best Of [playlist]/z.mp3",
        "Rock/Artist C/albums/Album 3/01 - Three.mp3",
    ] {
        touch(&music, rel);
    }
    let books = root.join("books");
    for rel in [
        "loose.pdf",
        "loose.m4b",
        "Dracula/dracula.epub",
        "Author/Title (2011)/book.m4b",
        "Series/book1.epub",
        "Series/book2.epub",
        "Cued/rip.m4b",
        "Cued/rip.cue",
        "Reading [playlist]/novel.epub",
    ] {
        touch(&books, rel);
    }
    let home = root.join("home");
    for rel in [
        "root.jpg",
        "clip.mp4",
        "clip.jpg",
        "Lesson pt1.mp4",
        "Lesson pt2.mp4",
        "Trip 2024/img1.jpg",
        "Trip 2024/day2/img2.jpg",
        "Trip 2024/day2/movie.mkv",
        "Trip 2024/Best [playlist]/img3.jpg",
        "No Photos/Sub/x.png",
    ] {
        touch(&home, rel);
    }
    let mixed = root.join("mixed");
    for rel in MIXED {
        touch(&mixed, rel);
    }
    // A directory that differs from `tv` only in case: a library of its own
    // on a case-sensitive filesystem, which a series' presentation key still
    // names (`GetCollectionFolders` matches locations ignoring case).
    let tv_upper = root.join("TV");
    touch(&tv_upper, "Serenity/Season 1/Serenity S01E01.mkv");
    // A location given with a trailing slash.
    let mut books_library = library(4, Some(CollectionTypeOptions::books), &books);
    books_library.locations[0].push('/');
    vec![
        library(1, Some(CollectionTypeOptions::movies), &movies),
        library(2, Some(CollectionTypeOptions::tvshows), &tv),
        library(3, Some(CollectionTypeOptions::music), &music),
        books_library,
        library(5, Some(CollectionTypeOptions::homevideos), &home),
        library(6, Some(CollectionTypeOptions::mixed), &mixed),
        library(7, None, &mixed),
        // A second library whose location is the series' parent folder.
        library(8, Some(CollectionTypeOptions::tvshows), &tv),
        library(9, Some(CollectionTypeOptions::tvshows), &tv_upper),
    ]
}

/// What the invariant compares for one planned item: its id, its ancestor
/// closure, and its row (presentation keys included) — with a directory's
/// (a folder's, a disc rip's) `DateCreated` cleared, which the planner
/// stamps with the clock.
fn canon(items: Vec<Planned>) -> Vec<(Uuid, Vec<Uuid>, BaseItemEntity)> {
    items
        .into_iter()
        .map(|p| {
            let mut entity = p.entity;
            // A directory — a folder's, a disc rip's — and a path-less
            // folder (a season with no folder) are stamped with the resolve
            // time.
            if crate::item_data::path_is_directory(&entity)
                || (entity.is_folder && entity.path.is_none())
            {
                entity.date_created = None;
            }
            (p.id, p.ancestors, entity)
        })
        .collect()
}

/// The full plan filtered to `paths` (and `exact`), as a scoped scan used to
/// build it: the items at, under or above a path, the path-less virtual
/// seasons those items sit in, and the rest of a kept video's local group —
/// its primary, stacked parts and alternate versions, linked by `OwnerId`
/// (no extra), which a scoped plan plans whole — and a multi-disc movie
/// one of whose `AdditionalParts` disc folders (which have no rows) is.
fn filtered(full: &[Planned], paths: &[String], exact: Option<&str>) -> Vec<Planned> {
    let kept = |path: &str| {
        exact == Some(path)
            || paths
                .iter()
                .any(|c| path_is_under(path, c) || path_is_under(c, path))
    };
    let by_path = |p: &Planned| {
        p.entity.path.as_deref().is_some_and(kept)
            || crate::item_data::parse_data(p.entity.data.as_deref())
                .get("AdditionalParts")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|parts| {
                    parts
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .any(|part| crate::item_data::path_is_directory(&p.entity) && kept(part))
                })
    };
    let parents: std::collections::HashSet<String> = full
        .iter()
        .filter(|p| by_path(p))
        .flat_map(|p| [p.entity.parent_id.clone(), p.entity.season_id.clone()])
        .flatten()
        .collect();
    let owners: std::collections::HashMap<&str, &str> = full
        .iter()
        .filter(|p| p.entity.extra_type.is_none())
        .filter_map(|p| Some((p.entity.id.as_str(), p.entity.owner_id.as_deref()?)))
        .collect();
    let head = |p: &Planned| {
        let mut id = p.entity.id.as_str();
        while let Some(owner) = owners.get(id) {
            id = owner;
        }
        id.to_owned()
    };
    let groups: std::collections::HashSet<String> =
        full.iter().filter(|p| by_path(p)).map(head).collect();
    full.iter()
        .filter(|p| {
            by_path(p)
                || (p.entity.path.is_none() && parents.contains(&p.entity.id))
                || groups.contains(&head(p))
        })
        .map(|p| Planned {
            id: p.id,
            entity: p.entity.clone(),
            ancestors: p.ancestors.clone(),
            generated_names: p.generated_names.clone(),
        })
        .collect()
}

/// The owner of `p` when it is a local group's member — a stacked part or
/// a local alternate version (owned, no extra).
fn owned_by(p: &Planned) -> Option<&str> {
    p.entity
        .owner_id
        .as_deref()
        .filter(|_| p.entity.extra_type.is_none())
}

/// Every local group shape is present in `full`: a stacked part (owned, no
/// extra, pointing at nothing), a local alternate version (owned and
/// pointing at its primary), and an alternate's own part.
fn assert_group_shapes(full: &[Planned]) {
    let alternates: Vec<&str> = full
        .iter()
        .filter(|p| owned_by(p).is_some() && p.entity.primary_version_id.is_some())
        .map(|p| p.entity.id.as_str())
        .collect();
    assert!(!alternates.is_empty(), "the fixture plans no alternate");
    let parts: Vec<&Planned> = full
        .iter()
        .filter(|p| owned_by(p).is_some() && p.entity.primary_version_id.is_none())
        .collect();
    assert!(
        parts
            .iter()
            .any(|p| !alternates.contains(&owned_by(p).unwrap())),
        "the fixture plans no primary's part"
    );
    assert!(
        parts
            .iter()
            .any(|p| alternates.contains(&owned_by(p).unwrap())),
        "the fixture plans no alternate's part"
    );
}

/// For every planned item of every fixture library — and for folders that
/// are no item, the library roots (with and without a trailing slash), the
/// inside of a disc rip, and paths that do not exist — planning that one
/// path, through the libraries a path-scoped scan walks for it
/// (`affected_libraries`), yields exactly the full plan filtered to it.
// The fixture's path list is one table.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn plan_paths_matches_the_filtered_full_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let folders = fixture(tmp.path());
    let fs = Arc::new(CountingFs::default());
    let scanner = scanner(tmp.path(), Arc::clone(&fs)).await;
    let full = scanner.plan(&folders);
    // Every resolver shape is present.
    for kind in [
        "Movies.Movie",
        "Entities.Trailer",
        "Entities.Video",
        "Audio.Audio",
        "TV.Series",
        "TV.Season",
        "TV.Episode",
        "Audio.MusicAlbum",
        "Audio.MusicArtist",
        "Entities.Book",
        "Entities.AudioBook",
        "Entities.Photo",
        "Entities.PhotoAlbum",
        "Playlists.Playlist",
    ] {
        assert!(
            full.iter().any(|p| p.entity.type_.ends_with(kind)),
            "the fixture plans no {kind}"
        );
    }
    assert_group_shapes(&full);
    // A dotted directory name is a name, not a file name with an extension.
    let named = |kind: &str, name: &str| {
        full.iter()
            .any(|p| p.entity.type_.ends_with(kind) && p.entity.name.as_deref() == Some(name))
    };
    assert!(named("Audio.MusicAlbum", "Greatest Hits Vol. 2"));
    assert!(
        full.iter().any(|p| p.entity.type_.ends_with("TV.Series")
            && p.entity
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with("S.W.A.T."))),
        "the series keeps its dotted name"
    );
    let mut paths: Vec<String> = full.iter().filter_map(|p| p.entity.path.clone()).collect();
    let root = tmp.path().to_string_lossy().into_owned();
    for extra in [
        "movies",
        "movies/Heat (1995)/trailers",
        "movies/Collections",
        "movies/Collections/Alien",
        "movies/Blade Runner (1982)/VIDEO_TS/VTS_01_1.VOB",
        "movies/Dune (2021)/BDMV/STREAM/00001.m2ts",
        "movies/Dune (2021)/BDMV",
        "movies/Dune (2021)/trailers",
        "movies/Jaws (1975)/VTS_01_1.VOB",
        "movies/Sound of Music",
        "movies/Sound of Music/Sound of Music (1965) (Disc 02)/VIDEO_TS/VTS_01_1.VOB",
        "movies/Sound of Music/trailers",
        "movies/Gone (2000).mkv",
        "tv",
        "tv/",
        "books/",
        "TV",
        "tv/Firefly (2002)/Season 01/extras",
        "tv/Flat Show/bonus",
        "tv/Firefly (2002)/Season 03/Firefly S03E01.mkv",
        "music",
        "music/Artist A/Album 2/CD1",
        "music/Compilations",
        "music/Artist A/Missing Album",
        "books/Series",
        "home/No Photos",
        "home/Trip 2024/day2",
        "home/Trip 2024/Best [playlist]/img3.jpg",
        "mixed/Folder",
        "music/Road Trip [playlist]/x.mp3",
        "music/Artist A/Album 1/Best Of [playlist]/z.mp3",
        "books/Reading [playlist]/novel.epub",
        "music/Lists/y.mp3",
        "music/Rock",
        "music/Rock/Artist C/albums",
        "music/Rock/Artist C/albums/Album 3",
        "tv/Mixtape [playlist]/a.mkv",
        "tv/Firefly (2002)/Season 01/Gag Reel [playlist]/a.mkv",
        "tv/Flat Show/bonus/deeper [playlist]/b.mkv",
        "mixed/Road [Playlist]/a.mkv",
    ] {
        paths.push(format!("{root}/{extra}"));
    }
    paths.sort();
    paths.dedup();
    assert!(paths.len() > 60, "{} paths", paths.len());
    for path in &paths {
        let scope = std::slice::from_ref(path);
        let scoped = scanner.plan_paths(&folders, PlanScope::paths(scope, None));
        assert_eq!(
            canon(scoped),
            canon(filtered(&full, scope, None)),
            "plan_paths([{path}]) differs from the filtered full plan"
        );
    }
    // Several paths at once, overlapping ones included, keep the full
    // plan's order and list no item twice.
    let several = vec![
        format!("{root}/tv/Firefly (2002)/Season 01/Firefly S01E01.mkv"),
        format!("{root}/tv/Firefly (2002)/Specials"),
        format!("{root}/tv/Firefly (2002)"),
        format!("{root}/music/Artist A/Album 2/CD2/01 - Two.flac"),
        format!("{root}/movies/Heat (1995)/trailers/Teaser.mkv"),
    ];
    assert_eq!(
        canon(scanner.plan_paths(&folders, PlanScope::paths(&several, None))),
        canon(filtered(&full, &several, None)),
    );
    // `RefreshArtist`'s shape: the children of some folders plus one item
    // alone.
    let albums = vec![format!("{root}/music/Artist A/Album 1")];
    let artist = format!("{root}/music/Solo Artist");
    assert_eq!(
        canon(scanner.plan_paths(&folders, PlanScope::paths(&albums, Some(&artist)))),
        canon(filtered(&full, &albums, Some(&artist))),
    );
}

/// `PlaylistResolver` answers first in every walk (priority First): each
/// playlist folder is a `Playlist` under the item around it — or the
/// library, where the walk plans no row for the folder holding it — with
/// nothing planned inside it, and each playlist file of a music or untyped
/// library is one, a `.pls` too.
#[tokio::test]
async fn playlists_resolve_before_every_other_resolver() {
    let tmp = tempfile::tempdir().unwrap();
    let folders = fixture(tmp.path());
    let scanner = scanner(tmp.path(), Arc::new(CountingFs::default())).await;
    let full = scanner.plan(&folders);
    let root = tmp.path().to_string_lossy().into_owned();
    let at = |rel: &str| {
        let path = format!("{root}/{rel}");
        full.iter()
            .find(|p| p.entity.path.as_deref() == Some(path.as_str()))
            .unwrap_or_else(|| panic!("nothing planned at {rel}"))
    };
    let library = |n: u128| Uuid::from_u128(0xF000 + n);
    let db = |id: Uuid| Some(ferrofin_db::store::guid_to_db(id));
    for (rel, parent) in [
        // TV: a series-level folder, a season's subfolder, a nested one.
        ("tv/Mixtape [playlist]", library(2)),
        (
            "tv/Firefly (2002)/Season 01/Gag Reel [playlist]",
            at("tv/Firefly (2002)/Season 01").id,
        ),
        (
            "tv/Flat Show/bonus/deeper [playlist]",
            at("tv/Flat Show").id,
        ),
        // Music: folders by name and by `playlist.xml`, an album's subfolder.
        ("music/Road Trip [playlist]", library(3)),
        ("music/Lists", library(3)),
        (
            "music/Artist A/Album 1/Best Of [playlist]",
            at("music/Artist A/Album 1").id,
        ),
        // Music files: in an album, a disc folder, an artist (`Compilations`
        // holds an album), a folder that is neither (a `Folder` of its own),
        // the root.
        (
            "music/Artist A/Album 1/Album 1.m3u",
            at("music/Artist A/Album 1").id,
        ),
        (
            "music/Artist A/Album 1/Mix.pls",
            at("music/Artist A/Album 1").id,
        ),
        (
            "music/Artist A/Album 2/CD2/disc.wpl",
            at("music/Artist A/Album 2").id,
        ),
        ("music/Artist A/artist mix.m3u8", at("music/Artist A").id),
        ("music/Compilations/top.zpl", at("music/Compilations").id),
        ("music/Mixtapes/side.m3u", at("music/Mixtapes").id),
        ("music/root list.m3u", library(3)),
        // Books, home videos (inside a photo album), untyped.
        ("books/Reading [playlist]", library(4)),
        ("home/Trip 2024/Best [playlist]", at("home/Trip 2024").id),
        ("mixed/Road [Playlist]", library(6)),
        ("mixed/Mix.m3u", library(6)),
        ("mixed/Mixes/list.m3u8", at("mixed/Mixes").id),
        ("mixed/Show/Season 1/show.pls", at("mixed/Show/Season 1").id),
    ] {
        let playlist = at(rel);
        assert!(
            playlist.entity.type_.ends_with("Playlists.Playlist"),
            "{rel}: {}",
            playlist.entity.type_
        );
        assert_eq!(playlist.entity.parent_id, db(parent), "{rel}");
        assert_eq!(playlist.ancestors.last(), Some(&parent), "{rel}");
        let inside = format!("{root}/{rel}/");
        assert!(
            !full.iter().any(|p| p
                .entity
                .path
                .as_deref()
                .is_some_and(|path| path.starts_with(&inside))),
            "something is planned inside {rel}"
        );
    }
    // Beside a movie in its own folder nothing resolves (upstream's movie is
    // no folder): no playlist there.
    let own = format!("{root}/mixed/Folder/list.m3u8");
    assert!(
        !full
            .iter()
            .any(|p| p.entity.path.as_deref() == Some(own.as_str())),
        "a playlist beside a movie in its own folder"
    );
    // The folder-arm names: the marker cut, any case.
    assert_eq!(
        at("mixed/Road [Playlist]").entity.name.as_deref(),
        Some("Road")
    );
}

/// A loose episode of a series with no season folders brings its virtual
/// season (no path of its own) along in a scoped plan: the row the full plan
/// groups it under, parent of the episode — never a dangling parent id —
/// and the series' own scoped plan holds every virtual season the full plan
/// gives it.
#[tokio::test]
async fn plan_paths_plans_a_loose_episodes_virtual_season() {
    let tmp = tempfile::tempdir().unwrap();
    let folders = fixture(tmp.path());
    let scanner = scanner(tmp.path(), Arc::new(CountingFs::default())).await;
    let full = scanner.plan(&folders);
    let root = tmp.path().to_string_lossy().into_owned();
    let loose = format!("{root}/tv/Flat Show/Flat Show S02E01.mkv");
    let scoped = scanner.plan_paths(
        &folders,
        PlanScope::paths(std::slice::from_ref(&loose), None),
    );
    let season = scoped
        .iter()
        .find(|p| p.entity.path.is_none())
        .expect("the loose episode's virtual season is planned");
    assert_eq!(season.entity.index_number, Some(2));
    let episode = scoped
        .iter()
        .find(|p| p.entity.path.as_deref() == Some(loose.as_str()))
        .expect("the episode");
    let series = scoped
        .iter()
        .find(|p| p.entity.type_.ends_with("TV.Series"))
        .expect("the series");
    assert_eq!(
        episode.entity.parent_id.as_deref(),
        Some(series.entity.id.as_str()),
    );
    assert_eq!(
        episode.entity.season_id.as_deref(),
        Some(season.entity.id.as_str()),
    );
    assert!(episode.ancestors.contains(&season.id));
    assert_eq!(
        canon(scoped),
        canon(filtered(&full, std::slice::from_ref(&loose), None))
    );
    // The series' own scan plans every virtual season it holds.
    let series = format!("{root}/tv/Flat Show");
    let scoped = scanner.plan_paths(
        &folders,
        PlanScope::paths(std::slice::from_ref(&series), None),
    );
    assert_eq!(
        scoped.iter().filter(|p| p.entity.path.is_none()).count(),
        full.iter()
            .filter(
                |p| p.entity.path.is_none() && p.entity.series_name.as_deref() == Some("Flat Show")
            )
            .count(),
    );
}

/// A scoped plan lists only the directories that lead to its path — the
/// library root, the series and the season of one episode — not the other
/// series of the library, which the full plan walks.
#[tokio::test]
async fn plan_paths_walks_only_the_path_to_the_item() {
    let tmp = tempfile::tempdir().unwrap();
    let tv = tmp.path().join("tv");
    for series in 0..40 {
        for season in 1..=3 {
            for episode in 1..=4 {
                touch(
                    &tv,
                    &format!(
                        "Show {series}/Season {season}/Show {series} S0{season}E0{episode}.mkv"
                    ),
                );
            }
        }
    }
    let music = tmp.path().join("music");
    for artist in 0..30 {
        for album in 0..3 {
            touch(
                &music,
                &format!("Artist {artist}/Album {album}/01 - Track.mp3"),
            );
        }
    }
    let folders = vec![
        library(1, Some(CollectionTypeOptions::tvshows), &tv),
        library(2, Some(CollectionTypeOptions::music), &music),
    ];
    let fs = Arc::new(CountingFs::default());
    let scanner = scanner(tmp.path(), Arc::clone(&fs)).await;

    scanner.plan(&folders);
    let full_walk = fs.take().len();

    let tv_root = tv.to_string_lossy().into_owned();
    let episode = format!("{tv_root}/Show 7/Season 2/Show 7 S02E03.mkv");
    let planned = scanner.plan_paths(
        &folders,
        PlanScope::paths(std::slice::from_ref(&episode), None),
    );
    let listed = fs.take();
    assert_eq!(
        planned
            .iter()
            .map(|p| p.entity.path.clone().unwrap())
            .collect::<Vec<_>>(),
        vec![
            format!("{tv_root}/Show 7"),
            format!("{tv_root}/Show 7/Season 2"),
            episode.clone(),
        ],
    );
    assert_eq!(
        listed,
        vec![
            tv_root.clone(),
            format!("{tv_root}/Show 7"),
            format!("{tv_root}/Show 7/Season 2"),
        ],
        "only the episode's library root, series and season are listed"
    );
    assert!(
        full_walk > 150,
        "the full plan walked {full_walk} directories"
    );

    // A track: its artist's albums are looked at (the artist resolver's own
    // rule) but no other artist's folder is.
    let music_root = music.to_string_lossy().into_owned();
    let track = format!("{music_root}/Artist 12/Album 1/01 - Track.mp3");
    let planned = scanner.plan_paths(
        &folders,
        PlanScope::paths(std::slice::from_ref(&track), None),
    );
    let listed = fs.take();
    assert_eq!(planned.len(), 3, "the artist, the album and the track");
    assert!(
        listed
            .iter()
            .all(|dir| *dir == music_root || path_is_under(dir, &format!("{music_root}/Artist 12"))),
        "listed outside the track's artist: {listed:?}"
    );
}

/// Parallel discovery must keep the serial plan, including extras ownership and
/// deletion guards, for every resolver. One scanner observes saved limit changes.
#[tokio::test]
async fn live_fanout_keeps_full_and_scoped_plans_identical() {
    use std::sync::atomic::{AtomicI32, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let folders = fixture(tmp.path());
    touch(tmp.path(), "movies/Ignored/.ignore");
    touch(tmp.path(), "movies/Ignored/Hidden.mkv");
    let fs = Arc::new(CountingFs::default());
    let setting = Arc::new(AtomicI32::new(1));
    let source = Arc::clone(&setting);
    let scanner = scanner(tmp.path(), fs)
        .await
        .with_scan_fanout(move || source.load(Ordering::Acquire));
    let baseline = scanner.plan_in(
        &folders,
        &folders,
        PlanScope::ALL,
        super::DateAdded::default(),
        None,
        None,
    );
    let expected = canon(baseline.items);
    let roots = [
        tmp.path()
            .join("tv/Firefly (2002)/Season 01")
            .to_string_lossy()
            .into_owned(),
        tmp.path()
            .join("movies/Heat (1995)")
            .to_string_lossy()
            .into_owned(),
    ];
    let scoped = canon(scanner.plan_paths(&folders, PlanScope::paths(&roots, None)));
    for limit in [2, 4, 1] {
        setting.store(limit, Ordering::Release);
        let actual = scanner.plan_in(
            &folders,
            &folders,
            PlanScope::ALL,
            super::DateAdded::default(),
            None,
            None,
        );
        assert_eq!(canon(actual.items), expected, "limit {limit}");
        assert_eq!(actual.owner_paths, baseline.owner_paths);
        for (mut got, want) in [
            (actual.excluded, &baseline.excluded),
            (actual.unlisted, &baseline.unlisted),
            (actual.inaccessible, &baseline.inaccessible),
        ] {
            let mut want = want.clone();
            got.sort();
            want.sort();
            assert_eq!(got, want);
        }
        assert_eq!(
            canon(scanner.plan_paths(&folders, PlanScope::paths(&roots, None))),
            scoped
        );
    }
}

/// A library built from linked folders — a series, a season and a movie
/// folder on another drive, linked in — plans the items under the link
/// paths, as Jellyfin's `ManagedFileSystem` does; a link back to the
/// library's own root adds nothing and ends the walk.
#[cfg(unix)]
#[tokio::test]
async fn plan_follows_linked_folders_and_stops_at_a_loop() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let drive = root.join("other drive");
    touch(&drive, "Linked Show/Season 01/Linked Show S01E01.mkv");
    touch(&drive, "Split Show/Season 01/Split Show S01E01.mkv");
    touch(&drive, "Linked Movie (2001)/Linked Movie (2001).mkv");
    let tv = root.join("tv");
    let movies = root.join("movies");
    std::fs::create_dir_all(tv.join("Split Show")).unwrap();
    std::fs::create_dir_all(&movies).unwrap();
    symlink(drive.join("Linked Show"), tv.join("Linked Show")).unwrap();
    symlink(
        drive.join("Split Show/Season 01"),
        tv.join("Split Show/Season 01"),
    )
    .unwrap();
    symlink(&tv, drive.join("Linked Show/loop")).unwrap();
    symlink(
        drive.join("Linked Movie (2001)"),
        movies.join("Linked Movie (2001)"),
    )
    .unwrap();
    let folders = vec![
        library(1, Some(CollectionTypeOptions::tvshows), &tv),
        library(2, Some(CollectionTypeOptions::movies), &movies),
    ];

    let scanner = scanner(root, Arc::new(CountingFs::default())).await;
    let paths: Vec<String> = scanner
        .plan(&folders)
        .into_iter()
        .filter_map(|p| p.entity.path)
        .collect();
    let tv = tv.to_string_lossy();
    let movies = movies.to_string_lossy();
    for linked in [
        format!("{tv}/Linked Show"),
        format!("{tv}/Linked Show/Season 01/Linked Show S01E01.mkv"),
        format!("{tv}/Split Show/Season 01"),
        format!("{tv}/Split Show/Season 01/Split Show S01E01.mkv"),
        format!("{movies}/Linked Movie (2001)/Linked Movie (2001).mkv"),
    ] {
        assert!(paths.contains(&linked), "{linked} is planned: {paths:#?}");
    }
    assert!(
        !paths.iter().any(|path| path.contains("/loop")),
        "a link back to the library root is not descended: {paths:#?}"
    );
}

/// `DotIgnoreIgnoreRule` (`LibraryManager.IgnoreFile`): a `.ignore` file hides
/// what lies beneath its folder from the plan, everything when it is empty
/// and otherwise what its gitignore lines match. The plan records the hidden
/// entries as excluded, so stored rows under them are pruned.
#[tokio::test]
async fn dot_ignore_files_hide_paths_from_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let movies = tmp.path().join("movies");
    for rel in [
        "Heat (1995)/Heat (1995).mkv",
        "Ronin (1998)/Ronin (1998).mkv",
        "Private/.ignore",
        "Private/Home Video (2001)/Home Video (2001).mkv",
        "Drafts/Cut (2020)/Cut (2020).mkv",
    ] {
        touch(&movies, rel);
    }
    std::fs::write(movies.join(".ignore"), "# work in progress\ndrafts/\n").unwrap();
    let folders = vec![library(1, Some(CollectionTypeOptions::movies), &movies)];
    let scanner = scanner(tmp.path(), Arc::new(CountingFs::default())).await;
    let plan = scanner.plan_in(
        &folders,
        &folders,
        PlanScope::ALL,
        super::DateAdded::default(),
        None,
        None,
    );
    let planned: Vec<String> = plan
        .items
        .iter()
        .filter_map(|p| p.entity.path.clone())
        .collect();
    let root = movies.to_string_lossy();
    for kept in [
        "Heat (1995)/Heat (1995).mkv",
        "Ronin (1998)/Ronin (1998).mkv",
    ] {
        let path = format!("{root}/{kept}");
        assert!(planned.contains(&path), "{kept} not planned: {planned:?}");
    }
    for hidden in ["Private", "Drafts"] {
        let path = format!("{root}/{hidden}");
        assert!(
            planned.iter().all(|p| !p.starts_with(&path)),
            "{hidden} planned: {planned:?}"
        );
        assert!(
            plan.excluded.contains(&path),
            "{hidden} not excluded: {:?}",
            plan.excluded
        );
    }
}
