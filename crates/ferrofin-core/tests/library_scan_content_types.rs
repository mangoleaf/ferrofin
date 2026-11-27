//! Folder content-type overrides (`ServerConfiguration.ContentTypes`, the
//! metadata editor's "Content type") through a scan, as v12.2 resolves them:
//! `ResolvePath` gives an untyped folder the override set for it or a folder
//! above it, `Folder.GetNonCachedChildren` resolves a folder's children with
//! `GetContentType(folder)`, and `SeriesResolver` makes a folder of a shows
//! context a series unless its own override is `tvshows`. `homevideos` and
//! `photos` resolve videos as `Video`s and photos (`PhotoResolver`,
//! `PhotoAlbumResolver`), as Jellyfin 12.4 does live.

use std::path::Path;
use std::sync::{Arc, Mutex};

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::NameValuePair;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// The overrides a scan reads, which a test may change between scans.
type Overrides = Arc<Mutex<Vec<NameValuePair>>>;

/// A library of `collection_type` at `{root}/media` holding `files` (paths
/// below it, a trailing `/` making a directory), and a scanner over it that
/// reads `overrides`, given relative to the library.
async fn library(
    root: &Path,
    collection_type: Option<CollectionTypeOptions>,
    files: &[&str],
    overrides: &[(&str, &str)],
) -> (LibraryScanner, Database, Overrides) {
    library_with_photos(root, collection_type, true, files, overrides).await
}

/// [`library`], with the library's `EnablePhotos` set to `enable_photos`.
async fn library_with_photos(
    root: &Path,
    collection_type: Option<CollectionTypeOptions>,
    enable_photos: bool,
    files: &[&str],
    overrides: &[(&str, &str)],
) -> (LibraryScanner, Database, Overrides) {
    let media = root.join("media");
    std::fs::create_dir_all(&media).expect("library dir");
    for file in files {
        let path = media.join(file);
        if file.ends_with('/') {
            std::fs::create_dir_all(&path).expect("fixture dir");
        } else {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dir");
            std::fs::write(&path, b"").expect("fixture file");
        }
    }
    let overrides: Overrides = Arc::new(Mutex::new(entries(&media, overrides)));

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Library",
        collection_type,
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            enable_photos,
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let read = Arc::clone(&overrides);
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items)
    .with_content_types(move || read.lock().expect("overrides").clone());
    (scanner, db, overrides)
}

/// `overrides` as `ContentTypes` entries, their folders below `media`.
fn entries(media: &Path, overrides: &[(&str, &str)]) -> Vec<NameValuePair> {
    overrides
        .iter()
        .map(|(folder, content_type)| NameValuePair {
            name: Some(media.join(folder).to_string_lossy().into_owned()),
            value: Some((*content_type).to_owned()),
        })
        .collect()
}

/// Every scanned video, series, season, plain folder, photo, album, audio
/// track and playlist as (kind, name, the name of what it hangs off), sorted.
async fn tree(db: &Database) -> Vec<(String, String, String)> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT b."Type", b."Name", p."Name" FROM "BaseItems" b
           LEFT JOIN "BaseItems" p ON p."Id" = b."ParentId"
           WHERE b."Type" IN (
               'MediaBrowser.Controller.Entities.Movies.Movie',
               'MediaBrowser.Controller.Entities.Video',
               'MediaBrowser.Controller.Entities.MusicVideo',
               'MediaBrowser.Controller.Entities.TV.Series',
               'MediaBrowser.Controller.Entities.TV.Season',
               'MediaBrowser.Controller.Entities.TV.Episode',
               'MediaBrowser.Controller.Entities.Folder',
               'MediaBrowser.Controller.Entities.Photo',
               'MediaBrowser.Controller.Entities.PhotoAlbum',
               'MediaBrowser.Controller.Entities.Audio.Audio',
               'MediaBrowser.Controller.Playlists.Playlist')"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("items");
    let mut rows: Vec<_> = rows
        .into_iter()
        .map(|(type_, name, parent)| {
            let kind = type_.rsplit('.').next().unwrap_or_default().to_owned();
            (kind, name, parent.unwrap_or_default())
        })
        .collect();
    rows.sort();
    rows
}

