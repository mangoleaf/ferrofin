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

/// `colour` under the black rectangle at alpha `0x78` a library tile (a thumb
/// collage) draws over its backdrop, as `StripCollageBuilder` does.
fn dimmed(colour: [u8; 3]) -> [u8; 3] {
    colour.map(|c| u8::try_from((u16::from(c) * (255 - 0x78) + 127) / 255).unwrap_or(u8::MAX))
}

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
            FerrofinVirtualFolderManager::new(tmp.path().join("root").join("default"))
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

const MUSIC_GENRE: &str = "MediaBrowser.Controller.Entities.Audio.MusicGenre";
const PLAYLIST: &str = "MediaBrowser.Controller.Playlists.Playlist";
const PERSON: &str = "MediaBrowser.Controller.Entities.Person";
const STUDIO: &str = "MediaBrowser.Controller.Entities.Studio";
const YEAR: &str = "MediaBrowser.Controller.Entities.Year";
const MUSIC_ARTIST: &str = "MediaBrowser.Controller.Entities.Audio.MusicArtist";
const BOX_SET: &str = "MediaBrowser.Controller.Entities.Movies.BoxSet";
const COLLECTION_FOLDER: &str = "MediaBrowser.Controller.Entities.CollectionFolder";

/// gl-590, gl-598: an upload is saved beside the item's own path whatever the
/// library says for the kinds whose `IsSaveLocalMetadataEnabled` is
/// overridden to true — a music genre and a playlist (`MusicGenre.cs:63`,
/// `Playlist.cs:126`), a person, a studio, a year and a by-name artist (each
/// also `ContainingFolderPath => Path`), a box set and a library
/// (`CollectionFolder.cs:196`) — while that path is in the server's own
/// directories: data, metadata, and the libraries' root folder. A playlist
/// file or a box-set folder inside a media library whose "Save artwork into
/// media folders" is off keeps its upload in the internal metadata folder:
/// nothing is written into the media.
#[rstest::rstest]
#[case::music_genre(
    MUSIC_GENRE,
    "metadata/MusicGenre/Mix",
    false,
    false,
    Some("folder.png")
)]
#[case::playlist(PLAYLIST, "data/playlists/Mix", true, false, Some("folder.png"))]
#[case::playlist_file_in_a_library(PLAYLIST, "photos/Mix.m3u", true, true, None)]
#[case::person(PERSON, "metadata/People/M/Mix", false, false, Some("folder.png"))]
#[case::studio(STUDIO, "metadata/Studio/Mix", false, false, Some("folder.png"))]
#[case::year(YEAR, "metadata/Year/2020", false, false, Some("folder.png"))]
#[case::by_name_artist(MUSIC_ARTIST, "metadata/artists/Mix", false, false, Some("folder.png"))]
#[case::box_set(
    BOX_SET,
    "data/collections/Mix [boxset]",
    true,
    false,
    Some("folder.png")
)]
#[case::box_set_in_a_library(BOX_SET, "photos/Mix [boxset]", true, false, None)]
#[case::library(COLLECTION_FOLDER, "root/default/Mix", true, false, Some("folder.png"))]
#[tokio::test(flavor = "multi_thread")]
async fn uploads_save_beside_their_path_for_the_overriding_kinds(
    #[case] type_: &str,
    #[case] path: &str,
    #[case] is_folder: bool,
    #[case] is_file: bool,
    #[case] beside: Option<&str>,
) {
    let lib = PhotoLibrary::scanned(RED).await;
    let id = uuid::Uuid::new_v4();
    let path = lib.tmp.path().join(path);
    lib.persistence
        .save_items(&[BaseItemEntity {
            id: guid_to_db(id),
            type_: type_.to_owned(),
            name: Some("Mix".to_owned()),
            path: Some(path.to_string_lossy().into_owned()),
            is_folder,
            is_in_mixed_folder: is_file,
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
    let want = match beside {
        Some(name) => path.join(name),
        None => lib
            .tmp
            .path()
            .join("metadata/library")
            .join(guid_to_db(id))
            .join("primary.png"),
    };
    assert_eq!(
        images
            .iter()
            .find(|i| i.image_type == ImageType::Primary)
            .map(|i| Path::new(&i.path).to_owned()),
        Some(want)
    );
    assert!(
        !lib.tmp.path().join("photos/Mix-poster.png").exists(),
        "nothing written into the media library"
    );
}

/// A library tile is recorded as any saved image is: its size and blurhash
/// read from the file (`LibraryManager.UpdateImagesAsync`), and the file's own
/// modification time, so `/UserViews` carries `ImageBlurHashes.Primary`.
#[tokio::test(flavor = "multi_thread")]
async fn a_library_tile_is_recorded_with_its_blurhash_and_file_time() {
    let lib = PhotoLibrary::scanned(RED).await;
    let id = uuid::Uuid::parse_str(&lib.row(BaseItemKind::CollectionFolder, "Photos").await.id)
        .expect("id");
    let images = lib.items.get_image_infos(id).await.expect("images");
    let tile = images
        .iter()
        .find(|i| i.image_type == ImageType::Primary)
        .expect("a drawn tile");
    let (width, height) = image::image_dimensions(&tile.path).expect("decodable");
    assert_eq!(
        (tile.width, tile.height),
        (
            i32::try_from(width).expect("width"),
            i32::try_from(height).expect("height")
        )
    );
    assert!(
        tile.blur_hash.as_deref().is_some_and(|h| !h.is_empty()),
        "the tile has a blurhash: {tile:?}"
    );
    let modified = std::fs::metadata(&tile.path)
        .and_then(|m| m.modified())
        .map(chrono::DateTime::<chrono::Utc>::from)
        .expect("mtime");
    assert_eq!(
        db_ticks(tile.date_modified),
        db_ticks(modified),
        "the row's date is the file's"
    );
}

/// A date at the database's 100 ns precision.
fn db_ticks(date: chrono::DateTime<chrono::Utc>) -> i64 {
    date.timestamp_nanos_opt().expect("in range") / 100
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
        dimmed(RED)
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
        dimmed(RED)
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
        dimmed(BLUE)
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
        dimmed(want)
    );
}

/// gl-594: a library refresh runs every provider on the library's own first
/// refresh (`isFirstRefresh` at `Default` or above), so a Default refresh of
/// a library never refreshed before redraws its tile past the date gate. It
/// stamps the library's `DateLastRefreshed`, so the next Default refresh is
/// behind the gate again.
#[tokio::test(flavor = "multi_thread")]
async fn a_librarys_first_default_refresh_redraws_its_tile() {
    let lib = PhotoLibrary::scanned(RED).await;
    let mut library = lib.row(BaseItemKind::CollectionFolder, "Photos").await;
    let library_id = uuid::Uuid::parse_str(&library.id).expect("id");
    // A library whose tile was drawn before it was ever refreshed (a
    // library row written by an older Ferrofin carries no stamp).
    library.date_last_refreshed = None;
    lib.persistence
        .save_items(std::slice::from_ref(&library))
        .await
        .expect("unstamp");
    let refresh = || async {
        let default = MetadataRefreshOptions::default();
        let above = MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::None,
            image_refresh_mode: MetadataRefreshMode::None,
            ..MetadataRefreshOptions::default()
        };
        let cancel = ScanCancel::new();
        lib.scanner
            .scan_target(
                &ScanTarget::Library(library_id),
                ScanRun::new(&default, &above, &cancel).with_passes(ScanPasses::Touched),
            )
            .await
            .expect("refresh");
    };

    write_photo(&lib.album_dir, BLUE);
    refresh().await;
    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        dimmed(BLUE),
        "the first refresh runs every provider"
    );
    assert!(
        lib.row(BaseItemKind::CollectionFolder, "Photos")
            .await
            .date_last_refreshed
            .is_some()
    );

    write_photo(&lib.album_dir, RED);
    refresh().await;
    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        dimmed(BLUE),
        "a refreshed library's current tile is behind its gate"
    );
}

