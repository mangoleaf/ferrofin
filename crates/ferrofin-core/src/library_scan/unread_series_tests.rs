//! The children of a series whose row cannot be read skip their TheTVDB
//! lookups without counting a failed request, so their refresh is stamped
//! instead of repeated on every scan.

use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_providers::rate_limit::count_request_failures;

use super::{ArtworkCache, LibraryScanner};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

#[tokio::test]
async fn an_unreadable_series_counts_no_failure_against_its_children() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let vf = Arc::new(FerrofinVirtualFolderManager::new(tmp.path().join("views")));
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    // Nothing listens there: a TheTVDB request would fail, and be counted.
    let tvdb = ferrofin_providers::TvdbClient::new().with_base_url("http://127.0.0.1:9");
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::new(crate::FerrofinItemRepository::new(
            db.clone(),
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        )))
        .with_tvdb(Arc::new(tvdb));
    db.close().await;
    let series = ferrofin_db::store::guid_to_db(uuid::Uuid::from_u128(1406));
    let mut cache = ArtworkCache::default();
    cache
        .item_provider_ids
        .insert(series.clone(), vec![("Tvdb".to_owned(), "5".to_owned())]);
    for kind in ["Season", "Episode"] {
        let child = BaseItemEntity {
            type_: format!("MediaBrowser.Controller.Entities.TV.{kind}"),
            series_id: Some(series.clone()),
            // Numbered, so a lookup guessing an order would reach TheTVDB.
            parent_index_number: Some(1),
            index_number: Some(1),
            ..Default::default()
        };
        // The save path reads the order first; a failed read is remembered.
        let (absolute, failures) =
            count_request_failures(scanner.remember_display_order(&mut cache, &child)).await;
        assert!(!absolute);
        assert_eq!(failures, 0, "{kind}: the order read");
        assert!(cache.unread_parents.contains(&series));
        let (images, failures) =
            count_request_failures(scanner.tvdb_child_images(&child, &cache)).await;
        assert!(images.is_empty(), "{images:?}");
        assert_eq!(failures, 0, "{kind}: the TheTVDB lookup");
    }
}
