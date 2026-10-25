//! How often a library scan lists a folder: once per scan, as upstream's one
//! `DirectoryService` per validation (`LibraryManager.cs:1499`,
//! `DirectoryService.cs`) does, however many items the folder holds.
//!
//! Each probed item asks its folder for its sidecars on every rescan: a
//! video for its external subtitle and audio files, an audio track for its
//! lyric files (`ProbeProvider.HasChanged`). Without a scan-wide cache, a
//! flat folder of N files was listed 2N or N more times on a rescan where
//! nothing changed. Over the real scanner, repositories and SQLite schema;
//! the filesystem is the real one, counting what it lists.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::file_system::FerrofinFileSystem;
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
use ferrofin_model::io::FileSystemEntryInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};

/// Files in each flat folder.
const FILES: usize = 12;

/// An ffprobe stand-in: a video is one video and one audio stream, an audio
/// track one audio stream, a `.srt` one subtitle stream.
struct Probe;

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
        if request
            .media_source
            .path
            .as_deref()
            .is_some_and(|p| Path::new(p).extension().is_some_and(|e| e == "srt"))
        {
            return Ok(MediaSourceInfo {
                media_streams: vec![MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Subtitle,
                    codec: Some("subrip".to_owned()),
                    ..MediaStream::default()
                }],
                ..MediaSourceInfo::default()
            });
        }
        let audio = MediaStream {
            index: 0,
            stream_type: MediaStreamType::Audio,
            codec: Some("aac".to_owned()),
            ..MediaStream::default()
        };
        let media_streams = if request.media_is_audio {
            vec![audio]
        } else {
            vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("h264".to_owned()),
                    ..MediaStream::default()
                },
                MediaStream { index: 1, ..audio },
            ]
        };
        Ok(MediaSourceInfo {
            run_time_ticks: Some(36_000_000_000),
            bitrate: Some(4_000_000),
            media_streams,
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(
        &self,
        _path: &str,
        _image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        Err(ServiceError::not_found("no embedded cover"))
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
        // A first scan grabs a frame for a video with no artwork (the
        // Screen Grabber); a real ffmpeg delivers one.
        let frame = std::env::temp_dir().join(format!(
            "ferrofin-test-frame-{}.jpg",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&frame, b"FRAME").map_err(ServiceError::backend_source)?;
        Ok(frame.to_string_lossy().into_owned())
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

/// The real filesystem, recording every directory listed.
#[derive(Default)]
struct CountingFs {
    listed: Mutex<Vec<String>>,
    /// A file written right after the first listing of its folder: one the
    /// disk gains once the plan pass has listed the folder.
    after_listing: Mutex<Option<std::path::PathBuf>>,
}

impl CountingFs {
    /// The directories listed since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.listed.lock().expect("lock"))
    }

    /// How often `dir` was listed since the last call, which forgets them all.
    fn take_count(&self, dir: &Path) -> usize {
        self.take()
            .iter()
            .filter(|listed| Path::new(listed) == dir)
            .count()
    }
}

impl FileSystem for CountingFs {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        self.listed.lock().expect("lock").push(path.to_owned());
        let entries = FerrofinFileSystem::new().try_get_file_system_entries(path);
        let mut after = self.after_listing.lock().expect("lock");
        if after
            .as_ref()
            .is_some_and(|file| file.parent() == Some(Path::new(path)))
        {
            let file = after.take().expect("file");
            std::fs::write(file, b"1\n00:00:01,000 --> 00:00:02,000\nhi\n").expect("write");
        }
        entries
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        Vec::new()
    }
    fn file_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().file_exists(path)
    }
    fn directory_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().directory_exists(path)
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        FerrofinFileSystem::new().get_files(path, extensions)
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        FerrofinFileSystem::new().read_file(path)
    }
}

