//! The library scan's lyric discovery, end to end: an audio track's lyric
//! sidecars become its `Lyric` stream and its `LyricFiles`, as Jellyfin's
//! `AudioFileProber.AddExternalLyrics` records them, and a change to that set
//! re-probes the track (`ProbeProvider.HasChanged`'s lyric arm). Lyrics
//! embedded in the track's tags are saved as an `.lrc` when it has none
//! (`AudioFileProber.FetchDataFromTags` → `LyricManager.SaveLyricAsync`).
//!
//! Port target: `LyricResolver` (`MediaInfoResolver.GetExternalFiles(Audio)`:
//! a case-insensitive prefix match on the track's stem plus a media-flag
//! delimiter, over the track's folder and its internal metadata folder) as
//! run from `AudioFileProber.FetchAsync`. Over the real scanner, repositories
//! and SQLite schema; the ffprobe seam is a fake that records each probe.

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
use ferrofin_traits::persistence::{ItemRepository, MediaStreamQuery, MediaStreamRepository};
use lofty::TextEncoding;
use lofty::config::WriteOptions;
use lofty::id3::v2::{
    BinaryFrame, ExtendedTextFrame, Frame, FrameId, Id3v2Tag, SyncTextContentType,
    SynchronizedTextFrame, TimestampFormat, UnsynchronizedTextFrame,
};
use lofty::tag::TagExt;
use uuid::Uuid;

/// `MediaStreamType.Lyric` as `MediaStreamInfos.StreamType` stores it.
const LYRIC: i32 = 5;

/// An ffprobe stand-in: every file is one FLAC audio stream carrying `tags`.
/// Records the probed paths.
#[derive(Default)]
struct RecordingProbe {
    probed: Mutex<Vec<String>>,
    tags: Mutex<HashMap<String, String>>,
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

