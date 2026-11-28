//! What a library scan stores as a music album's and a track's Primary, as
//! Jellyfin does (gl-692, gl-1633):
//!
//! - a track has a Primary only when its own file embeds a picture
//!   (`AudioImageProvider.GetImage`), extracted once per album and copied into
//!   each such track's own art folder; a track without one has none, and
//!   clients show its album's;
//! - an album with a `folder.jpg` beside it names that file in place
//!   (`LocalImageProvider`); one without takes a copy of its first track's
//!   cover in its own art folder (`MusicAlbumImageProvider`);
//! - nothing goes to the shared `album-covers` folder earlier builds wrote,
//!   and a row such a build stored there is replaced by the next scan, its
//!   file left on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaStreamRepository, FerrofinServerApplicationPaths,
    FerrofinServerConfigurationManager, FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType, MediaStreamType};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_providers::image_save::ImageFileSaver;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::options::{InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

/// A probe whose tracks named `01 …`, `03 …` or `04 …` embed a picture,
/// tagged with their folder's album (`04 …` with an album of its own) and one
/// album artist, and whose extraction writes a 6x6 picture coloured by album,
/// counting its runs.
struct Probe {
    extractions: AtomicUsize,
    probes: AtomicUsize,
}

fn embeds_picture(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| ["01", "03", "04"].iter().any(|n| name.starts_with(n)))
}

#[async_trait]
impl MediaEncoder for Probe {
    fn encoder_path(&self) -> String {
        "ffmpeg".to_owned()
    }
    fn probe_path(&self) -> String {
        "ffprobe".to_owned()
    }
    async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_media_info(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        let mut streams = vec![MediaStream {
            index: 0,
            stream_type: MediaStreamType::Audio,
            ..Default::default()
        }];
        if embeds_picture(request.media_source.path.as_deref().unwrap_or_default()) {
            streams.push(MediaStream {
                index: 1,
                stream_type: MediaStreamType::EmbeddedImage,
                ..Default::default()
            });
        }
        // A runtime, so a rescan finds the media info complete and probes
        // no unchanged file again (`IsMissingMediaInfo`).
        Ok(MediaSourceInfo {
            media_streams: streams,
            run_time_ticks: Some(20_000_000),
            ..Default::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        let path = request.media_source.path.clone().unwrap_or_default();
        let album = if path.contains("04 Single") {
            "A Single"
        } else if path.contains("Folder Album") {
            "Folder Album"
        } else {
            "Embedded Album"
        };
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
            album: Some(album.to_owned()),
            album_artists: vec!["Artist".to_owned()],
            ..Default::default()
        })
    }
    async fn extract_audio_image(
        &self,
        path: &str,
        _image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        self.extractions.fetch_add(1, Ordering::SeqCst);
        let out = format!("{path}.image.png");
        let rgb = if path.contains("04 Single") {
            [200, 0, 200]
        } else if path.contains("Folder Album") {
            [0, 160, 0]
        } else {
            [0, 0, 200]
        };
        image::RgbImage::from_pixel(6, 6, image::Rgb(rgb))
            .save(&out)
            .unwrap();
        Ok(out)
    }
    async fn extract_video_image(
        &self,
        _input_file: &str,
        _container: &str,
        _media_source: &MediaSourceInfo,
        _video_stream: &MediaStream,
        _threed_format: Option<ferrofin_model::entities::Video3DFormat>,
        _offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        unreachable!("a music library grabs no video frame")
    }
    fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _ticks: i64) -> String {
        String::new()
    }
    async fn convert_image(&self, _input: &str, _output: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

struct Library {
    scanner: LibraryScanner,
    items: Arc<dyn ItemRepository>,
    store: Arc<FerrofinItemPersistenceService>,
    probe: Arc<Probe>,
    media: PathBuf,
    meta: PathBuf,
}

/// "Folder Album" (a red `folder.jpg`, a track with a picture, one without)
/// and "Embedded Album" (two tracks with a picture, one without, and one
/// with a picture tagged with another album), scanned by
/// a scanner with an image processor and the artwork saver, under `root`, in
/// a library that saves artwork into its media folders when `save_local`.
async fn music_library(root: &Path, save_local: bool) -> Library {
    let media = root.join("music").join("Artist");
    let folder = media.join("Folder Album");
    let embedded = media.join("Embedded Album");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(&embedded).unwrap();
    image::RgbImage::from_pixel(8, 8, image::Rgb([200, 0, 0]))
        .save(folder.join("folder.jpg"))
        .unwrap();
    for file in ["01 Pic.flac", "02 Plain.flac"] {
        std::fs::write(folder.join(file), b"audio").unwrap();
    }
    for file in [
        "01 Pic.flac",
        "02 Plain.flac",
        "03 Pic.flac",
        "04 Single.flac",
    ] {
        std::fs::write(embedded.join(file), b"audio").unwrap();
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
                path: root.join("music").to_string_lossy().into_owned(),
            }],
            save_local_metadata: save_local,
            ..LibraryOptions::default()
        },
    )
    .await
    .unwrap();
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let probe = Arc::new(Probe {
        extractions: AtomicUsize::new(0),
        probes: AtomicUsize::new(0),
    });
    let meta = root.join("metadata");
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root,
        root.join("log"),
        root.join("server-config"),
        root.join("cache"),
        root.join("web"),
    ));
    let saver = Arc::new(ImageFileSaver::new(
        items.clone(),
        vf.clone(),
        Arc::new(
            FerrofinServerConfigurationManager::load(paths)
                .await
                .unwrap(),
        ),
        meta.clone().into(),
    ));
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), store.clone())
        .with_items(items.clone())
        .with_metadata_dir(meta.clone())
        .with_image_processor(Arc::new(ferrofin_drawing::ImageProcessor::new(
            Arc::new(ferrofin_drawing::ImageCrateEncoder::new()),
            root.join("image-cache"),
        )))
        .with_probe(
            probe.clone(),
            Arc::new(FerrofinMediaStreamRepository::new(db.clone())),
            Arc::new(FerrofinChapterRepository::new(db)),
        )
        .with_image_saver(saver);
    Library {
        scanner,
        items,
        store,
        probe,
        media,
        meta,
    }
}

