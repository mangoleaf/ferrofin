//! How often a library scan lists an item's metadata art folders (gl-1280).
//!
//! Each item's artwork pass lists the folders the scan writes its art to
//! (uploads, earlier downloads and extractions) before its image providers
//! run, and listed them again after them. On a plain rescan no remote
//! provider runs and the extractors add what they write to the item's
//! images, so the second listing could find nothing new: two listings per
//! item on every scan. The second now runs only after a remote fetch or a
//! cancel that cut the providers short.

use std::sync::Arc;

use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::VirtualFolderManager;

use super::{ART_DIR_LISTINGS, LibraryScanner};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_type_lookup::derive_item_id;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// The art-folder listings made on this thread so far.
fn listings() -> usize {
    ART_DIR_LISTINGS.with(std::cell::Cell::get)
}

// A current-thread runtime, so every listing is made on this thread.
#[tokio::test]
async fn a_plain_rescan_lists_each_items_art_folders_once_and_keeps_its_upload() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    std::fs::create_dir_all(&media).expect("media dir");
    for name in ["Alpha (2001).mkv", "Beta (2002).mkv"] {
        std::fs::write(media.join(name), b"").expect("movie");
    }
    let meta = tmp.path().join("metadata");
    // An uploaded logo, in the folder the scan writes the first movie's art to.
    let alpha = derive_item_id(
        BaseItemKind::Movie,
        &media.join("Alpha (2001).mkv").to_string_lossy(),
    )
    .expect("id");
    let art = crate::dynamic_images::own_art_dir(&meta, alpha);
    std::fs::create_dir_all(&art).expect("art dir");
    let logo = art.join("logo.png");
    std::fs::write(&logo, b"logo").expect("logo");

    let db = crate::test_support::test_db().await;
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let items = crate::test_support::item_repository_over(db);
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
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
    .expect("library");
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(items.clone())
        .with_metadata_dir(meta);
    Box::pin(scanner.scan_all()).await.expect("first scan");
    let uploaded = items.get_image_infos(alpha).await.expect("images");

    let before = listings();
    let rescan = Box::pin(scanner.scan_all()).await.expect("rescan");
    let listed = listings() - before;

    assert_eq!(
        (rescan.created, rescan.updated),
        (0, 0),
        "a plain rescan: {rescan:?}"
    );
    assert_eq!(
        listed, rescan.unchanged,
        "one listing per item walked: {rescan:?}"
    );
    assert_eq!(
        uploaded
            .iter()
            .map(|image| (image.image_type, image.path.clone()))
            .collect::<Vec<_>>(),
        [(ImageType::Logo, logo.to_string_lossy().into_owned())],
        "the first scan stores the upload"
    );
    assert_eq!(
        items.get_image_infos(alpha).await.expect("images"),
        uploaded,
        "the rescan keeps it"
    );
}
