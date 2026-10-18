//! The library scan's lyric discovery, end to end: an audio track's lyric
//! sidecars become its `Lyric` stream and its `LyricFiles`, as Jellyfin's
//! `AudioFileProber.AddExternalLyrics` records them, and a change to that set
//! re-probes the track (`ProbeProvider.HasChanged`'s lyric arm).
//!
//! Port target: `LyricResolver` (`MediaInfoResolver.GetExternalFiles(Audio)`:
//! a case-insensitive prefix match on the track's stem plus a media-flag
//! delimiter, over the track's folder and its internal metadata folder) as
//! run from `AudioFileProber.FetchAsync`. Over the real scanner, repositories
//! and SQLite schema; the ffprobe seam is a fake that records each probe.

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
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamQuery, MediaStreamRepository};
use uuid::Uuid;

/// `MediaStreamType.Lyric` as `MediaStreamInfos.StreamType` stores it.
const LYRIC: i32 = 5;

/// An ffprobe stand-in: every file is one FLAC audio stream. Records the
/// probed paths.
#[derive(Default)]
struct RecordingProbe {
    probed: Mutex<Vec<String>>,
}

impl RecordingProbe {
    /// The file names probed since the last call.
    fn take(&self) -> Vec<String> {
        self.probed
            .lock()
            .expect("lock")
            .drain(..)
            .map(|p| file_name(&p).to_owned())
            .collect()
    }
}

#[async_trait]
impl MediaEncoder for RecordingProbe {
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
        let path = request.media_source.path.clone().unwrap_or_default();
        self.probed.lock().expect("lock").push(path);
        Ok(MediaSourceInfo {
            run_time_ticks: Some(2_400_000_000),
            bitrate: Some(900_000),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Audio,
                codec: Some("flac".to_owned()),
                ..MediaStream::default()
            }],
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(
        &self,
        _path: &str,
        _image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        Err(ServiceError::not_found("no embedded art"))
    }
    async fn extract_video_image(
        &self,
        _input_file: &str,
        _container: &str,
        _media_source: &MediaSourceInfo,
        _video_stream: &MediaStream,
        _threed_format: Option<Video3DFormat>,
        _offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        Err(ServiceError::not_found("audio only"))
    }
    fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _ticks: i64) -> String {
        String::new()
    }
    async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

struct Harness {
    db: Database,
    scanner: LibraryScanner,
    streams: Arc<FerrofinMediaStreamRepository>,
    probe: Arc<RecordingProbe>,
    meta_root: PathBuf,
    track: PathBuf,
}

async fn harness(tmp: &Path) -> Harness {
    let media = tmp.join("music");
    let album = media.join("Artist").join("Album");
    std::fs::create_dir_all(&album).expect("album dir");
    let track = album.join("01 - Song.flac");
    std::fs::write(&track, b"fLaC").expect("track");
    let meta_root = tmp.join("metadata").join("library");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Music",
        Some(CollectionTypeOptions::music),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");

    let streams = Arc::new(FerrofinMediaStreamRepository::new(db.clone()));
    let probe = Arc::new(RecordingProbe::default());
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
        Arc::clone(&probe) as Arc<dyn MediaEncoder>,
        Arc::clone(&streams) as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    )
    .with_metadata_dir(meta_root.clone());
    Harness {
        db,
        scanner,
        streams,
        probe,
        meta_root,
        track,
    }
}