fn row(kind: &str, name: &str, parent: &str) -> (String, String, String) {
    (kind.to_owned(), name.to_owned(), parent.to_owned())
}

/// #15797's case: in a mixed library a movie folder holding a stray
/// `tvshow.nfo` resolves as a series, and "Content type: Movies" on the
/// folder makes it the movie again.
#[tokio::test]
async fn a_mixed_folder_set_to_movies_scans_as_a_movie() {
    let files = ["Blade (1998)/Blade (1998).mkv", "Blade (1998)/tvshow.nfo"];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[]).await;
    scanner.scan_all().await.expect("scan");
    assert!(
        tree(&db)
            .await
            .iter()
            .any(|(kind, name, _)| kind == "Series" && name == "Blade"),
        "without an override the folder is a series: {:?}",
        tree(&db).await
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[("Blade (1998)", "movies")]).await;
    scanner.scan_all().await.expect("scan");
    assert_eq!(
        tree(&db).await,
        vec![row("Movie", "Blade (1998)", "Library")]
    );
}

/// An override applies below its folder too (`ResolvePath` passes
/// `inherit: true`), and a mixed library's other folders keep resolving
/// without a type: a series there hangs off its plain folder.
#[tokio::test]
async fn an_override_applies_to_the_folders_below_it() {
    let files = [
        "Films/Heat (1995)/Heat (1995).mkv",
        "Films/Heat (1995)/tvshow.nfo",
        "Other/Show/tvshow.nfo",
        "Other/Show/Show S01E01.mkv",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[("Films", "movies")]).await;
    scanner.scan_all().await.expect("scan");
    let tree = tree(&db).await;
    assert!(
        tree.contains(&row("Movie", "Heat (1995)", "Films")),
        "{tree:?}"
    );
    assert!(
        tree.contains(&row("Folder", "Other", "Library")),
        "{tree:?}"
    );
    assert!(tree.contains(&row("Series", "Show", "Other")), "{tree:?}");
    assert!(
        !tree
            .iter()
            .any(|(kind, name, _)| kind == "Series" && name == "Heat"),
        "{tree:?}"
    );
}

/// "Content type: Shows" on a folder of a mixed library makes the folder
/// itself a plain folder (its own override is `tvshows`) and each of its
/// subfolders a series, which hangs off it. A series' extra keeps an extra's
/// shape: owned, with no parent and so no folder above it.
#[tokio::test]
async fn a_mixed_folder_set_to_shows_holds_series() {
    let files = [
        "Anime/Show A/Show A S01E01.mkv",
        "Anime/Show A/trailers/Teaser.mkv",
        "Anime/Show B/Season 1/Show B S01E01.mkv",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[("Anime", "tvshows")]).await;
    scanner.scan_all().await.expect("scan");
    let tree = tree(&db).await;
    assert!(
        tree.contains(&row("Folder", "Anime", "Library")),
        "{tree:?}"
    );
    assert!(tree.contains(&row("Series", "Show A", "Anime")), "{tree:?}");
    assert!(tree.contains(&row("Series", "Show B", "Anime")), "{tree:?}");
    assert!(
        tree.iter().filter(|(kind, _, _)| kind == "Episode").count() == 2,
        "{tree:?}"
    );
    assert!(!tree.iter().any(|(kind, _, _)| kind == "Movie"), "{tree:?}");

    let owner: Option<String> = sqlx::query_scalar(
        r#"SELECT o."Name" FROM "BaseItems" t JOIN "BaseItems" o ON o."Id" = t."OwnerId"
           WHERE t."Type" = 'MediaBrowser.Controller.Entities.Trailer'"#,
    )
    .fetch_optional(db.pool())
    .await
    .expect("trailer");
    assert_eq!(owner.as_deref(), Some("Show A"));
    let folders_above: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "AncestorIds" a
           JOIN "BaseItems" t ON t."Id" = a."ItemId"
           JOIN "BaseItems" f ON f."Id" = a."ParentItemId"
           WHERE t."Type" = 'MediaBrowser.Controller.Entities.Trailer'
             AND f."Type" = 'MediaBrowser.Controller.Entities.Folder'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("ancestors");
    assert_eq!(folders_above, 0, "the trailer is below no plain folder");
}