/// A scanner with the probe stand-in over `fs` and one library of `kind` at
/// `media`, whose `folder` holds `FILES` files named by `name`, scanned
/// once; the listings of that first scan are forgotten.
async fn scanned(
    tmp: &Path,
    kind: CollectionTypeOptions,
    folder: &Path,
    name: impl Fn(usize) -> String,
    fs: Arc<CountingFs>,
) -> (LibraryScanner, Arc<CountingFs>, Database) {
    std::fs::create_dir_all(folder).expect("mkdir");
    for n in 1..=FILES {
        std::fs::write(folder.join(name(n)), b"0123").expect("write");
    }
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Library",
        Some(kind),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: tmp.join("media").to_string_lossy().into_owned(),
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
    let scanner = LibraryScanner::new(vf, fs.clone(), persistence)
        .with_items(items)
        .with_probe(
            Arc::new(Probe) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        .with_metadata_dir(tmp.join("metadata"))
        .with_progress_every(0);
    let first = scanner.scan_all().await.expect("scan");
    assert!(first.created >= FILES, "{first:?}");
    fs.take_count(folder);
    (scanner, fs, db)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_rescan_lists_a_flat_folder_of_videos_once() {
    let tmp = tempfile::tempdir().expect("tmp");
    let folder = tmp.path().join("media");
    let (scanner, fs, _) = scanned(
        tmp.path(),
        CollectionTypeOptions::movies,
        &folder,
        |n| format!("Movie {n} ({}).mkv", 1990 + n),
        Arc::default(),
    )
    .await;

    let rescan = scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.created, 0, "{rescan:?}");
    // The plan pass lists it; every video's subtitle and audio sidecar check
    // reads that listing.
    assert_eq!(fs.take_count(&folder), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_rescan_lists_a_flat_album_folder_as_often_as_a_single() {
    let tmp = tempfile::tempdir().expect("tmp");
    let folder = tmp.path().join("media").join("Dump");
    let single = tmp.path().join("media").join("Single");
    std::fs::create_dir_all(&single).expect("mkdir");
    std::fs::write(single.join("01 Only.mp3"), b"0123").expect("write");
    let (scanner, fs, _) = scanned(
        tmp.path(),
        CollectionTypeOptions::music,
        &folder,
        |n| format!("{n:02} Track {n}.mp3"),
        Arc::default(),
    )
    .await;

    let rescan = scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.created, 0, "{rescan:?}");
    // The plan pass lists an album folder a fixed number of times (its
    // album check, a look-ahead, the walk); every track's lyric check reads
    // those listings, so twelve tracks cost what one does.
    let listed = fs.take();
    let count = |dir: &Path| listed.iter().filter(|l| Path::new(l) == dir).count();
    assert!(count(&single) > 0, "{listed:?}");
    assert_eq!(count(&folder), count(&single), "{listed:?}");
}

/// The subtitle streams stored from `.srt` files.
async fn srt_streams(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "Path" LIKE '%.srt'"#)
        .fetch_one(db.pool())
        .await
        .expect("count")
}

/// A sidecar that appears after the plan pass listed its folder is not seen
/// by that scan: its items read the scan's listing, as each validation's
/// `DirectoryService` serves its items upstream. The next scan lists the
/// folder again, finds it changed and probes the video with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sidecar_added_after_the_plan_pass_waits_for_the_next_scan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let folder = tmp.path().join("media");
    let fs = Arc::new(CountingFs::default());
    *fs.after_listing.lock().expect("lock") = Some(folder.join("Movie 1 (1991).en.srt"));
    let (scanner, _, db) = scanned(
        tmp.path(),
        CollectionTypeOptions::movies,
        &folder,
        |n| format!("Movie {n} ({}).mkv", 1990 + n),
        fs,
    )
    .await;
    assert!(folder.join("Movie 1 (1991).en.srt").is_file());
    assert_eq!(srt_streams(&db).await, 0, "listed before it was there");

    let rescan = scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.updated, 1, "{rescan:?}");
    assert_eq!(srt_streams(&db).await, 1);
}