impl Library {
    /// The id of the item of `kind` at `path`.
    async fn id(&self, kind: BaseItemKind, path: &Path) -> Uuid {
        let rows = self
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![kind],
                recursive: true,
                ..InternalItemsQuery::default()
            })
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|row| row.path.as_deref() == Some(path.to_str().unwrap()))
            .unwrap_or_else(|| panic!("no {kind:?} at {}", path.display()));
        Uuid::parse_str(&row.id).unwrap()
    }

    async fn images(&self, id: Uuid) -> Vec<ItemImageInfo> {
        self.items.get_image_infos(id).await.unwrap()
    }

    /// The item's only image, its Primary.
    async fn primary(&self, id: Uuid) -> ItemImageInfo {
        let images = self.images(id).await;
        assert_eq!(images.len(), 1, "{images:#?}");
        assert_eq!(images[0].image_type, ImageType::Primary);
        images.into_iter().next().unwrap()
    }

    /// The colour of an image file's first pixel, read by its content (an
    /// extracted cover is stored as `.jpg` whatever its format).
    fn colour(path: &str) -> [u8; 3] {
        image::ImageReader::open(path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .to_rgb8()
            .get_pixel(0, 0)
            .0
    }
}

/// The scanned library's albums and tracks, by name.
struct Ids {
    folder_album: Uuid,
    embedded_album: Uuid,
    folder_pic: Uuid,
    folder_plain: Uuid,
    embedded_pic: Uuid,
    embedded_plain: Uuid,
    embedded_third: Uuid,
    embedded_single: Uuid,
}