/// A tvshows library's `SeriesResolver` rule: every folder is a series,
/// except one whose own override is `tvshows`, which is a folder of shows.
#[tokio::test]
async fn a_tv_folder_set_to_shows_is_a_folder_of_series() {
    let files = [
        "Anime/Show B/Season 1/Show B S01E01.mkv",
        "Plain/Season 1/Plain S01E01.mkv",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(
        tmp.path(),
        Some(CollectionTypeOptions::tvshows),
        &files,
        &[("Anime", "tvshows")],
    )
    .await;
    scanner.scan_all().await.expect("scan");
    let tree = tree(&db).await;
    assert!(
        tree.contains(&row("Folder", "Anime", "Library")),
        "{tree:?}"
    );
    assert!(tree.contains(&row("Series", "Show B", "Anime")), "{tree:?}");
    assert!(
        tree.contains(&row("Series", "Plain", "Library")),
        "{tree:?}"
    );
    assert!(
        !tree
            .iter()
            .any(|(kind, name, _)| kind == "Series" && name == "Anime")
    );
}

/// In a typed library a folder's override is what its children resolve with
/// (`GetContentType(folder)`): a movies library's folder set to Shows holds
/// series, and the series' episodes sit below the folder as well.
#[tokio::test]
async fn a_movie_library_folder_set_to_shows_holds_series() {
    let files = [
        "Shows/Show C/Season 1/Show C S01E01.mkv",
        "Heat (1995)/Heat (1995).mkv",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(
        tmp.path(),
        Some(CollectionTypeOptions::movies),
        &files,
        &[("Shows", "tvshows")],
    )
    .await;
    scanner.scan_all().await.expect("scan");
    let tree = tree(&db).await;
    assert!(
        tree.contains(&row("Folder", "Shows", "Library")),
        "{tree:?}"
    );
    assert!(tree.contains(&row("Series", "Show C", "Shows")), "{tree:?}");
    assert!(
        tree.contains(&row("Movie", "Heat (1995)", "Library")),
        "{tree:?}"
    );

    let folder: String = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems" WHERE "Name" = 'Shows'
           AND "Type" = 'MediaBrowser.Controller.Entities.Folder'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("folder");
    let below: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "AncestorIds" a JOIN "BaseItems" b ON b."Id" = a."ItemId"
           WHERE a."ParentItemId" = ?1 AND b."Type" IN (
               'MediaBrowser.Controller.Entities.TV.Series',
               'MediaBrowser.Controller.Entities.TV.Season',
               'MediaBrowser.Controller.Entities.TV.Episode')"#,
    )
    .bind(&folder)
    .fetch_one(db.pool())
    .await
    .expect("ancestors");
    assert_eq!(below, 3, "the series, its season and its episode");
}

/// An override set after a scan applies from the next one, as upstream's
/// (the editor's change takes effect on the folder's next refresh): the
/// series it replaces goes.
#[tokio::test]
async fn setting_an_override_retypes_the_folder_on_the_next_scan() {
    let files = ["Blade (1998)/Blade (1998).mkv", "Blade (1998)/tvshow.nfo"];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, overrides) = library(tmp.path(), None, &files, &[]).await;
    scanner.scan_all().await.expect("first scan");
    *overrides.lock().expect("overrides") =
        entries(&tmp.path().join("media"), &[("Blade (1998)", "movies")]);
    scanner.scan_all().await.expect("second scan");
    assert_eq!(
        tree(&db).await,
        vec![row("Movie", "Blade (1998)", "Library")]
    );
}

