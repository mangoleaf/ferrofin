//! `EmbeddedImageProvider` for videos, over the real scanner: a movie whose
//! file carries labelled embedded pictures and an attached logo gets a Primary,
//! a Backdrop and a Logo from them, an unchanged rescan extracts nothing more,
//! and the library's "Embedded Image Extractor" checkbox and per-type limits
//! gate it. The ffprobe/ffmpeg seam is a fake that records each extraction.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaAttachmentRepository, FerrofinMediaStreamRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, TypeOptions};
use ferrofin_model::drawing::ImageFormat;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::{MediaAttachment, MediaStream};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};

/// A probe that reports a video with two embedded pictures and an attached
/// logo, and an extractor that writes `IMG-<index>` and records its calls.
struct Pictures {
    scratch: std::path::PathBuf,
    extractions: Mutex<Vec<(i32, ImageFormat)>>,
}

#[async_trait]
impl MediaEncoder for Pictures {
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
        let picture = |index, comment: &str, codec: &str| MediaStream {
            index,
            stream_type: MediaStreamType::EmbeddedImage,
            codec: Some(codec.to_owned()),
            comment: Some(comment.to_owned()),
            ..MediaStream::default()
        };
        Ok(MediaSourceInfo {
            run_time_ticks: Some(30_000_000),
            media_streams: vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("h264".to_owned()),
                    width: Some(1920),
                    height: Some(1080),
                    ..MediaStream::default()
                },
                picture(2, "Cover", "mjpeg"),
                picture(3, "Backdrop art", "png"),
            ],
            media_attachments: vec![MediaAttachment {
                index: 4,
                file_name: Some("Studio-LOGO.png".to_owned()),
                mime_type: Some("image/png".to_owned()),
                ..MediaAttachment::default()
            }],
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(&self, _: &str, _: Option<i32>) -> Result<String, ServiceError> {
        unreachable!("no audio items in this library")
    }
    async fn extract_video_image(
        &self,
        _: &str,
        _: &str,
        _: &MediaSourceInfo,
        _: &MediaStream,
        _: Option<Video3DFormat>,
        _: Option<i64>,
    ) -> Result<String, ServiceError> {
        unreachable!("no frame extraction in a scan")
    }
    async fn extract_image_stream(
        &self,
        _input_file: &str,
        _container: &str,
        _media_source: &MediaSourceInfo,
        stream_index: i32,
        format: ImageFormat,
    ) -> Result<String, ServiceError> {
        let mut calls = self.extractions.lock().unwrap();
        let path = self
            .scratch
            .join(format!("extract-{}{}", calls.len(), format.extension()));
        std::fs::write(&path, format!("IMG-{stream_index}")).unwrap();
        calls.push((stream_index, format));
        Ok(path.to_string_lossy().into_owned())
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
    encoder: Arc<Pictures>,
    meta_root: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

async fn harness(options: LibraryOptions) -> Harness {
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
    let encoder = Arc::new(Pictures {
        scratch,
        extractions: Mutex::new(Vec::new()),
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
    .with_metadata_dir(meta_root.clone());
    Harness {
        db,
        scanner,
        encoder,
        meta_root,
        _tmp: tmp,
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

#[tokio::test(flavor = "multi_thread")]
async fn a_video_gets_primary_backdrop_and_logo_from_its_embedded_pictures() {
    let h = harness(LibraryOptions::default()).await;
    h.scanner.scan_all().await.unwrap();
    // Primary: the first picture labelled like a poster ("Cover", mjpeg → jpg);
    // Backdrop: the stream commented "Backdrop art" (png); Logo: no stream
    // matches, so the attachment whose name contains "logo" (png).
    let stored = images(&h).await;
    assert_eq!(
        stored,
        [
            (0, "primary.jpg".to_owned(), "IMG-2".to_owned()),
            (2, "backdrop.png".to_owned(), "IMG-3".to_owned()),
            (4, "logo.png".to_owned(), "IMG-4".to_owned()),
        ],
        "{stored:?} under {}",
        h.meta_root.display()
    );
    let calls = h.encoder.extractions.lock().unwrap().clone();
    assert_eq!(
        calls,
        [
            (2, ImageFormat::Jpg),
            (3, ImageFormat::Png),
            (4, ImageFormat::Png)
        ]
    );
    drop(calls);
    // Nothing changed: the rescan keeps the images and extracts nothing.
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.encoder.extractions.lock().unwrap().len(), 3);
    assert_eq!(images(&h).await.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_library_checkbox_and_type_limits_gate_the_extraction() {
    // Embedded Image Extractor unticked for movies: nothing is extracted.
    let off = harness(LibraryOptions {
        type_options: vec![TypeOptions {
            type_: Some("Movie".to_owned()),
            image_fetchers: vec!["TheMovieDb".to_owned()],
            ..TypeOptions::default()
        }],
        ..LibraryOptions::default()
    })
    .await;
    off.scanner.scan_all().await.unwrap();
    let found = images(&off).await;
    assert!(found.is_empty(), "{found:?}");
    assert!(off.encoder.extractions.lock().unwrap().is_empty());
}