impl Library {
    fn folder_dir(&self) -> PathBuf {
        self.media.join("Folder Album")
    }

    fn embedded_dir(&self) -> PathBuf {
        self.media.join("Embedded Album")
    }

    /// The library after one scan.
    async fn scanned(root: &Path) -> (Self, Ids) {
        Self::scanned_saving(root, false).await
    }

    /// The library after one scan, saving artwork into its media folders
    /// when `save_local`.
    async fn scanned_saving(root: &Path, save_local: bool) -> (Self, Ids) {
        let lib = music_library(root, save_local).await;
        Box::pin(lib.scanner.scan_all()).await.unwrap();
        let (folder, embedded) = (lib.folder_dir(), lib.embedded_dir());
        let ids = Ids {
            folder_album: lib.id(BaseItemKind::MusicAlbum, &folder).await,
            embedded_album: lib.id(BaseItemKind::MusicAlbum, &embedded).await,
            folder_pic: lib
                .id(BaseItemKind::Audio, &folder.join("01 Pic.flac"))
                .await,
            folder_plain: lib
                .id(BaseItemKind::Audio, &folder.join("02 Plain.flac"))
                .await,
            embedded_pic: lib
                .id(BaseItemKind::Audio, &embedded.join("01 Pic.flac"))
                .await,
            embedded_plain: lib
                .id(BaseItemKind::Audio, &embedded.join("02 Plain.flac"))
                .await,
            embedded_third: lib
                .id(BaseItemKind::Audio, &embedded.join("03 Pic.flac"))
                .await,
            embedded_single: lib
                .id(BaseItemKind::Audio, &embedded.join("04 Single.flac"))
                .await,
        };
        (lib, ids)
    }

    /// The item's own art folder, Jellyfin's `{xx}/{idN}`.
    fn own_folder(&self, id: Uuid) -> PathBuf {
        let simple = id.simple().to_string();
        self.meta.join(&simple[..2]).join(&simple)
    }
}

// gl-692. A multi-thread runtime, as the server runs the scan on.
#[tokio::test(flavor = "multi_thread")]
async fn a_track_has_a_primary_only_from_its_own_picture() {
    let root = tempfile::tempdir().unwrap();
    let (lib, ids) = Library::scanned(root.path()).await;

    // A track without a picture has no image, whatever its album has.
    assert_eq!(
        lib.images(ids.folder_plain).await,
        [],
        "folder_plain has no image"
    );
    assert_eq!(
        lib.images(ids.embedded_plain).await,
        [],
        "embedded_plain has no image"
    );
    // A track with one has that picture, not its album's folder image, in
    // its own art folder, measured.
    let pic = lib.primary(ids.folder_pic).await;
    assert!(
        Path::new(&pic.path).starts_with(lib.own_folder(ids.folder_pic)),
        "{pic:?}"
    );
    assert_eq!(Library::colour(&pic.path), [0, 160, 0]);
    assert_eq!((pic.width, pic.height), (6, 6));
    assert!(pic.blur_hash.is_some());
    // An album's tracks with a picture and the same album tag and artist
    // each have their own copy of one extraction, measured alike; one tagged
    // with another album has its own picture.
    let first = lib.primary(ids.embedded_pic).await;
    let third = lib.primary(ids.embedded_third).await;
    assert!(Path::new(&first.path).starts_with(lib.own_folder(ids.embedded_pic)));
    assert!(Path::new(&third.path).starts_with(lib.own_folder(ids.embedded_third)));
    assert_eq!(Library::colour(&third.path), [0, 0, 200]);
    assert_eq!(
        (third.width, third.height, &third.blur_hash),
        (first.width, first.height, &first.blur_hash)
    );
    let single = lib.primary(ids.embedded_single).await;
    assert!(Path::new(&single.path).starts_with(lib.own_folder(ids.embedded_single)));
    assert_eq!(Library::colour(&single.path), [200, 0, 200]);
    assert_eq!(
        lib.probe.extractions.load(Ordering::SeqCst),
        3,
        "one extraction per album and tag"
    );
    assert!(!lib.meta.join("album-covers").exists());

    // A rescan extracts nothing and changes nothing.
    let before = (
        lib.images(ids.folder_pic).await,
        lib.images(ids.embedded_third).await,
    );
    Box::pin(lib.scanner.scan_all()).await.unwrap();
    assert_eq!(lib.probe.extractions.load(Ordering::SeqCst), 3);
    assert_eq!(
        (
            lib.images(ids.folder_pic).await,
            lib.images(ids.embedded_third).await,
        ),
        before
    );
    assert_eq!(
        lib.images(ids.folder_plain).await,
        [],
        "folder_plain has no image"
    );
}