/// The row's Primary image row.
async fn primary_of(
    items: &Arc<dyn ItemRepository>,
    id: uuid::Uuid,
) -> ferrofin_traits::options::ItemImageInfo {
    items
        .get_image_infos(id)
        .await
        .expect("images")
        .into_iter()
        .find(|i| i.image_type == ImageType::Primary)
        .expect("a Primary")
}

/// Replaces the row's Primary with one at `path`, dated `date_modified`.
async fn set_primary_of(
    items: &Arc<dyn ItemRepository>,
    persistence: &FerrofinItemPersistenceService,
    id: uuid::Uuid,
    path: &str,
    date_modified: chrono::DateTime<chrono::Utc>,
) {
    let mut images = items.get_image_infos(id).await.expect("images");
    images.retain(|i| i.image_type != ImageType::Primary);
    images.insert(
        0,
        ferrofin_traits::options::ItemImageInfo {
            path: path.to_owned(),
            image_type: ImageType::Primary,
            date_modified,
            width: 0,
            height: 0,
            blur_hash: None,
        },
    );
    persistence
        .save_item_images(id, &images)
        .await
        .expect("save images");
}

/// Jellyfin's art folder for `id` under the test's data root
/// (`GetInternalMetadataPath`): `{metadata}/library/{xx}/{id:N}`.
fn jellyfin_art_dir(root: &Path, id: uuid::Uuid) -> std::path::PathBuf {
    let simple = id.simple().to_string();
    root.join("metadata")
        .join("library")
        .join(&simple[..2])
        .join(&simple)
}

