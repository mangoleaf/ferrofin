//! Folder content-type overrides (`ServerConfiguration.ContentTypes`, the
//! metadata editor's "Content type") through a scan, as v12.2 resolves them:
//! `ResolvePath` gives an untyped folder the override set for it or a folder
//! above it, `Folder.GetNonCachedChildren` resolves a folder's children with
//! `GetContentType(folder)`, and `SeriesResolver` makes a folder of a shows
//! context a series unless its own override is `tvshows`.

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

/// Every scanned video, series, season and plain folder as (kind, name, the
/// name of what it hangs off), sorted.
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
               'MediaBrowser.Controller.Entities.Folder')"#,
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
/// without a type.
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
    assert!(tree.contains(&row("Series", "Show", "Library")), "{tree:?}");
    assert!(
        !tree
            .iter()
            .any(|(kind, name, _)| kind == "Series" && name == "Heat"),
        "{tree:?}"
    );
}

/// "Content type: Shows" on a folder of a mixed library makes the folder
/// itself a plain folder (its own override is `tvshows`) and each of its
/// subfolders a series, which hangs off it.
#[tokio::test]
async fn a_mixed_folder_set_to_shows_holds_series() {
    let files = [
        "Anime/Show A/Show A S01E01.mkv",
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
