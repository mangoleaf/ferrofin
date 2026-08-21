//! What a library scan may and may not touch on a **metadata-locked** item.
//!
//! The scan reads the locked-item set once per scan
//! (`ItemRepository::locked_item_ids`) rather than hydrating every item's row
//! to read one boolean. These tests pin the behaviour that read feeds: the
//! loop's `locked` flag still has to mean `IsLocked`.
//!
//! Artwork is the assertion with teeth. The user-owned *metadata* columns are
//! protected a second time inside the scan upsert (`CASE WHEN "IsLocked" = 1
//! THEN "<col>" ELSE excluded."<col>" END`), so a test asserting on those would
//! still pass with the loop's flag stuck at `false`. The image rows have no such
//! SQL backstop — they are rewritten purely on the strength of the flag.

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
async fn a_locked_item_keeps_its_artwork_across_a_rescan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (scanner, db) = one_movie_library(tmp.path()).await;

    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        image_rows(&db).await,
        1,
        "the first scan discovers the poster"
    );

    // The user locks the item, then clears its artwork.
    set_locked(&db, 1).await;
    sqlx::query(r#"DELETE FROM "BaseItemImageInfos""#)
        .execute(db.writer())
        .await
        .expect("clear images");

    scanner.scan_all().await.expect("rescan while locked");
    assert_eq!(
        image_rows(&db).await,
        0,
        "a locked item's artwork is user-owned; the rescan must not rewrite it"
    );

    // Control: the poster is still there and still discoverable. Without this,
    // the assertion above would also pass if the scan had simply stopped
    // finding the file.
    set_locked(&db, 0).await;
    scanner.scan_all().await.expect("rescan while unlocked");
    assert_eq!(
        image_rows(&db).await,
        1,
        "an unlocked item's artwork is rediscovered"
    );
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