/// A movie library whose NFO declares the genre "Crime", scanned once: the
/// genre's collage is drawn from the poster's colour.
struct GenreLibrary {
    tmp: tempfile::TempDir,
    poster: std::path::PathBuf,
    scanner: LibraryScanner,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
}

impl GenreLibrary {
    async fn scanned() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let media = tmp.path().join("movies");
        write_movie_library(&media);
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
            persistence.clone(),
        )
        .with_items(Arc::clone(&items))
        .with_image_processor(processor)
        .with_metadata_dir(tmp.path().join("metadata").join("library"));
        scanner.scan_all().await.expect("scan");
        Self {
            poster: media.join("Heat (1995)").join("poster.png"),
            tmp,
            scanner,
            items,
            persistence,
        }
    }

    async fn genre(&self) -> uuid::Uuid {
        let row = self
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Genre],
                name: Some("Crime".to_owned()),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("query")
            .into_iter()
            .next()
            .expect("the genre");
        uuid::Uuid::parse_str(&row.id).expect("id")
    }

    async fn primary_colour(&self, id: uuid::Uuid) -> [u8; 3] {
        image::open(primary_of(&self.items, id).await.path)
            .expect("decodable")
            .to_rgb8()
            .get_pixel(10, 10)
            .0
    }
}

/// gl-591: an image Jellyfin drew lives in its own sharded folder
/// (`{metadata}/library/{xx}/{id:N}`, `GetInternalMetadataPath`); on an adopted
/// database it is still the genre's own, so a full refresh redraws it as it
/// would one Ferrofin drew (`FetchAsync`), from the poster's new colour.
#[tokio::test(flavor = "multi_thread")]
async fn a_genre_image_jellyfin_drew_is_redrawn_as_its_own() {
    let lib = GenreLibrary::scanned().await;
    let genre = lib.genre().await;
    let drawn = primary_of(&lib.items, genre).await;
    let dir = jellyfin_art_dir(lib.tmp.path(), genre);
    std::fs::create_dir_all(&dir).expect("jellyfin art dir");
    let adopted = dir.join("primary.png");
    std::fs::copy(&drawn.path, &adopted).expect("copy");
    set_primary_of(
        &lib.items,
        &lib.persistence,
        genre,
        &adopted.to_string_lossy(),
        drawn.date_modified,
    )
    .await;
    image::RgbImage::from_pixel(40, 60, image::Rgb(BLUE))
        .save(&lib.poster)
        .expect("poster");

    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    lib.scanner
        .scan_with(None, &full)
        .await
        .expect("full rescan");

    assert_eq!(lib.primary_colour(genre).await, BLUE);
}

/// gl-591: `ValidateImages` drops an image row whose file is gone (an adopted
/// database moved to a new host without its metadata folder), so the genre
/// has no Primary and the next scan draws one.
#[tokio::test(flavor = "multi_thread")]
async fn a_genre_whose_image_file_is_gone_is_drawn_again() {
    let lib = GenreLibrary::scanned().await;
    let genre = lib.genre().await;
    let gone = jellyfin_art_dir(lib.tmp.path(), genre).join("primary.png");
    set_primary_of(
        &lib.items,
        &lib.persistence,
        genre,
        &gone.to_string_lossy(),
        chrono::Utc::now(),
    )
    .await;

    lib.scanner.scan_all().await.expect("rescan");

    assert_eq!(lib.primary_colour(genre).await, RED);
}

