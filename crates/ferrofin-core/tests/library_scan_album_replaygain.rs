//! `AudioFileProber.FetchDataFromTags`' album half, over the real scanner: a
//! track's `REPLAYGAIN_ALBUM_GAIN` becomes its album's `NormalizationGain`
//! when the album has none (`if (audio.AlbumEntity is not null &&
//! !audio.AlbumEntity.NormalizationGain.HasValue)`), parsed as the track tag
//! is. The ffprobe seam is a fake that answers each file's raw tags.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// Answers each file's probe with the raw tags held for its file name.
#[derive(Default)]
struct Tagged {
    tags: Mutex<HashMap<String, HashMap<String, String>>>,
}

#[async_trait]
impl MediaEncoder for Tagged {
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
        Ok(MediaSourceInfo {
            path: request.media_source.path.clone(),
            run_time_ticks: Some(2_000_000_000),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Audio,
                codec: Some("flac".to_owned()),
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        let name = Path::new(request.media_source.path.as_deref().unwrap_or_default())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tags = self.tags.lock().unwrap().get(&name).cloned();
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
            raw_audio_tags: Some(tags.unwrap_or_default()),
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
        unreachable!("no frames")
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
    encoder: Arc<Tagged>,
    _tmp: tempfile::TempDir,
}

/// A music library holding `The Band/Album/` with one track per file name,
/// each probed with the `REPLAYGAIN_ALBUM_GAIN` tag given beside it (none
/// for `None`).
async fn harness(tracks: &[(&str, Option<&str>)]) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    let album: PathBuf = music.join("The Band").join("Album");
    std::fs::create_dir_all(&album).unwrap();
    let encoder = Arc::new(Tagged::default());
    for (name, gain) in tracks {
        std::fs::write(album.join(name), b"").unwrap();
        let mut tags = HashMap::from([("album".to_owned(), "Album".to_owned())]);
        if let Some(gain) = gain {
            tags.insert("replaygain_album_gain".to_owned(), (*gain).to_owned());
        }
        encoder
            .tags
            .lock()
            .unwrap()
            .insert((*name).to_owned(), tags);
    }
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Music",
        Some(CollectionTypeOptions::music),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: music.to_string_lossy().into_owned(),
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
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items)
    .with_probe(
        Arc::clone(&encoder) as Arc<dyn MediaEncoder>,
        Arc::new(FerrofinMediaStreamRepository::new(db.clone())) as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    );
    Harness {
        db,
        scanner,
        encoder,
        _tmp: tmp,
    }
}

impl Harness {
    async fn album_gain(&self) -> Option<f64> {
        sqlx::query_scalar(
            r#"SELECT "NormalizationGain" FROM "BaseItems" WHERE "Type" LIKE '%Audio.MusicAlbum'"#,
        )
        .fetch_one(self.db.pool())
        .await
        .expect("the scanned album")
    }
}

#[tokio::test]
async fn a_tagged_track_gives_its_album_the_album_gain() {
    let h = harness(&[
        ("01 Untagged.flac", None),
        ("02 Song.flac", Some("-7.25 dB")),
    ])
    .await;
    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.album_gain().await, Some(-7.25));
}

#[tokio::test]
async fn an_unparseable_album_gain_leaves_the_album_without_one() {
    let h = harness(&[("01 Song.flac", Some("loud"))]).await;
    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.album_gain().await, None);
}

#[tokio::test]
async fn an_album_with_a_gain_keeps_it() {
    let h = harness(&[("01 Song.flac", Some("-7.25 dB"))]).await;
    h.scanner.scan_all().await.expect("scan");
    sqlx::query(
        r#"UPDATE "BaseItems" SET "NormalizationGain" = -1.5 WHERE "Type" LIKE '%Audio.MusicAlbum'"#,
    )
    .execute(h.db.writer())
    .await
    .unwrap();
    h.encoder
        .tags
        .lock()
        .unwrap()
        .get_mut("01 Song.flac")
        .unwrap()
        .insert("replaygain_album_gain".to_owned(), "-9 dB".to_owned());

    h.scanner
        .scan_with(
            None,
            &MetadataRefreshOptions::for_item_refresh(
                MetadataRefreshMode::FullRefresh,
                MetadataRefreshMode::FullRefresh,
                false,
                false,
                false,
            ),
        )
        .await
        .expect("full refresh");

    assert_eq!(h.album_gain().await, Some(-1.5));
}

/// `FetchDataFromTags` runs for an unlocked track only
/// (`if (!audio.IsLocked)`), so a locked track's tag names no album gain.
#[tokio::test]
async fn a_locked_tracks_tag_gives_its_album_nothing() {
    let h = harness(&[("01 Song.flac", Some("-7.25 dB"))]).await;
    h.scanner.scan_all().await.expect("scan");
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Type" LIKE '%Audio.Audio'"#)
        .execute(h.db.writer())
        .await
        .unwrap();
    sqlx::query(
        r#"UPDATE "BaseItems" SET "NormalizationGain" = NULL WHERE "Type" LIKE '%Audio.MusicAlbum'"#,
    )
    .execute(h.db.writer())
    .await
    .unwrap();

    h.scanner
        .scan_with(
            None,
            &MetadataRefreshOptions::for_item_refresh(
                MetadataRefreshMode::FullRefresh,
                MetadataRefreshMode::FullRefresh,
                false,
                false,
                false,
            ),
        )
        .await
        .expect("full refresh");

    assert_eq!(h.album_gain().await, None);
}
