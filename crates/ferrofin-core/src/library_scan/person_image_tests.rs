//! When [`LibraryScanner::enrich_people`] writes a credited person's profile
//! image (gl-1274).
//!
//! Every saved item re-enriches its whole cast, so a recurring cast is
//! enriched once per credit of every episode. Upstream saves a person only
//! when something changed (`LibraryManager.SavePeopleMetadataAsync`'s
//! `saveEntity`); a person whose stored image rows already match must not take
//! the single writer, and one whose file changed must still be written.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, WrittenPerson};
use uuid::Uuid;

use super::LibraryScanner;
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
use crate::people_repository::FerrofinPeopleRepository;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// How long an enrichment that must not wait for the writer may take.
const NO_WAIT: Duration = Duration::from_secs(5);

/// Writes a `width`×`height` JPEG at `path`.
fn write_profile(path: &std::path::Path, width: u32, height: u32) {
    let mut profile = image::RgbImage::new(width, height);
    for (x, y, px) in profile.enumerate_pixels_mut() {
        *px = image::Rgb([
            u8::try_from(x % 256).unwrap_or(0),
            40,
            u8::try_from(y % 256).unwrap_or(0),
        ]);
    }
    profile.save(path).expect("write profile");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unchanged_profile_image_takes_no_writer_and_a_changed_one_is_written() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A file database: an in-memory one reads through its writer too.
    let url = format!("sqlite://{}", tmp.path().join("people.db").display());
    let db = Database::connect(&url).await.expect("connect");
    db.run_migrations().await.expect("migrations apply");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let items = FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()));

    let person_id = Uuid::new_v4();
    persistence
        .save_items(&[BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(person_id),
            type_: stored_type_name(BaseItemKind::Person)
                .expect("person type")
                .to_owned(),
            name: Some("Ada Lovelace".into()),
            ..Default::default()
        }])
        .await
        .expect("seed person");

    let meta_root = tmp.path().join("metadata");
    // Already on disk where `download_images` looks, so nothing is downloaded.
    let art_dir = crate::dynamic_images::own_art_dir(&meta_root, person_id);
    std::fs::create_dir_all(&art_dir).expect("art dir");
    let profile = art_dir.join("primary.jpg");
    write_profile(&profile, 48, 64);

    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence.clone()),
    );
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
        .with_image_processor(Arc::new(ImageProcessor::new(
            Arc::new(ImageCrateEncoder::new()),
            tmp.path().join("cache"),
        )))
        .with_metadata(
            Arc::new(ferrofin_providers::TmdbClient::new()),
            meta_root.clone(),
        );
    let people = FerrofinPeopleRepository::new(db.clone());
    let credit = || WrittenPerson {
        id: person_id,
        // No biography lookup: that branch is the only one that would reach
        // the network.
        needs_details: false,
        image_url: Some("https://image.tmdb.invalid/ada.jpg".into()),
        provider_id: None,
    };

    scanner.enrich_people(&people, vec![credit()]).await;
    let first = items.get_image_infos(person_id).await.expect("images");
    assert_eq!(
        first
            .iter()
            .map(|i| (i.width, i.height))
            .collect::<Vec<_>>(),
        [(48, 64)]
    );

    // The next item crediting the same person, the image unchanged: it must
    // finish while another writer holds the database.
    let held = db
        .writer()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("hold the writer");
    tokio::time::timeout(NO_WAIT, scanner.enrich_people(&people, vec![credit()]))
        .await
        .expect("an unchanged profile image waited for the writer");
    held.rollback().await.expect("release the writer");
    let kept = items.get_image_infos(person_id).await.expect("images");
    assert_eq!(kept, first, "the stored row is left as it was");

    // The file replaced: the next enrichment writes it.
    write_profile(&profile, 32, 40);
    std::fs::File::options()
        .write(true)
        .open(&profile)
        .and_then(|file| file.set_modified(SystemTime::now() + Duration::from_secs(60)))
        .expect("re-date the profile");
    scanner.enrich_people(&people, vec![credit()]).await;
    let changed = items.get_image_infos(person_id).await.expect("images");
    assert_eq!(
        changed
            .iter()
            .map(|i| (i.width, i.height))
            .collect::<Vec<_>>(),
        [(32, 40)],
        "a changed profile image is written"
    );
    assert_ne!(changed[0].date_modified, first[0].date_modified);

    // The Primary unchanged but another row stored beside it: the rows differ
    // from what would be written, so the write still runs and replaces the
    // set, leaving only the Primary, as before this guard (gl-99 is about
    // that replacement; this guard must not change it).
    let backdrop = ItemImageInfo {
        path: art_dir.join("backdrop.jpg").to_string_lossy().into_owned(),
        image_type: ImageType::Backdrop,
        date_modified: changed[0].date_modified,
        ..Default::default()
    };
    persistence
        .save_item_images(person_id, &[changed[0].clone(), backdrop])
        .await
        .expect("store a second row");
    scanner.enrich_people(&people, vec![credit()]).await;
    assert_eq!(
        items.get_image_infos(person_id).await.expect("images"),
        changed,
        "a person whose rows differ only by an extra row is still written"
    );
}