// gl-1633.
#[tokio::test(flavor = "multi_thread")]
async fn an_album_names_its_folder_image_or_a_copy_of_its_first_tracks_cover() {
    let root = tempfile::tempdir().unwrap();
    let (lib, ids) = Library::scanned(root.path()).await;

    // The folder image is the album's Primary in place.
    let album = lib.primary(ids.folder_album).await;
    assert_eq!(Path::new(&album.path), lib.folder_dir().join("folder.jpg"));
    assert_eq!((album.width, album.height), (8, 8));
    assert!(album.blur_hash.is_some());
    // An album without one takes a copy of its first track's cover in its
    // own art folder, never beside its media.
    let copy = lib.primary(ids.embedded_album).await;
    assert!(
        Path::new(&copy.path).starts_with(lib.own_folder(ids.embedded_album)),
        "{copy:?}"
    );
    assert_eq!(Library::colour(&copy.path), [0, 0, 200]);
    assert!(copy.blur_hash.is_some());
    assert!(!lib.embedded_dir().join("folder.jpg").exists());
    assert!(!lib.meta.join("album-covers").exists());

    // A rescan keeps both.
    Box::pin(lib.scanner.scan_all()).await.unwrap();
    assert_eq!(lib.primary(ids.folder_album).await, album);
    assert_eq!(lib.primary(ids.embedded_album).await, copy);
}

// gl-593: the copy is the last of the album's providers, after the music
// pass, so the stamp that pass earns waits for it: a full refresh whose copy
// fails leaves the album as last refreshed (`RunCustomProvider` counts the
// failure), and the next one, drawn, stamps it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_album_copy_withholds_the_albums_refresh_stamp() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let (lib, ids) = Library::scanned(root.path()).await;
    let stamp = || async {
        lib.items
            .retrieve_item(ids.embedded_album)
            .await
            .unwrap()
            .unwrap()
            .date_last_refreshed
    };
    let first = stamp().await.expect("the first scan stamped it");
    let copy = PathBuf::from(lib.primary(ids.embedded_album).await.path);
    let full = ferrofin_traits::providers::MetadataRefreshOptions {
        metadata_refresh_mode: ferrofin_traits::providers::MetadataRefreshMode::FullRefresh,
        image_refresh_mode: ferrofin_traits::providers::MetadataRefreshMode::FullRefresh,
        ..Default::default()
    };

    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().write(true).open(&copy).is_ok() {
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("skipped: a read-only file is writable here (running as root?)");
        return;
    }
    let failed = Box::pin(lib.scanner.scan_with(None, &full)).await;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o644)).unwrap();
    failed.unwrap();
    assert_eq!(
        stamp().await,
        Some(first),
        "a failed copy withholds the stamp"
    );

    Box::pin(lib.scanner.scan_with(None, &full)).await.unwrap();
    assert!(stamp().await.expect("stamped") > first);
}

