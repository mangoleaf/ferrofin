//! What a library scan may and may not touch on a **metadata-locked** item.
//!
//! The walk's `locked` flag is the item's stored row's `IsLocked`; the
//! locked-item set the scan reads once (`ItemRepository::locked_item_ids`)
//! serves only the items whose row it did not read. These tests pin that the
//! flag still means `IsLocked`.
//!
//! The NFO is the assertion with teeth. The user-owned *metadata* columns are
//! protected a second time inside the scan upsert (`CASE WHEN "IsLocked" = 1
//! THEN "<col>" ELSE excluded."<col>" END`), so a test asserting on those would
//! still pass with the loop's flag stuck at `false`. The external ids an NFO
//! pins have no such SQL backstop — they reach `BaseItemProviders` only when
//! the reader runs, which upstream never does for a locked item
//! (`RefreshWithProviders` returns on `IsLocked` before its local providers).
//!
//! Artwork is the other way round: upstream validates a locked item's local
//! images like any other's (`CanRefreshImages` enables `ILocalImageProvider`
//! before its `IsLocked` check), so the scan rediscovers them.
//!
//! A lock, an unlock or an edit made while the scan runs
//! (`PriorityLane::edited`) is seen by the walk before it reaches the item:
//! upstream's metadata editor changes the cached item the refresh merges
//! onto.

use std::path::Path;
use std::sync::{Arc, Mutex};

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::library_scan::{LaneRefresh, PriorityLane, ScanCancel, ScanOutcome, ScanRun};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

/// Builds a one-movie library (media file + a poster beside it) and wires a
/// scanner over an in-memory database.
async fn one_movie_library(root: &Path) -> (LibraryScanner, Database) {
    let media = root.join("movies");
    let folder = media.join("Movie 0001 (2020)");
    std::fs::create_dir_all(&folder).expect("fixture dirs");
    std::fs::write(folder.join("Movie 0001 (2020).mkv"), b"").expect("media file");
    std::fs::write(folder.join("poster.jpg"), b"jpeg").expect("poster");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");

    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Movies",
        Some(CollectionTypeOptions::movies),
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

/// How many artwork rows the library currently holds.
async fn image_rows(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItemImageInfos""#)
        .fetch_one(db.pool())
        .await
        .expect("count images")
}

/// Sets `IsLocked` on the movie row, as the metadata editor's lock does.
async fn set_locked(db: &Database, locked: i64) {
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = ?1 WHERE "Type" LIKE '%Movies.Movie'"#)
        .bind(locked)
        .execute(db.writer())
        .await
        .expect("set lock");
}

#[tokio::test]
async fn a_locked_item_still_validates_its_local_artwork() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;

    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        image_rows(&db).await,
        1,
        "the first scan discovers the poster"
    );

    // The item is locked and its image rows are lost; the poster is still
    // on disk beside the movie.
    set_locked(&db, 1).await;
    sqlx::query(r#"DELETE FROM "BaseItemImageInfos""#)
        .execute(db.writer())
        .await
        .expect("clear images");

    scanner.scan_all().await.expect("rescan while locked");
    assert_eq!(
        image_rows(&db).await,
        1,
        "local image validation runs whatever the lock: the poster is rediscovered"
    );
}

/// The `Tmdb` id rows the movie carries.
async fn tmdb_ids(db: &Database) -> Vec<String> {
    sqlx::query_scalar(
        r#"SELECT "ProviderValue" FROM "BaseItemProviders" WHERE "ProviderId" = 'Tmdb'"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("provider ids")
}

/// Moves `path`'s mtime an hour ahead, past the NFO reader's one-minute
/// tolerance over `DateLastSaved`.
fn touch_ahead(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3_600))
        .expect("set mtime");
}

