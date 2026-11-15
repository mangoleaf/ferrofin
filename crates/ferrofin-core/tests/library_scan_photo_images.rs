//! A photo's Primary image is the photo file itself, through real scans.
//!
//! `PhotoProvider.FetchAsync` calls `SetImagePath` on every run, which dates
//! the image by the file's last write time and resets its size, and its
//! `ImageUpdate` result makes the save force `UpdateImagesAsync` over the
//! item's images. So every run of the provider measures the photo again: a
//! changed file, or any full refresh. A plain rescan of an unchanged photo
//! does not run the provider (`MetadataService.GetProviders`) and finds the
//! image current (`ImageNeedsRefresh`), so it measures nothing.
//!
//! A file rewritten with its mtime put back is what tells those apart without
//! counting decodes: the row only moves if something measured the file.
//!
//! A photo whose provider does not run (a locked one, or any plain rescan)
//! keeps its stored row, and upstream still compares that row with the file:
//! `Folder.ValidateChildrenInternal` runs `UpdateImagesAsync` over every
//! unchanged current child (a saved one reaches it through
//! `RunMetadataSavers`), which measures each image `ImageNeedsRefresh` finds
//! more than a second from its file's last write time.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::drawing::ImageProcessor as _;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

mod counting_images;
use counting_images::CountingImages;

/// The red-dominant look of the first version of the photo.
const RED: [u8; 3] = [200, 30, 30];
/// The blue-dominant look of the edited version.
const BLUE: [u8; 3] = [30, 30, 200];

/// When the photo was first written: a whole second, which the database's
/// tenth-of-a-microsecond dates hold exactly.
fn first_write() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .expect("a date")
}

/// A library of one photo.
struct Library {
    _tmp: tempfile::TempDir,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    processor: Arc<CountingImages>,
    photo: PathBuf,
}

impl Library {
    /// A library holding a 40×60 red photo, not yet scanned.
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let media = tmp.path().join("photos");
        std::fs::create_dir_all(&media).expect("mkdir");
        let photo = media.join("shot.jpg");
        write_photo(&photo, 40, 60, RED);
        set_last_write_time(&photo, first_write());
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Photos",
            Some(CollectionTypeOptions::homevideos),
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
        let processor = Arc::new(CountingImages::new(ImageProcessor::new(
            Arc::new(ImageCrateEncoder::new()),
            tmp.path().join("cache"),
        )));
        let scanner = LibraryScanner::new(
            vf,
            Arc::new(FerrofinFileSystem::new()),
            Arc::clone(&persistence) as _,
        )
        .with_items(Arc::clone(&items))
        .with_image_processor(Arc::clone(&processor) as _)
        .with_progress_every(0);
        Self {
            _tmp: tmp,
            items,
            persistence,
            scanner,
            processor,
            photo,
        }
    }

    /// The photo's id.
    fn id(&self) -> Uuid {
        derive_item_id(BaseItemKind::Photo, &self.photo.to_string_lossy()).expect("id")
    }

    /// Locks the photo's metadata, as the metadata editor's lock does.
    async fn lock(&self) {
        let mut row = self
            .items
            .retrieve_item(self.id())
            .await
            .expect("read the photo")
            .expect("the photo's row");
        row.is_locked = true;
        self.persistence
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("lock");
    }

    /// Stores `image` as the photo's only image row.
    async fn store(&self, image: &ItemImageInfo) {
        self.persistence
            .save_item_images(self.id(), std::slice::from_ref(image))
            .await
            .expect("store the image row");
    }

    /// How many measurements (a size or a blurhash) the scans asked for.
    fn measured(&self) -> usize {
        self.processor.measured()
    }

    /// The cache tag clients key the photo's `image` by.
    async fn tag(&self, image: &ItemImageInfo) -> Option<String> {
        self.processor
            .get_image_cache_tag(Some(&self.photo.to_string_lossy()), image)
            .await
            .expect("tag")
    }

    /// The stored Primary image row of the photo.
    async fn primary(&self) -> ItemImageInfo {
        self.items
            .get_image_infos(self.id())
            .await
            .expect("image rows")
            .into_iter()
            .find(|image| image.image_type == ImageType::Primary)
            .expect("the photo's Primary row")
    }

    /// Rewrites the photo with new pixels and a new size, leaving its last
    /// write time where it was: an edit a plain rescan cannot see.
    fn edit_keeping_mtime(&self) {
        write_photo(&self.photo, 90, 30, BLUE);
        set_last_write_time(&self.photo, first_write());
    }

    /// Moves the photo's last write time past the one second
    /// `ImageNeedsRefresh` tolerates, and returns it.
    fn touch(&self) -> DateTime<Utc> {
        let moved = first_write() + Duration::hours(1);
        set_last_write_time(&self.photo, moved);
        moved
    }
}

