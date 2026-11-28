//! The children of a series whose row cannot be read skip their TheTVDB
//! lookups, and their episodes their TheMovieDb ones, without counting a
//! failed request, so their refresh is stamped instead of repeated on every
//! scan.

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

/// TheMovieDb looks an episode up in its series' display order (gl-1784), so
/// with no order to read it asks nothing rather than guess the aired one, as
/// upstream's lookup has no series ids without a series.
#[tokio::test]
async fn an_unreadable_series_asks_tmdb_nothing_for_its_episodes() {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let vf = Arc::new(FerrofinVirtualFolderManager::new(tmp.path().join("views")));
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    // Nothing listens there: a TheMovieDb request would fail, and be counted.
    let tmdb = ferrofin_providers::TmdbClient::new().with_base_url("http://127.0.0.1:9");
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::new(crate::FerrofinItemRepository::new(
            db.clone(),
            Arc::new(crate::item_type_lookup::ItemTypeLookup::new()),
        )))
        .with_metadata(Arc::new(tmdb), tmp.path().join("metadata"));
    db.close().await;
    let series = ferrofin_db::store::guid_to_db(uuid::Uuid::from_u128(1784));
    let mut cache = ArtworkCache::default();
    cache.series_tmdb.insert(series.clone(), 1399);
    let episode = BaseItemEntity {
        type_: "MediaBrowser.Controller.Entities.TV.Episode".to_owned(),
        series_id: Some(series.clone()),
        parent_index_number: Some(1),
        index_number: Some(1),
        ..Default::default()
    };

    let (answer, failures) = count_request_failures(Box::pin(scanner.fetch_tmdb_episode(
        &episode,
        &mut cache,
        &super::ResolverGuesses::default(),
    )))
    .await;
    assert!(answer.is_none());
    assert_eq!(failures, 0, "the metadata lookup");
    let (numbers, failures) = count_request_failures(Box::pin(scanner.tmdb_episode_image_numbers(
        &mut cache,
        (&series, 1399),
        (1, 1),
    )))
    .await;
    assert_eq!(numbers, None);
    assert_eq!(failures, 0, "the image lookup");
}
