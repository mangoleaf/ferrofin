//! The stored image rows a plain rescan keeps, compared with their files.
//!
//! A row no local provider re-supplies (a type discovery did not produce) is
//! kept while its file exists (`ItemImageProvider.ValidateImages`). Upstream
//! also compares it with that file: `Folder.ValidateChildrenInternal` runs
//! `LibraryManager.UpdateImagesAsync` over every unchanged current child (a
//! saved one reaches it through `RunMetadataSavers`), which measures each
//! local image `ImageNeedsRefresh` finds more than a second from its file's
//! last write time, and stores the file's date. These tests take the
//! row an adopted Jellyfin database holds most, a poster in Jellyfin's own
//! metadata layout stored as `%MetadataPath%/…`, beside a remote row, which a
//! scan leaves as stored.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, TimeZone as _, Utc};
use ferrofin_core::app_paths::FerrofinServerApplicationPaths;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::virtual_paths::VirtualPathExpander;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use uuid::Uuid;

mod counting_images;
use counting_images::CountingImages;

/// The poster's stored path, in the form Jellyfin writes it.
const POSTER: &str = "%MetadataPath%/library/ab/abcdef/poster.jpg";
/// A remote row, which no scan reads.
const BANNER: &str = "https://image.example/banner.jpg";

/// When the poster file was last written: a whole second, which the
/// database's tenth-of-a-microsecond dates hold exactly.
fn written() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .expect("a date")
}

/// A library of one movie, scanned once, whose rows were adopted from
/// Jellyfin: a poster under the metadata directory and a remote banner.
struct Library {
    _tmp: tempfile::TempDir,
    persistence: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    processor: Arc<CountingImages>,
    movie: Uuid,
}

impl Library {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let media = tmp.path().join("movies");
        let folder = media.join("Movie (2020)");
        std::fs::create_dir_all(&folder).expect("mkdir");
        let file = folder.join("Movie (2020).mkv");
        std::fs::write(&file, b"").expect("media file");
        let metadata = tmp.path().join("metadata");
        let poster = metadata.join("library/ab/abcdef/poster.jpg");
        std::fs::create_dir_all(poster.parent().expect("parent")).expect("mkdir");
        write_image(&poster, 40, 60);

        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            tmp.path(),
            "log",
            "config",
            "cache",
            "web",
        ));
        paths.set_internal_metadata_path(Some(metadata.to_str().expect("a UTF-8 path")));
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
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
        .with_items(items)
        .with_image_processor(Arc::clone(&processor) as _)
        .with_virtual_paths(VirtualPathExpander::from_paths(paths))
        .with_progress_every(0);
        scanner.scan_all().await.expect("scan");
        let movie = derive_item_id(BaseItemKind::Movie, &file.to_string_lossy()).expect("id");
        Self {
            _tmp: tmp,
            persistence,
            scanner,
            processor,
            movie,
        }
    }

    /// Stores the adopted rows: the poster as `poster` says, and the banner.
    async fn adopt(&self, poster: &ItemImageInfo) {
        self.persistence
            .save_item_images(self.movie, &[poster.clone(), banner()])
            .await
            .expect("store the image rows");
    }

    /// The movie's stored image rows, as stored (the scan reads them so).
    async fn rows(&self) -> Vec<ItemImageInfo> {
        let mut rows = self
            .persistence
            .scan_stored_links(&[self.movie])
            .await
            .expect("read the rows")
            .and_then(|mut links| links.remove(&self.movie))
            .map(|links| links.images)
            .unwrap_or_default();
        rows.sort_by_key(|image| image.path.clone());
        rows
    }

    /// The row stored for `path`.
    async fn row(&self, path: &str) -> ItemImageInfo {
        self.rows()
            .await
            .into_iter()
            .find(|image| image.path == path)
            .expect("the row")
    }
}

/// The poster as Jellyfin last measured it: `width`×`height` at `date`.
fn poster(width: i32, height: i32, date: DateTime<Utc>) -> ItemImageInfo {
    ItemImageInfo {
        path: POSTER.to_owned(),
        image_type: ImageType::Primary,
        date_modified: date,
        width,
        height,
        blur_hash: Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".to_owned()),
    }
}

/// The remote banner row.
fn banner() -> ItemImageInfo {
    ItemImageInfo {
        path: BANNER.to_owned(),
        image_type: ImageType::Banner,
        date_modified: Utc
            .with_ymd_and_hms(2019, 5, 6, 7, 8, 9)
            .single()
            .expect("a date"),
        width: 1000,
        height: 185,
        blur_hash: Some("L6PZfSi_.AyE_3t7t7R**0o#DgR4".to_owned()),
    }
}

/// Writes a `width`×`height` JPEG last written at [`written`].
fn write_image(path: &Path, width: u32, height: u32) {
    let mut image = image::RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgb([
            u8::try_from(x % 200).expect("under 200"),
            u8::try_from(y % 200).expect("under 200"),
            90,
        ]);
    }
    image.save(path).expect("write the image");
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(written().into())
        .expect("set mtime");
}

/// The poster file was replaced since Jellyfin measured it: the rescan reads
/// it through the expanded path, measures it and stores the file's date,
/// with the path still in its token form. The banner is left as stored.
#[tokio::test(flavor = "multi_thread")]
async fn an_adopted_poster_whose_file_changed_is_measured_through_its_token() {
    let library = Library::new().await;
    let before = poster(300, 450, written() - chrono::Duration::days(30));
    library.adopt(&before).await;

    library.scanner.scan_all().await.expect("rescan");

    let row = library.row(POSTER).await;
    assert_eq!((row.width, row.height), (40, 60));
    assert_eq!(row.date_modified, written());
    assert!(
        row.blur_hash
            .is_some_and(|hash| Some(hash) != before.blur_hash)
    );
    assert_eq!(library.row(BANNER).await, banner());
}

/// Rows that agree with their files are not measured, the first rescan
/// after adoption included, and the rescans after one that measured a row
/// measure nothing either.
#[tokio::test(flavor = "multi_thread")]
async fn a_rescan_of_settled_rows_measures_nothing() {
    let library = Library::new().await;
    let current = poster(300, 450, written() + chrono::Duration::milliseconds(900));
    library.adopt(&current).await;

    let before = library.processor.measured();
    library.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        library.processor.measured(),
        before,
        "a current row is kept"
    );
    assert_eq!(library.rows().await, vec![current, banner()]);

    library
        .adopt(&poster(300, 450, written() - chrono::Duration::days(30)))
        .await;
    library
        .scanner
        .scan_all()
        .await
        .expect("the rescan that settles it");
    let settled = library.rows().await;
    let before = library.processor.measured();
    library.scanner.scan_all().await.expect("rescan");
    library.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        library.processor.measured(),
        before,
        "a settled row is kept"
    );
    assert_eq!(library.rows().await, settled);
}