/// Writes a `width`×`height` JPEG of `base`, with enough of a pattern that
/// its blurhash follows the pixels.
fn write_photo(path: &Path, width: u32, height: u32, base: [u8; 3]) {
    let mut photo = image::RgbImage::new(width, height);
    for (x, y, pixel) in photo.enumerate_pixels_mut() {
        let [r, g, b] = base;
        *pixel = image::Rgb([
            r.saturating_add(u8::try_from(x % 40).expect("under 40")),
            g.saturating_add(u8::try_from(y % 40).expect("under 40")),
            b,
        ]);
    }
    photo.save(path).expect("write the photo");
}

fn set_last_write_time(path: &Path, at: DateTime<Utc>) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(at.into())
        .expect("set mtime");
}

/// The refresh jellyfin-web's "Replace all metadata" sends: every provider
/// runs, whatever looks stale.
fn full_refresh() -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        ..MetadataRefreshOptions::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_photos_image_is_dated_by_its_file() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");

    let image = library.primary().await;
    assert_eq!(image.date_modified, first_write());
    assert_eq!((image.width, image.height), (40, 60));
    assert!(image.blur_hash.is_some(), "the first scan hashes the photo");
}

/// The reported bug: the rescan counted the photo as updated, and its image
/// row kept the first scan's size, blurhash and a 1970 date.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_photo_is_measured_again() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;

    write_photo(&library.photo, 90, 30, BLUE);
    let moved = library.touch();
    library.scanner.scan_all().await.expect("rescan");

    let image = library.primary().await;
    assert_eq!((image.width, image.height), (90, 30));
    assert_eq!(image.date_modified, moved);
    assert_ne!(image.blur_hash, first.blur_hash);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_touched_photo_takes_its_files_new_date() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;

    let moved = library.touch();
    library.scanner.scan_all().await.expect("rescan");

    let image = library.primary().await;
    assert_eq!(image.date_modified, moved);
    assert_eq!((image.width, image.height), (first.width, first.height));
    assert_eq!(image.blur_hash, first.blur_hash);
}

/// An unchanged photo on a plain rescan costs no decode, here or upstream.
/// The edit keeps the mtime, so a measurement would show in the row.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_rescan_does_not_measure_a_photo_whose_mtime_did_not_move() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;

    library.edit_keeping_mtime();
    library.scanner.scan_all().await.expect("rescan");

    let image = library.primary().await;
    assert_eq!((image.width, image.height), (first.width, first.height));
    assert_eq!(image.blur_hash, first.blur_hash);
    assert_eq!(image.date_modified, first.date_modified);
}

/// The way out of the case above: a full refresh runs `PhotoProvider` over
/// any photo, so it measures the file whatever its date says.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_refresh_measures_a_photo_whose_mtime_did_not_move() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;

    library.edit_keeping_mtime();
    library
        .scanner
        .scan_with(None, &full_refresh())
        .await
        .expect("full refresh");

    let image = library.primary().await;
    assert_eq!((image.width, image.height), (90, 30));
    assert_ne!(image.blur_hash, first.blur_hash);
    assert_eq!(image.date_modified, first.date_modified);
}

/// gl-1126: a locked photo's provider does not run, so its row is the one
/// stored, and the file it was measured from has changed since. Upstream's
/// `UpdateImagesAsync` over the current child finds the row more than a
/// second from the file (`ImageNeedsRefresh`) and measures it, so its size,
/// blurhash and date (and the tag that hashes the date) follow the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_photo_is_measured_again_when_its_file_changes() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;
    library.lock().await;

    write_photo(&library.photo, 90, 30, BLUE);
    let moved = library.touch();
    library.scanner.scan_all().await.expect("rescan");

    let image = library.primary().await;
    assert_eq!((image.width, image.height), (90, 30));
    assert_eq!(image.date_modified, moved);
    assert_ne!(image.blur_hash, first.blur_hash);
    assert_ne!(library.tag(&image).await, library.tag(&first).await);
}

