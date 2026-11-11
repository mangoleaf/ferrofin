//! When [`LibraryScanner::enrich_people`] writes a credited person's profile
//! image (gl-1274, gl-99).
//!
//! Every saved item re-enriches its whole cast. Upstream gives a person the
//! credit's image only while it has no Primary (`LibraryManager.
//! SavePeopleMetadataAsync`: `if (... && !personEntity.HasImage(ImageType.
//! Primary)) personEntity.SetImage(.., 0)`), and saves nothing otherwise. So a
//! person with a Primary, wherever it lives, takes no download and no writer,
//! and the Primary it is given never removes the person's other images.

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

/// A file database with one stored `Person`, a scanner whose metadata
/// directory holds that person's `primary.jpg` (48×64) where
/// `download_images` looks (so nothing is downloaded), and the stores.
struct Fixture {
    /// Keeps the directory alive.
    _tmp: tempfile::TempDir,
    db: Database,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: FerrofinItemRepository,
    scanner: LibraryScanner,
    people: FerrofinPeopleRepository,
    person_id: Uuid,
    /// The person's art folder in the metadata directory.
    art_dir: std::path::PathBuf,
    /// Another folder (the person's own, an adopted Jellyfin path).
    elsewhere: std::path::PathBuf,
}

impl Fixture {
    async fn new() -> Self {
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
        let art_dir = crate::dynamic_images::own_art_dir(&meta_root, person_id);
        std::fs::create_dir_all(&art_dir).expect("art dir");
        write_profile(&art_dir.join("primary.jpg"), 48, 64);
        let elsewhere = tmp.path().join("People").join("A").join("Ada Lovelace");
        std::fs::create_dir_all(&elsewhere).expect("person folder");

        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
                .with_item_store(persistence.clone()),
        );
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_image_processor(Arc::new(ImageProcessor::new(
                    Arc::new(ImageCrateEncoder::new()),
                    tmp.path().join("cache"),
                )))
                .with_metadata(Arc::new(ferrofin_providers::TmdbClient::new()), meta_root);
        let people = FerrofinPeopleRepository::new(db.clone());
        Self {
            _tmp: tmp,
            db,
            persistence,
            items,
            scanner,
            people,
            person_id,
            art_dir,
            elsewhere,
        }
    }

    /// The person credited with a TMDB profile image.
    fn credit(&self) -> WrittenPerson {
        WrittenPerson {
            id: self.person_id,
            // No biography lookup: that branch is the only one that would
            // reach the network.
            needs_details: false,
            image_url: Some("https://image.tmdb.invalid/ada.jpg".into()),
            provider_id: None,
        }
    }

    async fn enrich(&self) {
        self.scanner
            .enrich_people(&self.people, vec![self.credit()])
            .await;
    }

    async fn images(&self) -> Vec<ItemImageInfo> {
        self.items
            .get_image_infos(self.person_id)
            .await
            .expect("images")
    }

    /// An image row of `image_type` for a file `name` in `dir`.
    fn row(dir: &std::path::Path, name: &str, image_type: ImageType) -> ItemImageInfo {
        let path = dir.join(name);
        write_profile(&path, 20, 30);
        ItemImageInfo {
            // Whole seconds, so the stored row reads back equal.
            date_modified: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("date"),
            path: path.to_string_lossy().into_owned(),
            image_type,
            width: 20,
            height: 30,
            blur_hash: None,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_with_a_primary_takes_no_writer_and_is_left_as_it_is() {
    let f = Fixture::new().await;

    f.enrich().await;
    let first = f.images().await;
    assert_eq!(
        first
            .iter()
            .map(|i| (i.image_type, i.width, i.height))
            .collect::<Vec<_>>(),
        [(ImageType::Primary, 48, 64)],
        "a person with no Primary is given the credit's"
    );

    // The next item crediting the same person: it must finish while another
    // writer holds the database.
    let held =
        f.db.writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("hold the writer");
    tokio::time::timeout(NO_WAIT, f.enrich())
        .await
        .expect("a person with a Primary waited for the writer");
    held.rollback().await.expect("release the writer");
    assert_eq!(f.images().await, first, "the stored row is left as it was");

    // The file replaced on disk: upstream's scan does not look at a Primary
    // the person has (the person's own refresh does).
    let profile = f.art_dir.join("primary.jpg");
    write_profile(&profile, 32, 40);
    std::fs::File::options()
        .write(true)
        .open(&profile)
        .and_then(|file| file.set_modified(SystemTime::now() + Duration::from_secs(60)))
        .expect("re-date the profile");
    f.enrich().await;
    assert_eq!(
        f.images().await,
        first,
        "an existing Primary is not rewritten"
    );

    // Another row stored beside the Primary is kept.
    let backdrop = Fixture::row(&f.art_dir, "backdrop.jpg", ImageType::Backdrop);
    f.persistence
        .save_item_images(f.person_id, &[first[0].clone(), backdrop.clone()])
        .await
        .expect("store a second row");
    f.enrich().await;
    assert_eq!(
        f.images().await,
        [first[0].clone(), backdrop],
        "the person's other images are kept"
    );
}

/// gl-99: a Primary stored outside the metadata art folder (an adopted
/// Jellyfin `People/A/Name/folder.jpg`, an upload saved to the person's
/// folder) was replaced by the credit's, and the person's Thumb deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_primary_stored_elsewhere_and_the_other_images_survive_a_credit() {
    let f = Fixture::new().await;
    let primary = Fixture::row(&f.elsewhere, "folder.jpg", ImageType::Primary);
    let thumb = Fixture::row(&f.elsewhere, "landscape.jpg", ImageType::Thumb);
    f.persistence
        .save_item_images(f.person_id, &[primary.clone(), thumb.clone()])
        .await
        .expect("seed the person's images");

    f.enrich().await;

    assert_eq!(f.images().await, [primary.clone(), thumb]);
    // The write itself also refuses a second Primary: an upload stored
    // while the credit's file downloads is not replaced.
    let added = f
        .persistence
        .add_item_image_if_absent(
            f.person_id,
            &Fixture::row(&f.art_dir, "primary.jpg", ImageType::Primary),
        )
        .await
        .expect("add");
    assert!(!added);
    assert_eq!(
        f.images()
            .await
            .into_iter()
            .filter(|i| i.image_type == ImageType::Primary)
            .collect::<Vec<_>>(),
        [primary]
    );
}

/// A person with other images but no Primary gets the credit's Primary
/// added beside them, as `SetImage` adds one image.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_primary_is_added_beside_the_other_images() {
    let f = Fixture::new().await;
    let thumb = Fixture::row(&f.elsewhere, "landscape.jpg", ImageType::Thumb);
    f.persistence
        .save_item_images(f.person_id, std::slice::from_ref(&thumb))
        .await
        .expect("seed a thumb");

    f.enrich().await;

    let images = f.images().await;
    assert!(images.contains(&thumb), "{images:?}");
    let primary: Vec<_> = images
        .iter()
        .filter(|i| i.image_type == ImageType::Primary)
        .map(|i| (i.path.clone(), i.width, i.height))
        .collect();
    assert_eq!(
        primary,
        [(
            f.art_dir.join("primary.jpg").to_string_lossy().into_owned(),
            48,
            64
        )]
    );
}
