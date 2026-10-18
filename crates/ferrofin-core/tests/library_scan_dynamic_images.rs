//! The library scan's dynamic-image pass, end to end: a movie library whose
//! NFO declares a genre and whose poster sits beside the file must leave the
//! materialized `Genre` row with a generated 600×600 Primary on disk and in
//! `BaseItemImageInfos` — the file `GET /Genres/{name}/Images/Primary` serves.
//!
//! Port target: `GenreImageProvider` over `BaseDynamicImageProvider`, which
//! Jellyfin runs from `GenresValidator` at the end of library validation.
//!
//! Also the scan side of an item refresh (gl-576): a photo album's refresh and
//! a full-refresh validation run the same providers as custom providers.

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner, ScanCancel, ScanPasses, ScanRun,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// One movie with a Kodi sidecar naming the genre, and a real PNG poster.
fn write_movie_library(media: &Path) {
    let dir = media.join("Heat (1995)");
    std::fs::create_dir_all(&dir).expect("fixture dirs");
    std::fs::write(dir.join("Heat (1995).mkv"), b"").expect("media file");
    std::fs::write(
        dir.join("movie.nfo"),
        r#"<?xml version="1.0"?>
<movie><title>Heat</title><year>1995</year><genre>Crime</genre></movie>"#,
    )
    .expect("nfo");
    let mut poster = image::RgbImage::new(40, 60);
    for px in poster.pixels_mut() {
        *px = image::Rgb([180, 30, 30]);
    }
    poster.save(dir.join("poster.png")).expect("poster");
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_generates_the_genre_collage_from_the_library_posters() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    write_movie_library(&media);
    let meta_root = tmp.path().join("metadata").join("library");

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
    let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
        ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), tmp.path().join("cache")),
    );
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(Arc::clone(&items))
    .with_image_processor(processor)
    .with_metadata_dir(meta_root.clone());

    scanner.scan_all().await.expect("scan");

    let genre = items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Genre],
            name: Some("Crime".to_owned()),
            ..InternalItemsQuery::default()
        })
        .await
        .expect("genre query")
        .into_iter()
        .next()
        .expect("the NFO genre is materialized as a Genre row");
    let genre_id = uuid::Uuid::parse_str(&genre.id).expect("genre id");
    let images = items.get_image_infos(genre_id).await.expect("images");
    let primary = images
        .iter()
        .find(|i| i.image_type == ImageType::Primary)
        .expect("the scan generated a Primary for the genre");
    assert_eq!(
        Path::new(&primary.path),
        meta_root.join(guid_to_db(genre_id)).join("primary.png"),
        "written into the genre's own art folder"
    );
    let decoded = image::open(&primary.path).expect("a decodable PNG");
    assert_eq!((decoded.width(), decoded.height()), (600, 600));
    assert_eq!((primary.width, primary.height), (600, 600));
    // The poster's colour lands in the top-left tile — proof the collage was
    // composed from the library's artwork rather than an empty canvas.
    assert_eq!(decoded.to_rgb8().get_pixel(10, 10).0, [180, 30, 30]);

    // A second scan finds the image current and leaves it untouched.
    let before = std::fs::metadata(&primary.path)
        .and_then(|m| m.modified())
        .expect("mtime");
    scanner.scan_all().await.expect("rescan");
    let after = std::fs::metadata(&primary.path)
        .and_then(|m| m.modified())
        .expect("mtime");
    assert_eq!(before, after, "a current collage is not redrawn on rescan");
    let images = items.get_image_infos(genre_id).await.expect("images");
    assert_eq!(
        images
            .iter()
            .filter(|i| i.image_type == ImageType::Primary)
            .count(),
        1
    );
}

const RED: [u8; 3] = [180, 30, 30];
const BLUE: [u8; 3] = [30, 30, 180];

/// A solid `colour` photo at `{dir}/a.png`.
fn write_photo(dir: &Path, colour: [u8; 3]) {
    image::RgbImage::from_pixel(40, 60, image::Rgb(colour))
        .save(dir.join("a.png"))
        .expect("photo");
}

/// A home-video library over `Trip/a.png`, scanned once.
struct PhotoLibrary {
    _tmp: tempfile::TempDir,
    album_dir: std::path::PathBuf,
    scanner: LibraryScanner,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
}