#[tokio::test]
async fn a_locked_item_never_reads_its_nfo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    scanner.scan_all().await.expect("first scan");

    set_locked(&db, 1).await;
    let nfo = tmp
        .path()
        .join("movies")
        .join("Movie 0001 (2020)")
        .join("Movie 0001 (2020).nfo");
    std::fs::write(&nfo, "<movie><tmdbid>603</tmdbid></movie>").expect("nfo");
    touch_ahead(&nfo);
    scanner.scan_all().await.expect("rescan while locked");
    assert!(
        tmdb_ids(&db).await.is_empty(),
        "the NFO's pinned id never reaches a locked item"
    );

    // Control: unlocked, the same (changed) NFO is read.
    set_locked(&db, 0).await;
    scanner.scan_all().await.expect("rescan while unlocked");
    assert_eq!(tmdb_ids(&db).await, ["603"]);
}

#[tokio::test]
async fn the_scan_reads_the_locked_set_from_the_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    scanner.scan_all().await.expect("first scan");

    let items = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));
    assert!(
        items
            .locked_item_ids()
            .await
            .expect("locked ids")
            .is_empty(),
        "a freshly scanned library locks nothing"
    );

    set_locked(&db, 1).await;
    assert_eq!(
        items.locked_item_ids().await.expect("locked ids").len(),
        1,
        "the locked movie is the one row the scan's per-scan read returns"
    );
}

/// Adds a second movie to [`one_movie_library`]'s library.
fn add_second_movie(root: &Path) {
    let folder = root.join("movies").join("Movie 0002 (2020)");
    std::fs::create_dir_all(&folder).expect("fixture dirs");
    std::fs::write(folder.join("Movie 0002 (2020).mkv"), b"").expect("media file");
}

/// A priority lane with no refresh to serve. At its first call once the
/// walk is past its first item, and so past the read of the stored-row window
/// holding the movies, it runs `edit` on the movie rows as the metadata
/// editor would, and reports the movies it edited as edited outside the scan.
struct EditingLane {
    db: Database,
    /// The edit: an `UPDATE` of the movie rows `RETURNING "Id"`.
    edit: &'static str,
    /// The walk's last reported percent: below 96 while it walks.
    percent: Arc<Mutex<Option<f64>>>,
    /// The edited movies, once the edit landed.
    edited: Mutex<Option<Vec<Uuid>>>,
    /// The edited movies the scan has not taken yet.
    unread: Mutex<Vec<Uuid>>,
}

impl PriorityLane for EditingLane {
    fn next(&self) -> Option<LaneRefresh> {
        // The walk reports `scanned * 96 / total` before each item, and
        // nothing reports a percent in (0, 96) before it: the first one is
        // at its second item, after its first window read. Were the
        // planning to report one, the edit would land before that read and
        // the tests would pass without the re-read.
        let walking = self
            .percent
            .lock()
            .expect("percent")
            .is_some_and(|p| p > 0.0 && p < 96.0);
        let mut edited = self.edited.lock().expect("edited");
        if walking && edited.is_none() {
            let ids: Vec<String> = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(sqlx::query_scalar(self.edit).fetch_all(self.db.writer()))
            })
            .expect("edit");
            let ids: Vec<Uuid> = ids.iter().map(|id| id.parse().expect("id")).collect();
            self.unread.lock().expect("unread").extend(&ids);
            *edited = Some(ids);
        }
        None
    }

    fn done(&self, _refresh: LaneRefresh, _outcome: &Result<ScanOutcome, ServiceError>) {}

    fn edited(&self) -> Vec<Uuid> {
        std::mem::take(&mut *self.unread.lock().expect("unread"))
    }
}

/// Rescans every library with `options` while `edit` of the movies lands
/// mid-walk, after their stored rows were read.
async fn rescan_with_edit_mid_walk(
    scanner: &LibraryScanner,
    db: &Database,
    options: &MetadataRefreshOptions,
    edit: &'static str,
) {
    let percent = Arc::new(Mutex::new(None));
    let report = {
        let percent = Arc::clone(&percent);
        move |p: f64| *percent.lock().expect("percent") = Some(p)
    };
    let lane = EditingLane {
        db: db.clone(),
        edit,
        percent,
        edited: Mutex::new(None),
        unread: Mutex::new(Vec::new()),
    };
    let cancel = ScanCancel::new();
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(options, options, &cancel)
                .with_progress(&report)
                .with_lane(&lane),
        )
        .await
        .expect("rescan");
    let edited = lane.edited.lock().expect("edited").clone();
    assert_eq!(
        edited.map(|ids| ids.len()),
        Some(2),
        "the edit landed while the walk was between items"
    );
}

