//! The scan's per-item accounting — `ScanOutcome` and the `ItemsAdded` of the
//! scan-end `LibraryChanged` push — in the wirings where the stored-row read
//! cannot simply answer "row or no row":
//!
//! - a row that cannot be READ: it fails its window's batch read, the window
//!   is re-read per item, and only that row is unknown (saved and counted as
//!   updated, never announced as added);
//! - a scanner with NO item repository: it falls back to asking the item store
//!   per item, so a rescan announces nothing;
//! - an id the plan holds TWICE (overlapping library locations): created and
//!   announced once, the repeat is an update.

use std::sync::{Arc, Mutex};

use ferrofin_core::event_manager::consumer_done;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinEventManager, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinVirtualFolderManager, LibraryScanner, ScanOutcome,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// A movie library over `locations`, plus the database and the item store.
async fn movie_library(
    tmp: &std::path::Path,
    locations: &[&std::path::Path],
) -> (
    Database,
    Arc<FerrofinItemPersistenceService>,
    Arc<dyn VirtualFolderManager>,
) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("default")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Movies",
        Some(CollectionTypeOptions::movies),
        &LibraryOptions {
            path_infos: locations
                .iter()
                .map(|p| MediaPathInfo {
                    path: p.to_string_lossy().into_owned(),
                })
                .collect(),
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    (db, persistence, vf)
}

/// An event manager recording every `LibraryChanged` payload.
fn recording_events() -> (
    Arc<FerrofinEventManager>,
    Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let events = FerrofinEventManager::new();
    let changes: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&changes);
    events.subscribe(
        "LibraryChanged",
        Arc::new(move |payload: &str| {
            sink.lock()
                .expect("sink")
                .push(serde_json::from_str(payload).expect("json"));
            consumer_done()
        }),
    );
    (Arc::new(events), changes)
}

/// Every id announced as added across the recorded pushes.
fn added(changes: &Mutex<Vec<serde_json::Value>>) -> Vec<String> {
    changes
        .lock()
        .expect("changes")
        .iter()
        .flat_map(|c| c["ItemsAdded"].as_array().cloned().unwrap_or_default())
        .map(|v| v.as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Rows of the movie type.
async fn movie_rows(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#)
        .fetch_one(db.pool())
        .await
        .expect("count")
}

fn repository(db: &Database) -> Arc<dyn ItemRepository> {
    Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ))
}

// One row the decoder rejects fails its whole window's batch read. The window
// is then re-read row by row, so only that row is left unread; its healthy
// neighbours keep their stored state, and a new file in the same window is
// still created and announced. The unread row runs no provider and is not
// re-saved from the scan (nothing can be merged onto a row that cannot be
// read): only its file facts are written when they moved, so the user's
// overview on it survives.
#[tokio::test]
async fn an_unreadable_row_leaves_only_itself_unread() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    std::fs::create_dir_all(&media).expect("mkdir");
    std::fs::write(media.join("Alien (1979).mkv"), b"").expect("write");
    std::fs::write(media.join("Heat (1995).mkv"), b"").expect("write");
    let (db, persistence, vf) = movie_library(tmp.path(), &[&media]).await;
    let (events, changes) = recording_events();
    let items = repository(&db);
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::clone(&items))
        .with_events(events);

    let first = scanner.scan_all().await.expect("first scan");
    assert_eq!(first.created, 2);
    assert_eq!(added(&changes).len(), 2);

    // A stored value the row decoder rejects. The scan upsert keeps a stored
    // `DateCreated`, so the row stays unreadable scan after scan.
    sqlx::query(
        r#"UPDATE "BaseItems" SET "DateCreated" = 'not a date', "Overview" = 'My overview'
           WHERE "Type" LIKE '%Movies.Movie' AND "Name" = 'Alien'"#,
    )
    .execute(db.pool())
    .await
    .expect("corrupt a row");
    let movie_id = |file: &str| {
        derive_item_id(BaseItemKind::Movie, &media.join(file).to_string_lossy()).expect("movie id")
    };
    let (alien, heat, ran) = (
        movie_id("Alien (1979).mkv"),
        movie_id("Heat (1995).mkv"),
        movie_id("Ran (1985).mkv"),
    );
    // The setup has teeth: the batch read of the window fails outright, while
    // the healthy row still reads on its own.
    assert!(items.retrieve_items(&[alien, heat, ran]).await.is_err());
    assert!(items.retrieve_item(alien).await.is_err());
    assert!(items.retrieve_item(heat).await.expect("heat").is_some());

    std::fs::write(media.join("Ran (1985).mkv"), b"").expect("write");
    let rescan = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        rescan,
        ScanOutcome {
            created: 1,
            unchanged: 2,
            ..ScanOutcome::default()
        },
        "the new file is still created; the unreadable row's file facts did not \
         move, and the healthy row, re-read on its own, is found unchanged"
    );
    let overview: Option<String> = sqlx::query_scalar(
        r#"SELECT "Overview" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie' AND "Name" = 'Alien'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("overview");
    assert_eq!(
        overview.as_deref(),
        Some("My overview"),
        "the user's edit survives"
    );

    // A moved file fact (the file grew) is written, and only that.
    std::fs::write(media.join("Alien (1979).mkv"), b"now longer").expect("grow");
    let rescan = scanner.scan_all().await.expect("rescan");
    assert_eq!(rescan.updated, 1);
    let (size, overview): (Option<i64>, Option<String>) = sqlx::query_as(
        r#"SELECT "Size", "Overview" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie' AND "Name" = 'Alien'"#,
    )
    .fetch_one(db.pool())
    .await
    .expect("row");
    assert_eq!(size, Some(10));
    assert_eq!(overview.as_deref(), Some("My overview"));

    // A stale ancestor closure (here: lost) is rewritten from the plan.
    let ancestors = || async {
        sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*) FROM "AncestorIds" WHERE "ItemId" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(alien))
            .fetch_one(db.pool())
            .await
            .expect("ancestors")
    };
    let before = ancestors().await;
    assert!(before > 0);
    sqlx::query(r#"DELETE FROM "AncestorIds" WHERE "ItemId" = ?1"#)
        .bind(ferrofin_db::store::guid_to_db(alien))
        .execute(db.pool())
        .await
        .expect("drop closure");
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    assert_eq!(ancestors().await, before, "the closure is restored");
    assert_eq!(
        scanner.scan_all().await.expect("rescan").updated,
        0,
        "then quiet"
    );
    assert_eq!(movie_rows(&db).await, 3, "no row is lost");
    let announced: Vec<uuid::Uuid> = added(&changes)
        .iter()
        .map(|id| uuid::Uuid::parse_str(id).expect("announced id"))
        .collect();
    assert_eq!(announced.len(), 3, "exactly the new file is announced");
    assert_eq!(announced.last(), Some(&ran));
}