impl Harness {
    async fn track_id(&self) -> Uuid {
        let id: String =
            sqlx::query_scalar(r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%Audio.Audio'"#)
                .fetch_one(self.db.pool())
                .await
                .expect("the scanned track row");
        Uuid::parse_str(&id).expect("track id")
    }

    /// The track's `Lyric` streams as `(index, file name, language)`.
    async fn lyric_streams(&self) -> Vec<(i64, String, Option<String>)> {
        let mut rows = self
            .streams
            .get_media_streams(&MediaStreamQuery {
                item_id: self.track_id().await,
                stream_type: None,
                index: None,
            })
            .await
            .expect("stream rows");
        rows.sort_by_key(|r| r.stream_index);
        rows.into_iter()
            .filter(|r| r.stream_type == LYRIC)
            .map(|r| {
                (
                    r.stream_index,
                    file_name(r.path.as_deref().unwrap_or_default()).to_owned(),
                    r.language,
                )
            })
            .collect()
    }

    /// The file names in the track's stored `LyricFiles` (no key: none).
    async fn lyric_files(&self) -> Vec<String> {
        let data: Option<String> = sqlx::query_scalar(
            r#"SELECT "Data" FROM "BaseItems" WHERE "Type" LIKE '%Audio.Audio'"#,
        )
        .fetch_one(self.db.pool())
        .await
        .expect("data");
        let data: serde_json::Value =
            serde_json::from_str(data.as_deref().unwrap_or("{}")).expect("json");
        data["LyricFiles"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| file_name(p.as_str().unwrap_or_default()).to_owned())
            .collect()
    }

    /// The track's internal metadata folder, where a downloaded lyric lands.
    async fn metadata_folder(&self) -> PathBuf {
        let dashless = self.track_id().await.simple().to_string();
        self.meta_root.join(&dashless[..2]).join(&dashless)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lyric_sidecars_become_the_tracks_lyric_stream_and_changes_reprobe_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    let album = h.track.parent().expect("album").to_path_buf();
    // A language-tagged sidecar in another case is still the track's; a
    // longer stem is another track's.
    std::fs::write(album.join("01 - song.en.LRC"), "[00:01.00]hi").expect("lrc");
    std::fs::write(album.join("01 - Songbook.lrc"), "[00:01.00]no").expect("other");

    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert_eq!(
        h.lyric_streams().await,
        [(1, "01 - song.en.LRC".to_owned(), Some("eng".to_owned()))],
        "numbered after the probe's last stream"
    );
    assert_eq!(h.lyric_files().await, ["01 - song.en.LRC"]);

    // Unchanged: nothing is probed.
    h.scanner.scan_all().await.expect("rescan");
    assert!(h.probe.take().is_empty());

    // A row scanned before `LyricFiles` was recorded, with a lyric beside
    // it, is re-probed once, which records the list.
    sqlx::query(r#"UPDATE "BaseItems" SET "Data" = NULL WHERE "Type" LIKE '%Audio.Audio'"#)
        .execute(h.db.writer())
        .await
        .expect("clear data");
    h.scanner.scan_all().await.expect("backfill scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert_eq!(h.lyric_files().await, ["01 - song.en.LRC"]);
    h.scanner.scan_all().await.expect("rescan after backfill");
    assert!(h.probe.take().is_empty());

    // A lyric the task downloads into the metadata folder is a changed set:
    // the track is re-probed, both files are recorded, the first is the
    // stream.
    let downloaded = h.metadata_folder().await;
    std::fs::create_dir_all(&downloaded).expect("metadata folder");
    std::fs::write(downloaded.join("01 - Song.lrc"), "[00:01.00]dl").expect("download");
    h.scanner.scan_all().await.expect("scan after download");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert_eq!(h.lyric_files().await, ["01 - song.en.LRC", "01 - Song.lrc"]);
    assert_eq!(h.lyric_streams().await.len(), 1);
    h.scanner.scan_all().await.expect("quiet rescan");
    assert!(
        h.probe.take().is_empty(),
        "two files, one stream: still quiet"
    );

    // Removing them drops the stream.
    std::fs::remove_file(album.join("01 - song.en.LRC")).expect("rm");
    std::fs::remove_file(downloaded.join("01 - Song.lrc")).expect("rm");
    h.scanner.scan_all().await.expect("scan after delete");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert!(h.lyric_streams().await.is_empty());
    assert!(h.lyric_files().await.is_empty());
}
