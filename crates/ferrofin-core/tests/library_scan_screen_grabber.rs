//! `VideoImageProvider` ("Screen Grabber"), over the real scanner: a movie
//! with no artwork of its own gets a Primary grabbed from its default video
//! stream, 10 % into its runtime (10 s in when the runtime is unknown); an
//! unchanged rescan grabs nothing more, a failed grab is tried again, and
//! replacing all images grabs again; the library's "Screen Grabber"
//! checkbox and Primary limit gate it. The ffprobe/ffmpeg seam is a fake
//! that records each probe and grab.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaAttachmentRepository, FerrofinMediaStreamRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{ImageOption, LibraryOptions, MediaPathInfo, TypeOptions};
use ferrofin_model::drawing::ImageFormat;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// One frame grab: the container, the stream's index and the offset.
type Grab = (String, i32, Option<i64>);

/// A probe that reports a video with a subtitle stream before its video
/// stream (so the default video stream is not the first stream), or with no
/// video stream at all, and a grabber that writes `FRAME@<offset>` (or fails
/// while `fail` is set) and records each call.
struct Frames {
    scratch: std::path::PathBuf,
    run_time_ticks: Option<i64>,
    video: bool,
    fail: AtomicBool,
    probes: AtomicUsize,
    grabs: Mutex<Vec<Grab>>,
}

#[async_trait]
impl MediaEncoder for Frames {
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
        _request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        let stream_type = if self.video {
            MediaStreamType::Video
        } else {
            MediaStreamType::Audio
        };
        Ok(MediaSourceInfo {
            container: Some("matroska".to_owned()),
            run_time_ticks: self.run_time_ticks,
            media_streams: vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Subtitle,
                    codec: Some("subrip".to_owned()),
                    ..MediaStream::default()
                },
                MediaStream {
                    index: 1,
                    stream_type,
                    codec: Some("h264".to_owned()),
                    width: Some(1920),
                    height: Some(1080),
                    ..MediaStream::default()
                },
            ],
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(&self, _: &str, _: Option<i32>) -> Result<String, ServiceError> {
        unreachable!("no audio items in this library")
    }
    async fn extract_video_image(
        &self,
        _input_file: &str,
        container: &str,
        _media_source: &MediaSourceInfo,
        video_stream: &MediaStream,
        _threed_format: Option<Video3DFormat>,
        offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        let mut grabs = self.grabs.lock().unwrap();
        grabs.push((container.to_owned(), video_stream.index, offset_ticks));
        if self.fail.load(Ordering::SeqCst) {
            return Err(ServiceError::backend("ffmpeg produced no frame"));
        }
        let path = self.scratch.join(format!("grab-{}.jpg", grabs.len()));
        std::fs::write(&path, format!("FRAME@{}", offset_ticks.unwrap_or(-1))).unwrap();
        Ok(path.to_string_lossy().into_owned())
    }
    async fn extract_image_stream(
        &self,
        _: &str,
        _: &str,
        _: &MediaSourceInfo,
        _: i32,
        _: ImageFormat,
    ) -> Result<String, ServiceError> {
        unreachable!("the file has no embedded pictures")
    }
    fn get_input_argument(&self, input_file: &str, _: &MediaSourceInfo) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _: i64) -> String {
        String::new()
    }
    async fn convert_image(&self, _: &str, _: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

struct Harness {
    db: Database,
    scanner: LibraryScanner,
    encoder: Arc<Frames>,
    tmp: tempfile::TempDir,
}

async fn harness(options: LibraryOptions, run_time_ticks: Option<i64>) -> Harness {
    harness_with(options, run_time_ticks, true).await
}

async fn harness_with(
    options: LibraryOptions,
    run_time_ticks: Option<i64>,
    video: bool,
) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("movies");
    std::fs::create_dir_all(media.join("Heat (1995)")).unwrap();
    std::fs::write(media.join("Heat (1995)/Heat (1995).mkv"), b"").unwrap();
    let meta_root = tmp.path().join("metadata").join("library");
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
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
            ..options
        },
    )
    .await
    .unwrap();
    let encoder = Arc::new(Frames {
        scratch,
        run_time_ticks,
        video,
        fail: AtomicBool::new(false),
        probes: AtomicUsize::new(0),
        grabs: Mutex::new(Vec::new()),
    });
    let streams = Arc::new(FerrofinMediaStreamRepository::new(db.clone()));
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items)
    .with_probe(
        Arc::clone(&encoder) as Arc<dyn MediaEncoder>,
        streams as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    )
    .with_attachments(Arc::new(FerrofinMediaAttachmentRepository::new(db.clone())))
    .with_metadata_dir(meta_root);
    Harness {
        db,
        scanner,
        encoder,
        tmp,
    }
}

/// `(ImageType discriminant, file name, file content)` of the movie's images.
async fn images(h: &Harness) -> Vec<(i64, String, String)> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        r#"SELECT i."ImageType", i."Path" FROM "BaseItemImageInfos" i
           JOIN "BaseItems" b ON b."Id" = i."ItemId" WHERE b."Type" LIKE '%Movies.Movie'
           ORDER BY i."ImageType""#,
    )
    .fetch_all(h.db.pool())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(kind, path)| {
            let name = Path::new(&path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            (kind, name, content)
        })
        .collect()
}