// gl-1633: `MusicAlbumImageProvider` saves with `saveLocallyWithMedia: false`,
// so an album's copy stays in its own folder when the library saves artwork
// into its media folders, on the scan that draws it and on every one after.
#[tokio::test(flavor = "multi_thread")]
async fn an_albums_copy_is_never_written_beside_its_media() {
    let root = tempfile::tempdir().unwrap();
    let (lib, ids) = Library::scanned_saving(root.path(), true).await;
    let copy = lib.primary(ids.embedded_album).await;
    assert!(
        Path::new(&copy.path).starts_with(lib.own_folder(ids.embedded_album)),
        "{copy:?}"
    );
    for _ in 0..2 {
        Box::pin(lib.scanner.scan_all()).await.unwrap();
        assert_eq!(lib.primary(ids.embedded_album).await.path, copy.path);
        assert!(!lib.embedded_dir().join("folder.jpg").exists());
    }
}

// Rows an earlier build stored: an album and every track naming one shared
// copy, and no file of the tracks' or the album's own. The next scan, which
// probes no file again, replaces them from the stored streams; the shared
// file stays.
#[tokio::test(flavor = "multi_thread")]
async fn rows_naming_an_earlier_builds_shared_cover_are_replaced_by_the_next_scan() {
    let root = tempfile::tempdir().unwrap();
    let (lib, ids) = Library::scanned(root.path()).await;
    let album = lib.primary(ids.folder_album).await;
    let pic = lib.primary(ids.folder_pic).await;
    let first = lib.primary(ids.embedded_pic).await;
    let copy = lib.primary(ids.embedded_album).await;

    let shared_dir = lib
        .meta
        .join("album-covers")
        .join(guid_to_db(ids.folder_album));
    std::fs::create_dir_all(&shared_dir).unwrap();
    let shared = shared_dir.join("0123abcd.jpg");
    std::fs::copy(lib.folder_dir().join("folder.jpg"), &shared).unwrap();
    let shared_row = ItemImageInfo {
        path: shared.to_string_lossy().into_owned(),
        ..album.clone()
    };
    for id in [
        ids.folder_album,
        ids.folder_plain,
        ids.folder_pic,
        ids.embedded_album,
        ids.embedded_pic,
        ids.embedded_plain,
        ids.embedded_third,
    ] {
        lib.store
            .save_item_images(id, std::slice::from_ref(&shared_row))
            .await
            .unwrap();
    }
    for own in [&pic.path, &first.path, &copy.path] {
        std::fs::remove_file(own).unwrap();
    }
    let third = lib.own_folder(ids.embedded_third).join("poster.jpg");
    std::fs::remove_file(&third).unwrap();
    let probes = lib.probe.probes.load(Ordering::SeqCst);
    Box::pin(lib.scanner.scan_all()).await.unwrap();
    assert_eq!(
        lib.probe.probes.load(Ordering::SeqCst),
        probes,
        "the rescan probes no file again"
    );

    assert_eq!(lib.primary(ids.folder_album).await.path, album.path);
    assert_eq!(
        lib.images(ids.folder_plain).await,
        [],
        "folder_plain has no image"
    );
    assert_eq!(
        lib.images(ids.embedded_plain).await,
        [],
        "embedded_plain has no image"
    );
    let pic = lib.primary(ids.folder_pic).await;
    assert!(Path::new(&pic.path).starts_with(lib.own_folder(ids.folder_pic)));
    assert_eq!(Library::colour(&pic.path), [0, 160, 0]);
    assert!(third.is_file());
    let copy = lib.primary(ids.embedded_album).await;
    assert!(Path::new(&copy.path).starts_with(lib.own_folder(ids.embedded_album)));
    assert_eq!(Library::colour(&copy.path), [0, 0, 200]);
    assert_eq!(
        lib.probe.extractions.load(Ordering::SeqCst),
        5,
        "one extraction per album and tag again, from the stored streams"
    );
    assert!(shared.is_file(), "an earlier build's shared file is kept");
}