/// gl-1126: a row stored before gl-1057 dates the photo 1970. A plain rescan
/// does not run the photo's provider, so only the comparison with the file
/// can put it right; it measures the photo once and stores the file's date.
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_rescan_measures_a_photo_row_dated_before_its_file() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;
    library
        .store(&ItemImageInfo {
            date_modified: DateTime::<Utc>::UNIX_EPOCH,
            ..first.clone()
        })
        .await;

    library.edit_keeping_mtime();
    library.scanner.scan_all().await.expect("rescan");

    let image = library.primary().await;
    assert_eq!((image.width, image.height), (90, 30));
    assert_ne!(image.blur_hash, first.blur_hash);
    assert_eq!(image.date_modified, first_write());
}

/// Once the rows agree with their files, a plain rescan measures nothing.
/// The photo's row is first dated 1970 and settled by a rescan, then the
/// photo is locked and edited and settled by another.
#[tokio::test(flavor = "multi_thread")]
async fn a_rescan_of_settled_photos_measures_nothing() {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let first = library.primary().await;
    library
        .store(&ItemImageInfo {
            date_modified: DateTime::<Utc>::UNIX_EPOCH,
            ..first
        })
        .await;
    library
        .scanner
        .scan_all()
        .await
        .expect("the rescan that settles the 1970 row");
    library.lock().await;
    write_photo(&library.photo, 90, 30, BLUE);
    library.touch();
    library
        .scanner
        .scan_all()
        .await
        .expect("the rescan that settles the locked edit");
    let settled = library.primary().await;
    assert_eq!((settled.width, settled.height), (90, 30));

    let before = library.measured();
    library.scanner.scan_all().await.expect("rescan");
    library.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        library.measured(),
        before,
        "a settled photo is not measured"
    );
    assert_eq!(library.primary().await, settled);
}

/// gl-1530: `PhotoProvider.HasChanged` runs the photo's reader whenever the
/// file's mtime drifted from the stored `DateModified`, below `Default` too
/// (`GetProviders` runs the monitors that fired), and against an unset
/// `DateModified` (`DateTime.MinValue`), which `RequiresRefresh` skips. Either
/// way the photo's own size follows the rewritten file.
#[rstest::rstest]
#[case::validation_only(MetadataRefreshMode::ValidationOnly, false)]
#[case::no_stored_date(MetadataRefreshMode::Default, true)]
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_photo_is_read_again_by_its_change_monitor(
    #[case] mode: MetadataRefreshMode,
    #[case] unset_date: bool,
) {
    let library = Library::new().await;
    library.scanner.scan_all().await.expect("scan");
    let read = || async {
        let row = library
            .items
            .retrieve_item(library.id())
            .await
            .expect("read the photo")
            .expect("the photo's row");
        (row.width, row.height)
    };
    assert_eq!(read().await, (Some(40), Some(60)));
    if unset_date {
        let mut row = library
            .items
            .retrieve_item(library.id())
            .await
            .expect("read the photo")
            .expect("the photo's row");
        row.date_modified = None;
        library
            .persistence
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("unset DateModified");
        library.edit_keeping_mtime();
    } else {
        write_photo(&library.photo, 90, 30, BLUE);
        library.touch();
    }

    let options = MetadataRefreshOptions {
        metadata_refresh_mode: mode,
        image_refresh_mode: mode,
        ..MetadataRefreshOptions::default()
    };
    library
        .scanner
        .scan_with(None, &options)
        .await
        .expect("rescan");
    assert_eq!(read().await, (Some(90), Some(30)));

    // The read stored the file's date, so the monitor settles.
    let before = library.measured();
    library
        .scanner
        .scan_with(None, &options)
        .await
        .expect("rescan");
    assert_eq!(library.measured(), before, "a settled photo is not read");
}