impl PhotoLibrary {
    async fn scanned(colour: [u8; 3]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("photos");
        let album_dir = media.join("Trip");
        std::fs::create_dir_all(&album_dir).expect("album dir");
        write_photo(&album_dir, colour);
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
        let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
            ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), tmp.path().join("cache")),
        );
        let scanner = LibraryScanner::new(
            vf,
            Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_items(Arc::clone(&items))
        .with_image_processor(processor)
        .with_metadata_dir(tmp.path().join("metadata").join("library"));
        scanner.scan_all().await.expect("scan");
        Self {
            _tmp: tmp,
            album_dir,
            scanner,
            items,
            persistence,
        }
    }

    async fn row(&self, kind: BaseItemKind, name: &str) -> BaseItemEntity {
        self.items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![kind],
                name: Some(name.to_owned()),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("query")
            .into_iter()
            .next()
            .expect("row")
    }

    /// The colour of the row's Primary at a pixel of its first tile.
    async fn primary_colour(&self, kind: BaseItemKind, name: &str) -> [u8; 3] {
        let id = uuid::Uuid::parse_str(&self.row(kind, name).await.id).expect("id");
        let images = self.items.get_image_infos(id).await.expect("images");
        let primary = images
            .iter()
            .find(|i| i.image_type == ImageType::Primary)
            .expect("a Primary");
        image::open(&primary.path)
            .expect("decodable")
            .to_rgb8()
            .get_pixel(10, 10)
            .0
    }
}

/// gl-576: a photo album's own refresh (a scan of its folder with the
/// folder-refresh closing passes, as `POST /Items/{id}/Refresh` queues it)
/// runs `PhotoAlbumImageProvider`: forced by a full refresh, behind its date
/// gate on a Default one, not at all with metadata mode `None` or for a
/// locked album.
#[rstest::rstest]
#[case::full_refresh(MetadataRefreshMode::FullRefresh, false, BLUE)]
#[case::default(MetadataRefreshMode::Default, false, RED)]
#[case::mode_none(MetadataRefreshMode::None, false, RED)]
#[case::locked(MetadataRefreshMode::FullRefresh, true, RED)]
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_album_refresh_redraws_its_image_as_its_options_say(
    #[case] mode: MetadataRefreshMode,
    #[case] locked: bool,
    #[case] want: [u8; 3],
) {
    let lib = PhotoLibrary::scanned(RED).await;
    assert_eq!(
        lib.primary_colour(BaseItemKind::PhotoAlbum, "Trip").await,
        RED
    );
    if locked {
        let mut album = lib.row(BaseItemKind::PhotoAlbum, "Trip").await;
        album.is_locked = true;
        lib.persistence
            .save_items(std::slice::from_ref(&album))
            .await
            .expect("lock");
    }
    write_photo(&lib.album_dir, BLUE);

    let options = MetadataRefreshOptions {
        metadata_refresh_mode: mode,
        image_refresh_mode: mode,
        ..MetadataRefreshOptions::default()
    };
    // A folder refresh refreshes nothing above the folder.
    let above = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    let cancel = ScanCancel::new();
    lib.scanner
        .scan_target(
            &ScanTarget::Paths(vec![lib.album_dir.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &above, &cancel).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("refresh");
    assert_eq!(
        lib.primary_colour(BaseItemKind::PhotoAlbum, "Trip").await,
        want
    );
}

/// gl-576: a library validation whose options run every provider redraws the
/// albums it walked and the library tile past their date gates, where the
/// default validation leaves both current images alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_refresh_validation_redraws_albums_and_the_library_tile() {
    let lib = PhotoLibrary::scanned(RED).await;
    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        RED
    );
    write_photo(&lib.album_dir, BLUE);

    lib.scanner.scan_all().await.expect("default rescan");
    assert_eq!(
        lib.primary_colour(BaseItemKind::PhotoAlbum, "Trip").await,
        RED
    );
    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        RED
    );

    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    lib.scanner
        .scan_with(None, &full)
        .await
        .expect("full rescan");
    assert_eq!(
        lib.primary_colour(BaseItemKind::PhotoAlbum, "Trip").await,
        BLUE
    );
    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        BLUE
    );
}

/// gl-576: a library's own refresh (`ScanTarget::Library` with the
/// folder-refresh closing passes, as `POST /Items/{id}/Refresh` queues it)
/// forces `CollectionFolderImageProvider` when it runs every provider — and
/// leaves a locked library's tile alone, as upstream returns before a locked
/// item's custom providers.
#[rstest::rstest]
#[case::unlocked(false, BLUE)]
#[case::locked(true, RED)]
#[tokio::test(flavor = "multi_thread")]
async fn a_library_refresh_redraws_its_tile_unless_locked(
    #[case] locked: bool,
    #[case] want: [u8; 3],
) {
    let lib = PhotoLibrary::scanned(RED).await;
    let mut library = lib.row(BaseItemKind::CollectionFolder, "Photos").await;
    let library_id = uuid::Uuid::parse_str(&library.id).expect("id");
    if locked {
        library.is_locked = true;
        lib.persistence
            .save_items(std::slice::from_ref(&library))
            .await
            .expect("lock");
    }
    write_photo(&lib.album_dir, BLUE);

    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    let above = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    let cancel = ScanCancel::new();
    lib.scanner
        .scan_target(
            &ScanTarget::Library(library_id),
            ScanRun::new(&full, &above, &cancel).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("refresh");
    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        want
    );
}