/// gl-591: the library tile is never drawn over a Primary that is not the
/// library's own (`HasChanged` / `FetchAsync`), here a remote one, even when
/// the tile file it would have drawn is missing.
#[tokio::test(flavor = "multi_thread")]
async fn a_library_tile_is_never_drawn_over_a_remote_primary() {
    let lib = PhotoLibrary::scanned(RED).await;
    let library =
        uuid::Uuid::parse_str(&lib.row(BaseItemKind::CollectionFolder, "Photos").await.id)
            .expect("id");
    let tile = primary_of(&lib.items, library).await;
    std::fs::remove_file(&tile.path).expect("remove the drawn tile");
    let remote = "https://images.example.test/photos.jpg";
    set_primary_of(
        &lib.items,
        &lib.persistence,
        library,
        remote,
        chrono::Utc::now(),
    )
    .await;

    lib.scanner.scan_all().await.expect("rescan");

    assert_eq!(primary_of(&lib.items, library).await.path, remote);
}

/// gl-591: a library tile Jellyfin drew into its own sharded folder is the
/// library's own: a Default rescan leaves it while it is current, as it would
/// one Ferrofin drew, and a forced one redraws it.
#[rstest::rstest]
#[case::default_keeps_a_current_tile(MetadataRefreshMode::Default, RED)]
#[case::forced_redraws(MetadataRefreshMode::FullRefresh, BLUE)]
#[tokio::test(flavor = "multi_thread")]
async fn a_library_tile_jellyfin_drew_is_its_own(
    #[case] mode: MetadataRefreshMode,
    #[case] want: [u8; 3],
) {
    let lib = PhotoLibrary::scanned(RED).await;
    let library =
        uuid::Uuid::parse_str(&lib.row(BaseItemKind::CollectionFolder, "Photos").await.id)
            .expect("id");
    let drawn = primary_of(&lib.items, library).await;
    let dir = jellyfin_art_dir(lib.tmp.path(), library);
    std::fs::create_dir_all(&dir).expect("jellyfin art dir");
    let adopted = dir.join("folder.png");
    std::fs::copy(&drawn.path, &adopted).expect("copy");
    std::fs::remove_file(&drawn.path).expect("remove Ferrofin's copy");
    set_primary_of(
        &lib.items,
        &lib.persistence,
        library,
        &adopted.to_string_lossy(),
        drawn.date_modified,
    )
    .await;
    write_photo(&lib.album_dir, BLUE);

    let options = MetadataRefreshOptions {
        metadata_refresh_mode: mode,
        image_refresh_mode: mode,
        ..MetadataRefreshOptions::default()
    };
    lib.scanner.scan_with(None, &options).await.expect("rescan");

    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        dimmed(want)
    );
}

/// gl-591: a library whose tile file is gone loses the row (`ValidateImages`)
/// and is drawn again.
#[tokio::test(flavor = "multi_thread")]
async fn a_library_whose_tile_file_is_gone_is_drawn_again() {
    let lib = PhotoLibrary::scanned(RED).await;
    let library =
        uuid::Uuid::parse_str(&lib.row(BaseItemKind::CollectionFolder, "Photos").await.id)
            .expect("id");
    let gone = jellyfin_art_dir(lib.tmp.path(), library).join("folder.png");
    set_primary_of(
        &lib.items,
        &lib.persistence,
        library,
        &gone.to_string_lossy(),
        chrono::Utc::now(),
    )
    .await;

    lib.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        dimmed(RED)
    );
}

/// gl-598: a library's uploaded image is saved in the library's own folder
/// (`CollectionFolder.IsSaveLocalMetadataEnabled`), so it moves with a rename,
/// and the renamed library reads it back from there (`LocalImageProvider` on
/// its `ContainingFolderPath`, then `MergeImages`). It is not the library's
/// own drawing, so even a forced refresh never draws the collage over it.
#[tokio::test(flavor = "multi_thread")]
async fn a_library_upload_survives_a_rename_and_a_forced_refresh() {
    let lib = PhotoLibrary::scanned(RED).await;
    let library =
        uuid::Uuid::parse_str(&lib.row(BaseItemKind::CollectionFolder, "Photos").await.id)
            .expect("id");
    lib.uploader()
        .await
        .save_image(library, &png(BLUE), "image/png", ImageType::Primary, None)
        .await
        .expect("upload");
    assert_eq!(
        primary_of(&lib.items, library).await.path,
        lib.tmp
            .path()
            .join("root/default/Photos/folder.png")
            .to_string_lossy()
    );

    lib.vf
        .rename_virtual_folder("Photos", "Pictures")
        .await
        .expect("rename");
    let full = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    lib.scanner.scan_with(None, &full).await.expect("rescan");

    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Pictures")
            .await,
        BLUE
    );
}