/// An override no walk follows yet (`music` in a mixed library) leaves the
/// folder resolving as before.
#[tokio::test]
async fn an_override_to_another_type_leaves_the_folder_as_it_was() {
    let files = ["Blade (1998)/Blade (1998).mkv", "Blade (1998)/tvshow.nfo"];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[("Blade (1998)", "music")]).await;
    scanner.scan_all().await.expect("scan");
    assert!(
        tree(&db)
            .await
            .iter()
            .any(|(kind, name, _)| kind == "Series" && name == "Blade"),
        "{:?}",
        tree(&db).await
    );
}

/// `musicvideos` is a video type the movie walk resolves itself: a folder set
/// to it scans its video as a music video, where a mixed library made a
/// movie of it.
#[tokio::test]
async fn a_mixed_folder_set_to_music_videos_holds_music_videos() {
    let files = ["Clips/Artist - Song.mkv"];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[]).await;
    scanner.scan_all().await.expect("scan");
    let before = tree(&db).await;
    assert!(
        before.iter().any(|(kind, _, _)| kind == "Movie"),
        "{before:?}"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &files, &[("Clips", "musicvideos")]).await;
    scanner.scan_all().await.expect("scan");
    let after = tree(&db).await;
    assert!(
        after.iter().any(|(kind, _, _)| kind == "MusicVideo"),
        "{after:?}"
    );
    assert!(
        !after.iter().any(|(kind, _, _)| kind == "Movie"),
        "{after:?}"
    );
}

/// A path-scoped scan (the library monitor, a folder refresh) resolves the
/// folder it covers by the overrides too, and prunes what the folder was.
#[tokio::test]
async fn a_scoped_scan_of_the_folder_follows_its_override() {
    let files = ["Blade (1998)/Blade (1998).mkv", "Blade (1998)/tvshow.nfo"];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, overrides) = library(tmp.path(), None, &files, &[]).await;
    scanner.scan_all().await.expect("first scan");
    let media = tmp.path().join("media");
    *overrides.lock().expect("overrides") = entries(&media, &[("Blade (1998)", "movies")]);
    scanner
        .scan_paths(&[media.join("Blade (1998)").to_string_lossy().into_owned()])
        .await
        .expect("scoped scan");
    assert_eq!(
        tree(&db).await,
        vec![row("Movie", "Blade (1998)", "Library")]
    );
}

/// The tree a mixed library's folder holds, and what Jellyfin 12.4 makes of it
/// with "Content type: Home videos" on the folder: the folder resolves with
/// the override (`ResolvePath`), so holding a photo it is a `PhotoAlbum`; its
/// clips are `Video`s, never a movie in its own folder; a subfolder holding a
/// photo is an album; and the audio and the playlist file no longer resolve.
const HOME_FILES: [&str; 7] = [
    "Home/clip.mkv",
    "Home/pic.jpg",
    "Home/song.mp3",
    "Home/list.m3u",
    "Home/Album/a.jpg",
    "Home/Film (2000)/Film (2000).mkv",
    "Home/Disc/01 - One.mp3",
];

fn home_tree() -> Vec<(String, String, String)> {
    let mut rows = vec![
        row("PhotoAlbum", "Home", "Library"),
        row("Video", "clip", "Home"),
        row("Photo", "pic", "Home"),
        row("PhotoAlbum", "Album", "Home"),
        row("Photo", "a", "Album"),
        row("Folder", "Film (2000)", "Home"),
        row("Video", "Film (2000)", "Film (2000)"),
        row("Folder", "Disc", "Home"),
    ];
    rows.sort();
    rows
}

#[tokio::test]
async fn a_mixed_folder_set_to_home_videos_holds_photos_and_videos() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(tmp.path(), None, &HOME_FILES, &[("Home", "homevideos")]).await;
    scanner.scan_all().await.expect("scan");
    assert_eq!(tree(&db).await, home_tree());
}

