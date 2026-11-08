//! A library rescan reads its albums' stored images in one go.
//!
//! After the walk, the album-covers pass (`finish_album_artwork`) checks that
//! each album the scan saw still stores the cover the walk gave it. It read
//! each album's image rows with a statement of its own, on every scan and for
//! unchanged albums too: one read per album, thousands on a music library
//! (gl-1277). It now reads them in one batched read.
//!
//! This binary records the statements one call runs ([`sql_statements`]).

mod sql_statements;

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use sql_statements::{record_statements, statements_of};
use uuid::Uuid;

/// The albums of the library.
const ALBUMS: usize = 3;

/// A music library of [`ALBUMS`] albums, each with a `cover.jpg` and two
/// tracks, under `root`, and a scanner over it.
async fn music_library(root: &Path) -> (LibraryScanner, Arc<dyn ItemRepository>) {
    let media = root.join("music");
    for album in 0..ALBUMS {
        let dir = media.join("Artist").join(format!("Album {album}"));
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbImage::from_pixel(4, 4, image::Rgb([40, 80, u8::try_from(album).unwrap()]))
            .save(dir.join("cover.jpg"))
            .unwrap();
        for track in 1..=2 {
            std::fs::write(dir.join(format!("0{track} Track.flac")), b"").unwrap();
        }
    }
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let store = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(store.clone()),
    );
    vf.add_virtual_folder(
        "Music",
        Some(CollectionTypeOptions::music),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .unwrap();
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db,
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), store)
        .with_items(items.clone())
        .with_metadata_dir(root.join("metadata"));
    (scanner, items)
}

/// The image paths every album and track stores, by item.
async fn stored_images(items: &dyn ItemRepository) -> Vec<(String, Vec<String>)> {
    let rows = items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::MusicAlbum, BaseItemKind::Audio],
            recursive: true,
            ..InternalItemsQuery::default()
        })
        .await
        .unwrap();
    let mut stored = Vec::new();
    for row in rows {
        let images = items
            .get_image_infos(Uuid::parse_str(&row.id).unwrap())
            .await
            .unwrap();
        stored.push((row.id, images.into_iter().map(|image| image.path).collect()));
    }
    stored.sort();
    stored
}

/// How many statements read one item's image rows.
fn one_item_image_reads(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|s| s.contains(r#"FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#))
        .count()
}

// A multi-thread runtime, as the server runs the scan on.
#[tokio::test(flavor = "multi_thread")]
async fn a_rescan_reads_no_album_images_one_album_at_a_time() {
    record_statements();
    let root = tempfile::tempdir().unwrap();
    let (scanner, items) = music_library(root.path()).await;
    Box::pin(scanner.scan_all()).await.unwrap();
    let before = stored_images(items.as_ref()).await;
    assert_eq!(before.len(), ALBUMS * 3, "the albums and their tracks");
    assert!(
        before.iter().all(|(_, paths)| paths.len() == 1),
        "every album and track stores its cover: {before:#?}"
    );

    let (rescanned, statements) = statements_of(Box::pin(scanner.scan_all())).await;

    rescanned.unwrap();
    assert_eq!(
        stored_images(items.as_ref()).await,
        before,
        "an unchanged rescan keeps every image"
    );
    assert_eq!(
        one_item_image_reads(&statements),
        0,
        "no album's images are read on their own: {statements:#?}"
    );
}