/// gl-598: an image already in a library's own folder (`root/default/<name>`,
/// where Jellyfin keeps an adopted library's `folder.jpg`) becomes the
/// library's image on the next pass — unless the library is locked, which runs
/// no image provider and keeps the tile its first scan drew (a thumb collage,
/// so under the collage's shadow).
#[rstest::rstest]
#[case::unlocked(false, BLUE)]
#[case::locked(true, dimmed(RED))]
#[tokio::test(flavor = "multi_thread")]
async fn a_librarys_own_folder_image_is_read_unless_it_is_locked(
    #[case] locked: bool,
    #[case] want: [u8; 3],
) {
    let lib = PhotoLibrary::scanned(RED).await;
    if locked {
        let mut row = lib.row(BaseItemKind::CollectionFolder, "Photos").await;
        row.is_locked = true;
        lib.persistence
            .save_items(std::slice::from_ref(&row))
            .await
            .expect("lock");
    }
    std::fs::write(
        lib.tmp.path().join("root/default/Photos/folder.png"),
        png(BLUE),
    )
    .expect("folder image");

    lib.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        want
    );
}

/// gl-788: `CollectionFolderLocalImageProvider` (`Order` 1) reads a library's
/// media locations after its own folder (`LocalImageProvider`, `Order` 0):
/// a `folder.png` at the media root becomes the library's image in place of
/// the collage, and one in the library's own folder wins over it, as
/// `MergeImages` takes the first image found of a type.
#[rstest::rstest]
#[case::media_root(false, BLUE)]
#[case::own_folder_first(true, RED)]
#[tokio::test(flavor = "multi_thread")]
async fn a_librarys_media_root_image_is_read_after_its_own_folder(
    #[case] own_folder: bool,
    #[case] want: [u8; 3],
) {
    let lib = PhotoLibrary::scanned(RED).await;
    std::fs::write(lib.tmp.path().join("photos/folder.png"), png(BLUE)).expect("media image");
    if own_folder {
        std::fs::write(
            lib.tmp.path().join("root/default/Photos/folder.png"),
            png(RED),
        )
        .expect("own image");
    }

    lib.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        want
    );
}

/// gl-788: the media locations are read without their subfolders
/// (`GetFiles(path, …, recursive: false)`), so a poster inside a media
/// subfolder never becomes the library's image; the tile stays its collage.
#[tokio::test(flavor = "multi_thread")]
async fn a_media_subfolder_image_is_not_the_librarys() {
    let lib = PhotoLibrary::scanned(RED).await;
    std::fs::write(lib.album_dir.join("poster.png"), png(BLUE)).expect("subfolder image");

    lib.scanner.scan_all().await.expect("rescan");

    let library =
        uuid::Uuid::parse_str(&lib.row(BaseItemKind::CollectionFolder, "Photos").await.id)
            .expect("id");
    let primary = primary_of(&lib.items, library).await.path;
    assert!(
        primary.ends_with("primary.png") && !primary.contains("Trip"),
        "the library keeps its collage: {primary}"
    );
}

/// gl-788: the media-location read names the library's own files as
/// upstream's `PopulatePrimaryImages` does, after the library folder
/// (`FileNameWithoutExtension`): a `Photos.png` at the media root of the
/// "Photos" library is its image.
#[tokio::test(flavor = "multi_thread")]
async fn a_media_root_image_named_after_the_library_is_read() {
    let lib = PhotoLibrary::scanned(RED).await;
    std::fs::write(lib.tmp.path().join("photos/Photos.png"), png(BLUE)).expect("named image");

    lib.scanner.scan_all().await.expect("rescan");

    assert_eq!(
        lib.primary_colour(BaseItemKind::CollectionFolder, "Photos")
            .await,
        BLUE
    );
}