/// `GetVideoImage`: `RunTimeTicks / 10` when the runtime is known, else
/// 10 s, from the stream at `DefaultVideoStreamIndex` (the probe's first
/// video stream, index 1 here).
#[rstest::rstest]
#[case::runtime_known(Some(6_000_000_000), 600_000_000)]
#[case::runtime_unknown(None, 100_000_000)]
#[tokio::test(flavor = "multi_thread")]
async fn a_video_without_artwork_gets_a_grabbed_primary(
    #[case] run_time_ticks: Option<i64>,
    #[case] offset: i64,
) {
    let h = harness(LibraryOptions::default(), run_time_ticks).await;
    h.scanner.scan_all().await.unwrap();
    assert_eq!(
        images(&h).await,
        [(0, "primary.jpg".to_owned(), format!("FRAME@{offset}"))]
    );
    assert_eq!(
        h.encoder.grabs.lock().unwrap().clone(),
        [("matroska".to_owned(), 1, Some(offset))]
    );
    // Nothing changed: the rescan keeps the Primary and grabs nothing.
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.encoder.grabs.lock().unwrap().len(), 1);
    assert_eq!(images(&h).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_library_checkbox_gates_the_grab() {
    // Screen Grabber unticked for movies: no frame is grabbed.
    let off = harness(
        LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("Movie".to_owned()),
                image_fetchers: vec!["Embedded Image Extractor".to_owned()],
                ..TypeOptions::default()
            }],
            ..LibraryOptions::default()
        },
        Some(6_000_000_000),
    )
    .await;
    off.scanner.scan_all().await.unwrap();
    let found = images(&off).await;
    assert!(found.is_empty(), "{found:?}");
    assert!(off.encoder.grabs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn local_artwork_leaves_nothing_to_grab() {
    let h = harness(LibraryOptions::default(), Some(6_000_000_000)).await;
    let folder = h.tmp.path().join("movies/Heat (1995)");
    std::fs::write(folder.join("poster.jpg"), b"POSTER").unwrap();
    h.scanner.scan_all().await.unwrap();
    assert_eq!(
        images(&h).await,
        [(0, "poster.jpg".to_owned(), "POSTER".to_owned())]
    );
    assert!(h.encoder.grabs.lock().unwrap().is_empty());
}

/// `RefreshFromProvider` counts a provider that threw (`result.Failures++`),
/// so the refresh is not stamped: the next scan grabs again, and once a
/// grab works the item is quiet.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_grab_stores_nothing_and_is_tried_again() {
    let h = harness(LibraryOptions::default(), Some(6_000_000_000)).await;
    h.encoder.fail.store(true, Ordering::SeqCst);
    h.scanner.scan_all().await.unwrap();
    let found = images(&h).await;
    assert!(found.is_empty(), "{found:?}");
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.encoder.grabs.lock().unwrap().len(), 2);
    h.encoder.fail.store(false, Ordering::SeqCst);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(images(&h).await.len(), 1);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.encoder.grabs.lock().unwrap().len(), 3);
}

/// `savedOptions.IsEnabled(Primary)`: a Primary limit of 0 grabs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_primary_limit_of_zero_grabs_nothing() {
    let h = harness(
        LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("Movie".to_owned()),
                image_fetchers: vec!["Screen Grabber".to_owned()],
                image_options: vec![ImageOption {
                    type_: ImageType::Primary,
                    limit: 0,
                    min_width: 0,
                }],
                ..TypeOptions::default()
            }],
            ..LibraryOptions::default()
        },
        Some(6_000_000_000),
    )
    .await;
    h.scanner.scan_all().await.unwrap();
    let found = images(&h).await;
    assert!(found.is_empty(), "{found:?}");
    assert!(h.encoder.grabs.lock().unwrap().is_empty());
}

/// `GetImage`: a file with no video stream has no `DefaultVideoStreamIndex`.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_without_a_video_stream_grabs_nothing() {
    let h = harness_with(LibraryOptions::default(), Some(6_000_000_000), false).await;
    h.scanner.scan_all().await.unwrap();
    let found = images(&h).await;
    assert!(found.is_empty(), "{found:?}");
    assert!(h.encoder.grabs.lock().unwrap().is_empty());
}

/// Replacing all images runs the dynamic providers again
/// (`IsReplacingImage`), so the frame is grabbed again. An image full
/// refresh with the default metadata mode does not re-probe the unchanged
/// file, so the grab reads the stored streams, as `GetMediaStreams` does.
#[tokio::test(flavor = "multi_thread")]
async fn replacing_all_images_grabs_again_from_the_stored_streams() {
    let h = harness(LibraryOptions::default(), Some(6_000_000_000)).await;
    h.scanner.scan_all().await.unwrap();
    let probes = h.encoder.probes.load(Ordering::SeqCst);
    let replace = MetadataRefreshOptions::for_item_refresh(
        MetadataRefreshMode::Default,
        MetadataRefreshMode::FullRefresh,
        false,
        true,
        false,
    );
    h.scanner.scan_with(None, &replace).await.unwrap();
    assert_eq!(
        h.encoder.probes.load(Ordering::SeqCst),
        probes,
        "no re-probe"
    );
    assert_eq!(
        h.encoder.grabs.lock().unwrap().clone(),
        [
            ("matroska".to_owned(), 1, Some(600_000_000)),
            ("matroska".to_owned(), 1, Some(600_000_000))
        ]
    );
    assert_eq!(
        images(&h).await,
        [(0, "primary.jpg".to_owned(), "FRAME@600000000".to_owned())]
    );
}