/// Setting the override after a scan, then removing it, rescans the folder
/// each time to what a first scan makes of it: the movies, audio and playlist
/// it held go, then the photos and albums.
#[tokio::test]
async fn setting_and_removing_a_home_video_override_rescans_the_folder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, overrides) = library(tmp.path(), None, &HOME_FILES, &[]).await;
    scanner.scan_all().await.expect("first scan");
    let untyped = tree(&db).await;
    assert!(
        untyped.contains(&row("Movie", "clip", "Home"))
            && untyped.contains(&row("Audio", "song", "Home"))
            && untyped.contains(&row("Playlist", "list", "Home")),
        "{untyped:?}"
    );
    let media = tmp.path().join("media");
    *overrides.lock().expect("overrides") = entries(&media, &[("Home", "homevideos")]);
    scanner.scan_all().await.expect("second scan");
    assert_eq!(tree(&db).await, home_tree());
    overrides.lock().expect("overrides").clear();
    scanner.scan_all().await.expect("third scan");
    assert_eq!(tree(&db).await, untyped);
}

/// `photos` takes photos whatever the library's `EnablePhotos` says
/// (`PhotoResolver.cs:64-65`), where `homevideos` follows it.
#[tokio::test]
async fn a_folder_set_to_photos_holds_photos_without_enable_photos() {
    let files = [
        "Pics/pic.jpg",
        "Pics/clip.mkv",
        "Pics/Album/a.jpg",
        "Clips/pic.jpg",
        "Clips/clip.mkv",
        "Clips/Album/a.jpg",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library_with_photos(
        tmp.path(),
        Some(CollectionTypeOptions::homevideos),
        false,
        &files,
        &[("Pics", "photos"), ("Clips", "homevideos")],
    )
    .await;
    scanner.scan_all().await.expect("scan");
    let mut expected = vec![
        row("Folder", "Pics", "Library"),
        row("Photo", "pic", "Pics"),
        row("Video", "clip", "Pics"),
        row("PhotoAlbum", "Album", "Pics"),
        row("Photo", "a", "Album"),
        row("Folder", "Clips", "Library"),
        row("Video", "clip", "Clips"),
        row("Folder", "Album", "Clips"),
    ];
    expected.sort();
    assert_eq!(tree(&db).await, expected);
}

/// `MovieResolver` makes no video of a `photos` folder (`MovieResolver.cs:
/// 96-140`): a disc rip there is a plain folder whose files are videos, where
/// under `homevideos` it is one `Video`.
#[tokio::test]
async fn a_disc_rip_in_a_photos_folder_is_a_folder_of_videos() {
    let files = [
        "Pics/Rip/VIDEO_TS/VIDEO_TS.IFO",
        "Pics/Rip/VIDEO_TS/VTS_01_1.VOB",
        "Clips/Rip/VIDEO_TS/VIDEO_TS.IFO",
        "Clips/Rip/VIDEO_TS/VTS_01_1.VOB",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(
        tmp.path(),
        None,
        &files,
        &[("Pics", "photos"), ("Clips", "homevideos")],
    )
    .await;
    scanner.scan_all().await.expect("scan");
    let mut expected = vec![
        row("Folder", "Pics", "Library"),
        row("Folder", "Rip", "Pics"),
        row("Folder", "VIDEO_TS", "Rip"),
        row("Video", "VIDEO_TS", "VIDEO_TS"),
        row("Video", "VTS_01_1", "VIDEO_TS"),
        row("Folder", "Clips", "Library"),
        row("Video", "Rip", "Clips"),
    ];
    expected.sort();
    assert_eq!(tree(&db).await, expected);
}

/// In a home-video library a folder set to Movies still resolves with its
/// parent's type, so holding a photo it stays a `PhotoAlbum`, while what it
/// holds resolves as in a movies library: movies, no photo, no album.
#[tokio::test]
async fn a_home_video_folder_set_to_movies_holds_movies() {
    let files = [
        "Films/pic.jpg",
        "Films/clip.mkv",
        "Films/Album/a.jpg",
        "Films/Film (2000)/Film (2000).mkv",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, overrides) = library(
        tmp.path(),
        Some(CollectionTypeOptions::homevideos),
        &files,
        &[],
    )
    .await;
    scanner.scan_all().await.expect("first scan");
    *overrides.lock().expect("overrides") =
        entries(&tmp.path().join("media"), &[("Films", "movies")]);
    scanner.scan_all().await.expect("second scan");
    let mut expected = vec![
        row("PhotoAlbum", "Films", "Library"),
        row("Movie", "clip", "Films"),
        row("Folder", "Album", "Films"),
        row("Movie", "Film (2000)", "Films"),
    ];
    expected.sort();
    assert_eq!(tree(&db).await, expected);
}

/// An album's children resolve with its `GetContentType` too, and an entry
/// for a folder above matches before a nearer one listed after it
/// (`GetContentTypeOverride` takes the first): `Sub`'s movies are movies, but
/// `Sub/Pics` takes `Home`'s `homevideos`, listed first, and its photo.
#[tokio::test]
async fn an_album_set_to_movies_holds_movies() {
    let files = [
        "Home/pic.jpg",
        "Home/Sub/pic.jpg",
        "Home/Sub/Movie (2001)/Movie (2001).mkv",
        "Home/Sub/Pics/x.jpg",
    ];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(
        tmp.path(),
        None,
        &files,
        &[("Home", "homevideos"), ("Home/Sub", "movies")],
    )
    .await;
    scanner.scan_all().await.expect("scan");
    let mut expected = vec![
        row("PhotoAlbum", "Home", "Library"),
        row("Photo", "pic", "Home"),
        row("PhotoAlbum", "Sub", "Home"),
        row("Movie", "Movie (2001)", "Sub"),
        row("Folder", "Pics", "Sub"),
        row("Photo", "x", "Pics"),
    ];
    expected.sort();
    assert_eq!(tree(&db).await, expected);
}

/// In a movies or a music-video library the folder itself resolves with the
/// library's type (a plain folder here), and what it holds as the override
/// says: videos of the walk's kind become `Video`s, never a movie in its own
/// folder, and its photos resolve.
#[tokio::test]
async fn a_video_library_folder_set_to_home_videos_or_photos_holds_photos() {
    let files = [
        "Pics/clip.mkv",
        "Pics/pic.jpg",
        "Pics/Album/a.jpg",
        "Pics/Film (2000)/Film (2000).mkv",
    ];
    for (library_type, content_type) in [
        (CollectionTypeOptions::movies, "photos"),
        (CollectionTypeOptions::musicvideos, "homevideos"),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (scanner, db, _) = library(
            tmp.path(),
            Some(library_type),
            &files,
            &[("Pics", content_type)],
        )
        .await;
        scanner.scan_all().await.expect("scan");
        let mut expected = vec![
            row("Folder", "Pics", "Library"),
            row("Video", "clip", "Pics"),
            row("Photo", "pic", "Pics"),
            row("PhotoAlbum", "Album", "Pics"),
            row("Photo", "a", "Album"),
            row("Folder", "Film (2000)", "Pics"),
            row("Video", "Film (2000)", "Film (2000)"),
        ];
        expected.sort();
        assert_eq!(
            tree(&db).await,
            expected,
            "{library_type:?} → {content_type}"
        );
    }
}

/// An album set to Shows holds series (`SeriesResolver`, the album's
/// `GetContentType`), which hang off the album; its photo no longer
/// resolves.
#[tokio::test]
async fn an_album_set_to_shows_holds_series() {
    let files = ["Shows/pic.jpg", "Shows/Show A/Season 1/Show A S01E01.mkv"];
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db, _) = library(
        tmp.path(),
        Some(CollectionTypeOptions::homevideos),
        &files,
        &[("Shows", "tvshows")],
    )
    .await;
    scanner.scan_all().await.expect("scan");
    let tree = tree(&db).await;
    assert!(
        tree.contains(&row("PhotoAlbum", "Shows", "Library")),
        "{tree:?}"
    );
    assert!(tree.contains(&row("Series", "Show A", "Shows")), "{tree:?}");
    assert!(
        tree.iter()
            .any(|(kind, _, parent)| kind == "Episode" && parent == "Season 1"),
        "{tree:?}"
    );
    assert!(!tree.iter().any(|(kind, _, _)| kind == "Photo"), "{tree:?}");
}
