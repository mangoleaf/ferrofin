//! What a library scan may and may not touch on a **metadata-locked** item.
//!
//! The scan reads the locked-item set once per scan
//! (`ItemRepository::locked_item_ids`) rather than hydrating every item's row
//! to read one boolean. These tests pin the behaviour that read feeds: the
//! loop's `locked` flag still has to mean `IsLocked`.
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
