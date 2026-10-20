//! An episode's range end (`IndexNumberEnd`, kept in the `Data` blob) through
//! a scan, as v12.2 sets it (PR 17805): from a multi-episode NFO, whose
//! `<episodedetails>` blocks are sorted by episode number first, and never
//! ending before the episode starts (`EpisodeMetadataService.BeforeSaveInternal`).

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// A one-episode show library holding `file`, with `nfo` beside it when
/// given, and a scanner over it.
async fn episode_library(root: &Path, file: &str, nfo: Option<&str>) -> (LibraryScanner, Database) {
    let media = root.join("tv");
    let season = media.join("Show").join("Season 1");
    std::fs::create_dir_all(&season).expect("fixture dirs");
    std::fs::write(season.join(file), b"").expect("media file");
    if let Some(nfo) = nfo {
        let stem = Path::new(file).file_stem().expect("stem");
        std::fs::write(season.join(stem).with_extension("nfo"), nfo).expect("nfo");
    }

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Shows",
        Some(CollectionTypeOptions::tvshows),
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
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items);
    (scanner, db)
}

/// The scanned episode's name, `IndexNumber` and `Data.IndexNumberEnd`.
async fn episode(db: &Database) -> (String, Option<i64>, Option<i64>) {
    let (name, index, data): (String, Option<i64>, Option<String>) = sqlx::query_as(
        r#"SELECT "Name", "IndexNumber", "Data" FROM "BaseItems" WHERE "Type" LIKE '%.Episode'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("the episode was stored");
    let end = data
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|d| d.get("IndexNumberEnd").and_then(serde_json::Value::as_i64));
    (name, index, end)
}

/// Sonarr writes a multi-episode NFO highest episode first; the episode is
/// still the range 1..2, its title and synopsis in ascending order.
#[tokio::test]
async fn a_reversed_multi_episode_nfo_is_stored_in_ascending_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nfo = "<episodedetails><title>Rising (2)</title><season>1</season><episode>2</episode></episodedetails>\n\
               <episodedetails><title>Rising (1)</title><season>1</season><episode>1</episode></episodedetails>";
    let (scanner, db) = episode_library(tmp.path(), "Show S01E01.mkv", Some(nfo)).await;
    scanner.scan_all().await.expect("scan");

    assert_eq!(
        episode(&db).await,
        ("Rising (1) / Rising (2)".to_owned(), Some(1), Some(2))
    );
}

/// An NFO whose range ends before the episode starts loses the end on save
/// (the scan's BeforeSave step); the episode number stays.
#[tokio::test]
async fn an_nfo_range_ending_before_the_episode_is_discarded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let nfo = "<episodedetails><title>Late</title><season>1</season>\
               <episode>2</episode><episodenumberend>1</episodenumberend></episodedetails>";
    let (scanner, db) = episode_library(tmp.path(), "Show S01E02.mkv", Some(nfo)).await;
    scanner.scan_all().await.expect("scan");

    assert_eq!(episode(&db).await, ("Late".to_owned(), Some(2), None));
}

/// A stored range ending before the episode starts (an adopted row: the
/// guard only arrived in 12.2) is cleared by the next scan, which refreshes
/// the unchanged episode as upstream's does and saves the cleared value.
#[tokio::test]
async fn a_rescan_clears_a_stored_range_ending_before_the_episode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = episode_library(tmp.path(), "Show S01E02.mkv", None).await;
    scanner.scan_all().await.expect("first scan");
    sqlx::query(
        r#"UPDATE "BaseItems" SET "Data" = json_set(COALESCE("Data", '{}'), '$.IndexNumberEnd', 1)
           WHERE "Type" LIKE '%.Episode'"#,
    )
    .execute(db.writer())
    .await
    .expect("plant an inverted range");
    assert_eq!(episode(&db).await.2, Some(1));

    scanner.scan_all().await.expect("rescan");

    assert_eq!(
        episode(&db).await,
        ("Show S01E02".to_owned(), Some(2), None)
    );
}
