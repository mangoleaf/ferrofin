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
    for rel in [
        "Alien (1979).mkv",
        "Alien (1979)-trailer.mkv",
        "Heat (1995)/Heat (1995).mkv",
        "Heat (1995)/Heat (1995)-trailer.mkv",
        "Ronin (1998)/Ronin (1998) - 1080p.mkv",
        "Ronin (1998)/Ronin (1998) - 2160p.mkv",
        "Ronin (1998)/Ronin (1998) - 1080p-trailer.mkv",
        "Heat (1995)/trailers/Teaser.mkv",
        "Heat (1995)/extras/Behind the Scenes.mkv",
        "Heat (1995)/theme.mp3",
        "Blade Runner (1982)/VIDEO_TS/VTS_01_1.VOB",
        "Blade Runner (1982)/VIDEO_TS/VIDEO_TS.IFO",
        "Dune (2021)/BDMV/index.bdmv",
        "Dune (2021)/BDMV/STREAM/00001.m2ts",
        "Dune (2021)/trailers/Dune Trailer.mkv",
        "Collections/Alien/Aliens (1986).mkv",
        "Collections/Alien/Alien 3 (1992).mkv",
        "Soundtrack/score.mp3",
    ] {
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
    ] {
        touch(&books, rel);
    }
    let home = root.join("home");
    for rel in [
        "root.jpg",
        "clip.mp4",
        "clip.jpg",
        "Trip 2024/img1.jpg",
        "Trip 2024/day2/img2.jpg",
        "Trip 2024/day2/movie.mkv",
        "No Photos/Sub/x.png",
    ] {
        touch(&home, rel);
    }
    let mixed = root.join("mixed");
    for rel in ["Film.mkv", "Folder/Other Film (2001).mkv", "Folder/pic.jpg"] {
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
/// closure, and its row (presentation keys included) — with a folder's
/// `DateCreated` cleared, which the planner stamps with the clock.
fn canon(items: Vec<Planned>) -> Vec<(Uuid, Vec<Uuid>, BaseItemEntity)> {
    items
        .into_iter()
        .map(|p| {
            let mut entity = p.entity;
            if entity.is_folder {
                entity.date_created = None;
            }
            (p.id, p.ancestors, entity)
        })
        .collect()
}

/// The full plan filtered to `paths` (and `exact`), as a scoped scan used to
/// build it: the items at, under or above a path, and the path-less virtual
/// seasons those items sit in.
fn filtered(full: &[Planned], paths: &[String], exact: Option<&str>) -> Vec<Planned> {
    let by_path = |p: &Planned| {
        p.entity.path.as_deref().is_some_and(|path| {
            exact == Some(path)
                || paths
                    .iter()
                    .any(|c| path_is_under(path, c) || path_is_under(c, path))
        })
    };
    let parents: std::collections::HashSet<String> = full
        .iter()
        .filter(|p| by_path(p))
        .filter_map(|p| p.entity.parent_id.clone())
        .collect();
    full.iter()
        .filter(|p| by_path(p) || (p.entity.path.is_none() && parents.contains(&p.entity.id)))
        .map(|p| Planned {
            id: p.id,
            entity: p.entity.clone(),
            ancestors: p.ancestors.clone(),
        })
        .collect()
}

/// For every planned item of every fixture library — and for folders that
/// are no item, the library roots (with and without a trailing slash), the
/// inside of a disc rip, and paths that do not exist — planning that one
/// path, through the libraries a path-scoped scan walks for it
/// (`affected_libraries`), yields exactly the full plan filtered to it.
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
    ] {
        assert!(
            full.iter().any(|p| p.entity.type_.ends_with(kind)),
            "the fixture plans no {kind}"
        );
    }
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
        "mixed/Folder",
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
    assert_eq!(
        episode.entity.parent_id.as_deref(),
        Some(season.entity.id.as_str())
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