    /// Every later probe reports these embedded tags (keys as ffprobe's,
    /// lowercased).
    fn set_tags(&self, tags: &[(&str, &str)]) {
        *self.tags.lock().expect("lock") = tags
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
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
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        Ok(MediaInfo {
            media_source: self.get_media_info(request).await?,
            raw_audio_tags: Some(self.tags.lock().expect("lock").clone()),
            ..MediaInfo::default()
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
    harness_with(tmp, LibraryOptions::default()).await
}

async fn harness_with(tmp: &Path, options: LibraryOptions) -> Harness {
    // Not a readable FLAC: the lyric pick falls back to the probe's tags.
    harness_track(tmp, options, "01 - Song.flac", b"fLaC").await
}

async fn harness_track(
    tmp: &Path,
    options: LibraryOptions,
    file_name: &str,
    bytes: &[u8],
) -> Harness {
    let media = tmp.join("music");
    let album = media.join("Artist").join("Album");
    std::fs::create_dir_all(&album).expect("album dir");
    let track = album.join(file_name);
    std::fs::write(&track, bytes).expect("track");
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
            ..options
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
    let probe_requests = h.probe.take();
    assert!(probe_requests.is_empty(), "{probe_requests:?}");

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
    let probe_requests = h.probe.take();
    assert!(probe_requests.is_empty(), "{probe_requests:?}");

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
    let lyric_streams = h.lyric_streams().await;
    assert!(lyric_streams.is_empty(), "{lyric_streams:?}");
    let lyric_files = h.lyric_files().await;
    assert!(lyric_files.is_empty(), "{lyric_files:?}");
}

/// The refresh `LyricsController.UploadLyrics` queues after saving a lyric
/// (`QueueRefresh` with default options): the track's own refresh sees the
/// new sidecar (`ProbeProvider.HasChanged`'s lyric arm) and records it.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_records_a_lyric_saved_beside_the_track() {
    use ferrofin_core::{ScanCancel, ScanPasses, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    let lyric_streams = h.lyric_streams().await;
    assert!(lyric_streams.is_empty(), "{lyric_streams:?}");

    let album = h.track.parent().expect("album");
    std::fs::write(album.join("01 - Song.lrc"), "[00:01.00]hello").expect("upload");
    // As the library manager queues it (`ScanRequest::item_refresh`).
    let target = ScanTarget::Items(vec![h.track.to_string_lossy().into_owned()]);
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    h.scanner
        .scan_target(
            &target,
            ScanRun::new(
                &MetadataRefreshOptions::default(),
                &none,
                &ScanCancel::new(),
            )
            .with_passes(ScanPasses::Touched),
        )
        .await
        .expect("item refresh");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert_eq!(h.lyric_files().await, ["01 - Song.lrc"]);
    assert_eq!(h.lyric_streams().await.len(), 1);
}

/// An ID3 `USLT` frame, as ffprobe reports it.
const USLT: (&str, &str) = ("lyrics-eng", "[00:01.00]Hello\r\n[00:02.5]World");
/// What `FetchDataFromTags` saves for [`USLT`]: ATL's LRC rendering.
const SAVED: &str = "\n[00:01.00]Hello\n[00:02.01]World\n";

#[tokio::test(flavor = "multi_thread")]
async fn embedded_lyrics_are_saved_to_the_metadata_folder_and_become_the_lyric_stream() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    h.probe.set_tags(&[("title", "Song"), USLT]);

    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    let saved = h.metadata_folder().await.join("01 - Song.lrc");
    assert_eq!(std::fs::read_to_string(&saved).expect("saved lrc"), SAVED);
    assert!(
        !h.track.with_extension("lrc").exists(),
        "the media folder is left alone"
    );
    assert_eq!(
        h.lyric_streams().await,
        [(1, "01 - Song.lrc".to_owned(), None)]
    );
    assert_eq!(h.lyric_files().await, ["01 - Song.lrc"]);

    // The saved file is the recorded set: nothing is probed again.
    h.scanner.scan_all().await.expect("rescan");
    let probe_requests = h.probe.take();
    assert!(probe_requests.is_empty(), "{probe_requests:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lyric_sidecar_keeps_the_embedded_lyrics_from_being_saved() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    let sidecar = h.track.with_extension("lrc");
    std::fs::write(&sidecar, "[00:01.00]mine").expect("lrc");
    h.probe.set_tags(&[USLT]);

    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert!(!h.metadata_folder().await.join("01 - Song.lrc").exists());
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("sidecar"),
        "[00:01.00]mine"
    );
    assert_eq!(h.lyric_files().await, ["01 - Song.lrc"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn save_lyrics_with_media_saves_embedded_lyrics_beside_the_track() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness_with(
        tmp.path(),
        LibraryOptions {
            save_lyrics_with_media: true,
            ..LibraryOptions::default()
        },
    )
    .await;
    h.probe.set_tags(&[USLT]);

    h.scanner.scan_all().await.expect("scan");
    let beside = h.track.with_extension("lrc");
    assert_eq!(std::fs::read_to_string(&beside).expect("saved lrc"), SAVED);
    // `TrySaveToFiles` stops at the first path that takes the file.
    assert!(!h.metadata_folder().await.join("01 - Song.lrc").exists());
    assert_eq!(
        h.lyric_streams().await,
        [(1, "01 - Song.lrc".to_owned(), None)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_track_never_saves_its_embedded_lyrics() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Type" LIKE '%Audio.Audio'"#)
        .execute(h.db.writer())
        .await
        .expect("lock");

    // The file changes, so the track is probed again, now with lyrics.
    h.probe.set_tags(&[USLT]);
    std::fs::write(&h.track, b"fLaC changed").expect("rewrite");
    std::fs::File::options()
        .write(true)
        .open(&h.track)
        .and_then(|f| {
            f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        })
        .expect("mtime");
    h.scanner.scan_all().await.expect("rescan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert!(!h.metadata_folder().await.join("01 - Song.lrc").exists());
    let lyric_streams = h.lyric_streams().await;
    assert!(lyric_streams.is_empty(), "{lyric_streams:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_all_metadata_saves_embedded_lyrics_despite_a_sidecar() {
    use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    let sidecar = h.track.with_extension("lrc");
    std::fs::write(&sidecar, "[00:01.00]mine").expect("lrc");
    h.probe.set_tags(&[USLT]);
    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert!(!h.metadata_folder().await.join("01 - Song.lrc").exists());

    // "Replace all metadata": the embedded lyric is saved even though the
    // track has one, and the stream stays the sidecar it already had.
    let replace_all = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        force_save: true,
        ..MetadataRefreshOptions::default()
    };
    h.scanner
        .scan_with(None, &replace_all)
        .await
        .expect("replace all");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    let saved = h.metadata_folder().await.join("01 - Song.lrc");
    assert_eq!(std::fs::read_to_string(&saved).expect("saved lrc"), SAVED);
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("sidecar"),
        "[00:01.00]mine"
    );
    assert_eq!(h.lyric_files().await, ["01 - Song.lrc"]);
    assert_eq!(h.lyric_streams().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lyric_that_cannot_be_written_fails_the_probe_and_is_retried() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness(tmp.path()).await;
    // A file where the metadata folder belongs: every save path fails.
    std::fs::create_dir_all(h.meta_root.parent().expect("parent")).expect("dir");
    std::fs::write(&h.meta_root, b"not a folder").expect("blocker");
    h.probe.set_tags(&[USLT]);

    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    let streams = h
        .streams
        .get_media_streams(&MediaStreamQuery {
            item_id: h.track_id().await,
            stream_type: None,
            index: None,
        })
        .await
        .expect("streams");
    assert!(
        streams.is_empty(),
        "the probe result is dropped: {streams:?}"
    );

    // The refresh did not complete, so the next scan probes again and saves.
    std::fs::remove_file(&h.meta_root).expect("unblock");
    h.scanner.scan_all().await.expect("rescan");
    assert_eq!(h.probe.take(), ["01 - Song.flac"]);
    assert_eq!(h.lyric_files().await, ["01 - Song.lrc"]);
}

/// A real MP3: an ID3v2 tag holding `frames` in this order, then three silent
/// MPEG-1 Layer III frames, so the file's own tags can be read.
fn mp3(frames: Vec<Frame<'static>>) -> Vec<u8> {
    let mut tag = Id3v2Tag::new();
    for frame in frames {
        let _ = tag.insert(frame);
    }
    let mut bytes = Vec::new();
    tag.dump_to(&mut bytes, WriteOptions::default())
        .expect("dump tag");
    for _ in 0..3 {
        // 128 kbit/s at 44.1 kHz: a 417-byte frame, header included.
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        bytes.extend(std::iter::repeat_n(0u8, 413));
    }
    bytes
}

fn uslt(language: [u8; 3], text: &str) -> Frame<'static> {
    Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
        TextEncoding::UTF8,
        language,
        "",
        text.to_owned(),
    ))
}

/// A `SYLT` frame (millisecond timestamps), stored as lofty reads it.
fn sylt(lines: &[(u32, &str)]) -> Frame<'static> {
    let frame = SynchronizedTextFrame::new(
        TextEncoding::UTF8,
        *b"eng",
        TimestampFormat::MS,
        SyncTextContentType::Lyrics,
        None,
        lines.iter().map(|(t, s)| (*t, (*s).to_owned())).collect(),
    );
    Frame::Binary(BinaryFrame::new(
        FrameId::new("SYLT").expect("id"),
        frame.as_bytes(WriteOptions::default()).expect("sylt"),
    ))
}

/// The `TXXX` frame ffmpeg writes for a `lyrics` key (described `USLT`),
/// which ffprobe reports as `lyrics`.
fn txxx_lyrics(text: &str) -> Frame<'static> {
    Frame::UserText(ExtendedTextFrame::new(
        TextEncoding::UTF8,
        "USLT".to_owned(),
        text.to_owned(),
    ))
}

/// Scans a library holding one MP3 with these ID3v2 frames (ffprobe
/// reporting `probe_tags`) and returns the `.lrc` saved for it, if any.
async fn saved_lyrics_for(
    frames: Vec<Frame<'static>>,
    probe_tags: &[(&str, &str)],
) -> Option<String> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness_track(
        tmp.path(),
        LibraryOptions::default(),
        "01 - Song.mp3",
        &mp3(frames),
    )
    .await;
    h.probe.set_tags(probe_tags);
    h.scanner.scan_all().await.expect("scan");
    assert_eq!(h.probe.take(), ["01 - Song.mp3"]);
    let saved = std::fs::read_to_string(h.metadata_folder().await.join("01 - Song.lrc")).ok();
    if saved.is_some() {
        assert_eq!(
            h.lyric_streams().await,
            [(1, "01 - Song.lrc".to_owned(), None)]
        );
    }
    saved
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sylt_only_track_saves_its_synchronised_lyrics_as_lrc() {
    // ffprobe reports no tag for a `SYLT` frame; the file's own tag has it.
    let saved = saved_lyrics_for(
        vec![sylt(&[(1_000, "Hello"), (65_250, "\nWorld")])],
        &[("title", "Song")],
    )
    .await;
    // ATL skips the newline a SYLT editor puts before a line.
    assert_eq!(
        saved.as_deref(),
        Some("\n[00:01.00]Hello\n[01:05.25]World\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_uslt_languages_save_the_first_in_file_order() {
    // ffprobe's tag map sorts `lyrics-deu` first; the file has `eng` first.
    let saved = saved_lyrics_for(
        vec![
            uslt(*b"eng", "English words"),
            uslt(*b"deu", "Deutsche Worte"),
        ],
        &[
            ("lyrics-eng", "English words"),
            ("lyrics-deu", "Deutsche Worte"),
        ],
    )
    .await;
    assert_eq!(saved.as_deref(), Some("English words"));
}

#[tokio::test(flavor = "multi_thread")]
async fn txxx_lyrics_are_still_saved_an_accepted_divergence() {
    // Jellyfin's ATL reads a `TXXX` lyric frame as a custom field and saves nothing;
    // Ferrofin keeps the lyrics the user has (owner decision D3).
    let saved = saved_lyrics_for(
        vec![txxx_lyrics("Custom field words")],
        &[("lyrics", "Custom field words")],
    )
    .await;
    assert_eq!(saved.as_deref(), Some("Custom field words"));
}

#[tokio::test(flavor = "multi_thread")]
async fn txxx_lyrics_never_win_over_a_uslt_frame() {
    let saved = saved_lyrics_for(
        vec![txxx_lyrics("[00:01.00]Custom"), uslt(*b"eng", "Real words")],
        &[("lyrics", "[00:01.00]Custom"), ("lyrics-eng", "Real words")],
    )
    .await;
    assert_eq!(saved.as_deref(), Some("Real words"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_file_falls_back_to_the_probes_tags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let h = harness_track(
        tmp.path(),
        LibraryOptions::default(),
        "01 - Song.mp3",
        b"ID3\x04\x00\x00\x7f\x7f\x7f\x7fnot a tag",
    )
    .await;
    h.probe.set_tags(&[USLT]);
    h.scanner.scan_all().await.expect("scan");
    let saved = h.metadata_folder().await.join("01 - Song.lrc");
    assert_eq!(std::fs::read_to_string(&saved).expect("saved lrc"), SAVED);
}