// Without an item repository the scan cannot read stored rows, so it asks the
// item store per item — exactly the check it used to make — and a rescan of an
// unchanged library announces nothing and creates nothing.
#[tokio::test]
async fn without_an_item_repository_a_rescan_announces_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    std::fs::create_dir_all(&media).expect("mkdir");
    std::fs::write(media.join("Alien (1979).mkv"), b"").expect("write");
    std::fs::write(media.join("Heat (1995).mkv"), b"").expect("write");
    let (_db, persistence, vf) = movie_library(tmp.path(), &[&media]).await;
    let (events, changes) = recording_events();
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_events(events);

    let first = scanner.scan_all().await.expect("first scan");
    assert_eq!(
        first,
        ScanOutcome {
            created: 2,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(added(&changes).len(), 2);

    let rescan = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        rescan,
        ScanOutcome {
            updated: 2,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(added(&changes).len(), 2, "a rescan announces no additions");
}

// The scan's changes also reach in-process consumers (the intro skipper's
// automatic analysis) as `ItemsChanged`, the same set the clients are told.
#[tokio::test]
async fn a_scan_announces_its_items_in_process_too() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    std::fs::create_dir_all(&media).expect("mkdir");
    std::fs::write(media.join("Alien (1979).mkv"), b"").expect("write");
    let (_db, persistence, vf) = movie_library(tmp.path(), &[&media]).await;
    let (events, changes) = recording_events();
    let in_process: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&in_process);
    events.subscribe(
        ferrofin_core::library_changed_notifier::ITEMS_CHANGED,
        Arc::new(move |payload: &str| {
            sink.lock()
                .expect("sink")
                .push(serde_json::from_str(payload).expect("json"));
            consumer_done()
        }),
    );
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_events(events);
    scanner.scan_all().await.expect("scan");
    assert_eq!(added(&changes).len(), 1);
    assert_eq!(added(&in_process), added(&changes));
}

// Overlapping library locations plan the same kind+path id twice, each copy
// with its own parent. Only the last copy is refreshed — the one whose row
// always won — so the file is created and announced once, and a rescan is
// quiet instead of saving each copy over the other every time.
#[tokio::test]
async fn a_repeated_planned_id_is_created_and_announced_once() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let nested = media.join("nested");
    std::fs::create_dir_all(&nested).expect("mkdir");
    std::fs::write(nested.join("Alien (1979).mkv"), b"").expect("write");
    let (db, persistence, vf) = movie_library(tmp.path(), &[&media, &nested]).await;
    let (events, changes) = recording_events();
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(repository(&db))
        .with_events(events);

    let first = scanner.scan_all().await.expect("first scan");
    assert_eq!(movie_rows(&db).await, 1, "one row for the one file");
    assert_eq!(
        first,
        ScanOutcome {
            created: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        },
        "the plan holds the file twice; the superseded copy writes nothing"
    );
    assert_eq!(added(&changes).len(), 1, "announced once");
    let parent = || async {
        sqlx::query_scalar::<_, Option<String>>(
            r#"SELECT "ParentId" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie'"#,
        )
        .fetch_one(db.pool())
        .await
        .expect("parent")
    };
    let first_parent = parent().await;

    for scan in ["second", "third"] {
        assert_eq!(
            scanner.scan_all().await.expect("rescan"),
            ScanOutcome {
                unchanged: 2,
                ..ScanOutcome::default()
            },
            "{scan} scan is quiet"
        );
        assert_eq!(parent().await, first_parent, "the ParentId is stable");
    }
    assert_eq!(added(&changes).len(), 1);
}
