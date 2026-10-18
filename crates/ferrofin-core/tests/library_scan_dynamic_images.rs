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
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions, ProviderManager};

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

/// The provider manager as the server wires it for `POST /Items/{id}/Images`:
/// the item store, the art root and the media-adjacent `ImageFileSaver`.
async fn uploader(
    root: &Path,
    items: &Arc<dyn ItemRepository>,
    vf: &Arc<dyn VirtualFolderManager>,
    persistence: &Arc<FerrofinItemPersistenceService>,
    meta_root: &Path,
) -> ferrofin_providers::LocalProviderManager {
    let paths = Arc::new(ferrofin_core::FerrofinServerApplicationPaths::new(
        root,
        root.join("log"),
        root.join("config"),
        root.join("cache"),
        root.join("web"),
    ));
    let configuration = Arc::new(
        ferrofin_core::FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("configuration"),
    );
    let saver = Arc::new(ferrofin_providers::image_save::ImageFileSaver::new(
        Arc::clone(items),
        Arc::clone(vf),
        configuration,
        meta_root.to_owned().into(),
    ));
    ferrofin_providers::LocalProviderManager::new(Vec::new())
        .with_image_saver(saver)
        .with_image_store(persistence.clone(), meta_root.to_owned())
        .with_remote_images(
            Arc::new(ferrofin_providers::TmdbClient::new()),
            Arc::clone(items),
        )
}

/// A solid `colour` 40×60 PNG, as an upload's body.
fn png(colour: [u8; 3]) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(40, 60, image::Rgb(colour))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("png");
    bytes.into_inner()
}

/// gl-590: a Primary uploaded to a genre is saved beside the genre's own
/// path, as `ImageSaver.SaveImage` saves it (`Genre.IsSaveLocalMetadataEnabled`
/// is true), with the file's own modification time — so neither a rescan nor
/// a full-refresh validation draws the collage over it. A row adopted from
/// Jellyfin stores that path as `%MetadataPath%/Genre/…`, expanded first.
#[rstest::rstest]
#[case::ferrofin(false)]
#[case::adopted(true)]
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_genre_primary_survives_rescans(#[case] adopted: bool) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    write_movie_library(&media);
    let metadata = tmp.path().join("metadata");
    let meta_root = metadata.join("library");

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
    let by_name = ferrofin_core::by_name_store::ByNameStore::new(
        persistence.clone(),
        metadata.join("Genre"),
        metadata.join("MusicGenre"),
        metadata.join("Studio"),
        metadata.join("artists"),
    );
    let scanner = LibraryScanner::new(
        Arc::clone(&vf),
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence.clone(),
    )
    .with_items(Arc::clone(&items))
    .with_image_processor(processor)
    .with_by_name_store(by_name)
    .with_metadata_dir(meta_root.clone());
    scanner.scan_all().await.expect("scan");
    let providers = uploader(tmp.path(), &items, &vf, &persistence, &meta_root).await;

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
    let genre_dir = Path::new(genre.path.as_deref().expect("a by-name path")).to_owned();
    if adopted {
        let mut row = genre.clone();
        row.path = Some("%MetadataPath%/Genre/Crime".to_owned());
        persistence
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("adopted path");
    }
    providers
        .save_image(genre_id, &png(BLUE), "image/png", ImageType::Primary, None)
        .await
        .expect("upload");

    let primary = || async {
        items
            .get_image_infos(genre_id)
            .await
            .expect("images")
            .into_iter()
            .find(|i| i.image_type == ImageType::Primary)
            .expect("a Primary")
    };
    let saved = primary().await;

    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    scanner.scan_all().await.expect("rescan");
    scanner.scan_with(None, &full).await.expect("full rescan");
    assert_eq!(primary().await.path, saved.path);
    let kept = image::open(&saved.path).expect("decodable").to_rgb8();
    assert_eq!(
        (kept.width(), kept.height(), kept.get_pixel(10, 10).0),
        (40, 60, BLUE),
        "the collage was drawn over the upload"
    );
    assert_eq!(
        Path::new(&saved.path),
        genre_dir.join("folder.png"),
        "saved beside the genre, outside its internal metadata folder"
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
    tmp: tempfile::TempDir,
    album_dir: std::path::PathBuf,
    scanner: LibraryScanner,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    vf: Arc<dyn VirtualFolderManager>,
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
            Arc::clone(&vf),
            Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_items(Arc::clone(&items))
        .with_image_processor(processor)
        .with_metadata_dir(tmp.path().join("metadata").join("library"));
        scanner.scan_all().await.expect("scan");
        Self {
            tmp,
            album_dir,
            scanner,
            items,
            persistence,
            vf,
        }
    }

    async fn uploader(&self) -> ferrofin_providers::LocalProviderManager {
        let root = self.tmp.path();
        uploader(
            root,
            &self.items,
            &self.vf,
            &self.persistence,
            &root.join("metadata").join("library"),
        )
        .await
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

/// gl-590: a Primary uploaded to a photo album whose library does not save
/// local metadata stays in the album's internal folder, as upstream's does,
/// and a default rescan finds it current and leaves it alone.
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_photo_album_primary_survives_a_default_rescan() {
    let lib = PhotoLibrary::scanned(RED).await;
    let album = lib.row(BaseItemKind::PhotoAlbum, "Trip").await;
    let album_id = uuid::Uuid::parse_str(&album.id).expect("id");
    lib.uploader()
        .await
        .save_image(album_id, &png(BLUE), "image/png", ImageType::Primary, None)
        .await
        .expect("upload");

    lib.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        lib.primary_colour(BaseItemKind::PhotoAlbum, "Trip").await,
        BLUE
    );
}

/// gl-590: a music genre's and a playlist folder's upload is saved beside the
/// item's own path whatever the library says, as `IsSaveLocalMetadataEnabled`
/// is overridden to true for both (`MusicGenre.cs:63`, `Playlist.cs:126`).
#[rstest::rstest]
#[case::music_genre("MediaBrowser.Controller.Entities.Audio.MusicGenre", false)]
#[case::playlist("MediaBrowser.Controller.Playlists.Playlist", true)]
#[tokio::test(flavor = "multi_thread")]
async fn uploads_to_music_genres_and_playlists_save_beside_their_path(
    #[case] type_: &str,
    #[case] is_folder: bool,
) {
    let lib = PhotoLibrary::scanned(RED).await;
    let id = uuid::Uuid::new_v4();
    let dir = lib.tmp.path().join("own").join("Mix");
    lib.persistence
        .save_items(&[BaseItemEntity {
            id: guid_to_db(id),
            type_: type_.to_owned(),
            name: Some("Mix".to_owned()),
            path: Some(dir.to_string_lossy().into_owned()),
            is_folder,
            ..BaseItemEntity::default()
        }])
        .await
        .expect("row");
    lib.uploader()
        .await
        .save_image(id, &png(BLUE), "image/png", ImageType::Primary, None)
        .await
        .expect("upload");
    let images = lib.items.get_image_infos(id).await.expect("images");
    assert_eq!(
        images
            .iter()
            .find(|i| i.image_type == ImageType::Primary)
            .map(|i| Path::new(&i.path).to_owned()),
        Some(dir.join("folder.png"))
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
