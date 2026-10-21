//! Jellyfin 12's `AudioFileProber` over the real scanner: an audiobook's probed
//! chapters are saved after its streams (`if (audio is AudioBook &&
//! mediaInfo.Chapters is { Length: > 0 }) _chapterManager.SaveChapters(...)`),
//! cut at the runtime. The ffprobe seam is a fake that answers with chapters.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaStreamRepository, FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::{ChapterInfo, MediaStream};
use ferrofin_model::media_info::MediaInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ChapterRepository as _, ItemRepository, MediaStreamRepository};
use uuid::Uuid;

/// A 10-second audio file with chapters at 0 s, 4 s and 12 s (past the end).
struct Chaptered;

#[async_trait]
impl MediaEncoder for Chaptered {
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
        let chapter = |seconds: i64, name: &str| ChapterInfo {
            start_position_ticks: seconds * 10_000_000,
            name: Some(name.to_owned()),
            ..ChapterInfo::default()
        };
        Ok(MediaSourceInfo {
            run_time_ticks: Some(100_000_000),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Audio,
                codec: Some("aac".to_owned()),
                ..MediaStream::default()
            }],
            chapters: vec![
                chapter(0, "Opening"),
                chapter(4, "Middle"),
                chapter(12, "Past the end"),
            ],
            ..MediaSourceInfo::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        assert!(
            request.extract_chapters,
            "an audiobook probe asks for chapters"
        );
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
            ..MediaInfo::default()
        })
    }
    async fn extract_audio_image(&self, _: &str, _: Option<i32>) -> Result<String, ServiceError> {
        Err(ServiceError::backend("no cover"))
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
        unreachable!("no chapter image is extracted from audio")
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

#[tokio::test]
async fn a_scanned_audiobook_keeps_its_probed_chapters() {
    let tmp = tempfile::tempdir().unwrap();
    let books = tmp.path().join("books");
    let book = books.join("Some Author").join("Some Book");
    std::fs::create_dir_all(&book).unwrap();
    std::fs::write(book.join("Some Book.m4b"), b"").unwrap();
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Books",
        Some(CollectionTypeOptions::books),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: books.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .unwrap();
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let chapters = Arc::new(FerrofinChapterRepository::new(db.clone()));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items)
    .with_probe(
        Arc::new(Chaptered) as Arc<dyn MediaEncoder>,
        Arc::new(FerrofinMediaStreamRepository::new(db.clone())) as Arc<dyn MediaStreamRepository>,
        Arc::clone(&chapters) as Arc<dyn ferrofin_traits::persistence::ChapterRepository>,
    );

    scanner.scan_all().await.expect("scan");

    let id: String =
        sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%.AudioBook'"#)
            .fetch_one(db.pool())
            .await
            .expect("the .m4b is an audiobook");
    let saved = chapters
        .get_chapters(Uuid::parse_str(&id).unwrap())
        .await
        .unwrap();
    assert_eq!(
        saved
            .iter()
            .map(|chapter| (chapter.name.as_deref(), chapter.start_position_ticks))
            .collect::<Vec<_>>(),
        [(Some("Opening"), 0), (Some("Middle"), 40_000_000)],
        "a chapter past the runtime is dropped"
    );
}