/// An edit made over the API while a saving scan runs, to an item the scan
/// has read but not reached yet, survives the scan's save of that item: the
/// walk reads the item again before it merges onto it.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_made_mid_scan_survives_the_scan_saving_the_item() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    add_second_movie(tmp.path());
    scanner.scan_all().await.expect("first scan");

    // A full refresh saves every item.
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    rescan_with_edit_mid_walk(
        &scanner,
        &db,
        &options,
        r#"UPDATE "BaseItems" SET "Overview" = 'Typed mid-scan'
           WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
    )
    .await;
    let overviews: Vec<Option<String>> = sqlx::query_scalar(
        r#"SELECT "Overview" FROM "BaseItems" WHERE "Type" LIKE '%Movies.Movie' ORDER BY "Name""#,
    )
    .fetch_all(db.pool())
    .await
    .expect("overviews");
    assert_eq!(
        overviews,
        [
            Some("Typed mid-scan".to_owned()),
            Some("Typed mid-scan".to_owned())
        ]
    );
}

/// A lock applied over the API while a scan runs is honoured for the item
/// the scan has not reached yet: its changed NFO is not read. The walk takes
/// the movies in listing order, so which one it reached before the lock
/// varies; only that one reads its NFO.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_applied_mid_scan_keeps_the_nfo_from_the_item_not_reached_yet() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    add_second_movie(tmp.path());
    scanner.scan_all().await.expect("first scan");

    for (movie, id) in [("Movie 0001 (2020)", 601), ("Movie 0002 (2020)", 602)] {
        let nfo = tmp
            .path()
            .join("movies")
            .join(movie)
            .join(format!("{movie}.nfo"));
        std::fs::write(&nfo, format!("<movie><tmdbid>{id}</tmdbid></movie>")).expect("nfo");
        touch_ahead(&nfo);
    }
    rescan_with_edit_mid_walk(
        &scanner,
        &db,
        &MetadataRefreshOptions::default(),
        r#"UPDATE "BaseItems" SET "IsLocked" = 1
           WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
    )
    .await;
    assert_eq!(
        tmdb_ids(&db).await.len(),
        1,
        "only the movie reached before the lock read its NFO"
    );

    // Control: unlocked, the same (changed) NFOs are read.
    set_locked(&db, 0).await;
    scanner.scan_all().await.expect("rescan while unlocked");
    let mut ids = tmdb_ids(&db).await;
    ids.sort();
    assert_eq!(ids, ["601", "602"]);
}

/// An unlock applied over the API while a scan runs is honoured for the item
/// the scan has not reached yet: it reads its changed NFO in this scan, not
/// the next one. The one reached before the unlock stays locked.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlock_applied_mid_scan_lets_the_item_not_reached_yet_read_its_nfo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;
    add_second_movie(tmp.path());
    scanner.scan_all().await.expect("first scan");
    set_locked(&db, 1).await;

    for (movie, id) in [("Movie 0001 (2020)", 601), ("Movie 0002 (2020)", 602)] {
        let nfo = tmp
            .path()
            .join("movies")
            .join(movie)
            .join(format!("{movie}.nfo"));
        std::fs::write(&nfo, format!("<movie><tmdbid>{id}</tmdbid></movie>")).expect("nfo");
        touch_ahead(&nfo);
    }
    rescan_with_edit_mid_walk(
        &scanner,
        &db,
        &MetadataRefreshOptions::default(),
        r#"UPDATE "BaseItems" SET "IsLocked" = 0
           WHERE "Type" LIKE '%Movies.Movie' RETURNING "Id""#,
    )
    .await;
    assert_eq!(
        tmdb_ids(&db).await.len(),
        1,
        "only the movie reached after the unlock read its NFO"
    );

    // Control: the movie reached first skipped its NFO only for its lock.
    scanner.scan_all().await.expect("rescan while unlocked");
    let mut ids = tmdb_ids(&db).await;
    ids.sort();
    assert_eq!(ids, ["601", "602"]);
}
