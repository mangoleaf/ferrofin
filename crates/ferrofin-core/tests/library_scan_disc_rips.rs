//! DVD and Blu-ray folder rips through the real scanner. A DVD rip's `Video`
//! is probed at its folder as a `Dvd` with its `IsoType`, which the encoder
//! answers from the title's `.vob` files (`FFProbeVideoInfo.ProbeVideo`), and
//! the scan stores the streams, run time and size that probe returns.
//! Playback's forced probe then probes a DVD rip a scan could not, so the next
//! `PlaybackInfo` finds a video stream. A Blu-ray folder is not probed yet:
//! its streams come from BDInfo upstream (gl-1834). The ffprobe seam is a fake
//! that records its requests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::media_source_manager::item_refresh_remote_probe;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemCountService, FerrofinItemPersistenceService,
    FerrofinItemRepository, FerrofinLibraryManager, FerrofinMediaStreamRepository,
    FerrofinPeopleRepository, FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat, VideoType};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::media_info::MediaInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};

/// Answers every probe with one MPEG-2 video stream, 2 minutes and 12 345
/// bytes, and records each request's path and video type; it fails every
/// probe while `failing` is set.
#[derive(Default)]
struct Recording {
    requests: Mutex<Vec<(String, Option<VideoType>)>>,
    failing: AtomicBool,
}

#[async_trait]
impl MediaEncoder for Recording {
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
        Ok(self.get_media_info_full(request).await?.media_source)
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaInfo, ServiceError> {
        let path = request.media_source.path.clone().unwrap_or_default();
        self.requests
            .lock()
            .unwrap()
            .push((path, request.media_source.video_type));
        if self.failing.load(Ordering::SeqCst) {
            return Err(ServiceError::backend("ffprobe is not installed"));
        }
        Ok(MediaInfo {
            media_source: MediaSourceInfo {
                run_time_ticks: Some(1_200_000_000),
                size: Some(12_345),
                container: Some("mpeg".to_owned()),
                media_streams: vec![MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("mpeg2video".to_owned()),
                    ..MediaStream::default()
                }],
                ..MediaSourceInfo::default()
            },
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
        Err(ServiceError::backend("no frames"))
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

const DVD: &str = "Disc Dvd (2020)";
const BLU_RAY: &str = "Disc Bluray (2021)";

struct Harness {
    db: Database,
    scanner: Arc<LibraryScanner>,
    persistence: Arc<FerrofinItemPersistenceService>,
    encoder: Arc<Recording>,
    root: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

/// A movie library holding a DVD rip (`VIDEO_TS` with one title `.vob`) and
/// a Blu-ray rip (`BDMV` with one stream file).
async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let movies = tmp.path().join("movies");
    for file in [
        format!("{DVD}/VIDEO_TS/VTS_01_1.VOB"),
        format!("{BLU_RAY}/BDMV/STREAM/00001.m2ts"),
    ] {
        let path = movies.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"disc").unwrap();
    }
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
                path: movies.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .unwrap();
    let encoder = Arc::new(Recording::default());
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence.clone(),
    )
    .with_items(items)
    .with_probe(
        Arc::clone(&encoder) as Arc<dyn MediaEncoder>,
        Arc::new(FerrofinMediaStreamRepository::new(db.clone())) as Arc<dyn MediaStreamRepository>,
        Arc::new(FerrofinChapterRepository::new(db.clone())),
    );
    Harness {
        db,
        scanner: Arc::new(scanner),
        persistence,
        encoder,
        root: movies,
        _tmp: tmp,
    }
}

impl Harness {
    fn rip(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }

    fn probed(&self) -> Vec<(String, Option<VideoType>)> {
        self.encoder.requests.lock().unwrap().clone()
    }

    /// The stored run time, size and `Data` of the movie at the rip `name`.
    async fn row(&self, name: &str) -> (Option<i64>, Option<i64>, serde_json::Value) {
        let (ticks, size, data): (Option<i64>, Option<i64>, Option<String>) = sqlx::query_as(
            r#"SELECT "RunTimeTicks", "Size", "Data" FROM "BaseItems"
               WHERE "Path" = ?1 AND "Type" LIKE '%Movie'"#,
        )
        .bind(self.rip(name))
        .fetch_one(self.db.pool())
        .await
        .expect("the scanned rip");
        let data = serde_json::from_str(&data.unwrap_or_else(|| "{}".to_owned())).unwrap();
        (ticks, size, data)
    }

    async fn stream_count(&self, name: &str) -> i64 {
        sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "ItemId" =
               (SELECT "Id" FROM "BaseItems" WHERE "Path" = ?1 AND "Type" LIKE '%Movie')"#,
        )
        .bind(self.rip(name))
        .fetch_one(self.db.pool())
        .await
        .unwrap()
    }

    /// The library manager over this harness's scanner, as the server builds it.
    fn library(&self) -> Arc<dyn LibraryManager> {
        Arc::new(
            FerrofinLibraryManager::new(
                Arc::new(FerrofinItemRepository::new(
                    self.db.clone(),
                    Arc::new(ItemTypeLookup::new()),
                )),
                Arc::new(FerrofinItemCountService::new(self.db.clone())),
                self.persistence.clone(),
                Arc::new(FerrofinPeopleRepository::new(self.db.clone())),
            )
            .with_scanner(Arc::clone(&self.scanner)),
        )
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_probes_a_dvd_rip_at_its_folder_and_not_a_blu_ray_rip() {
    let h = harness().await;
    h.scanner.scan_all().await.unwrap();

    assert_eq!(h.probed(), [(h.rip(DVD), Some(VideoType::Dvd))]);
    let (ticks, size, data) = h.row(DVD).await;
    assert_eq!(data["VideoType"], "Dvd");
    assert_eq!(ticks, Some(1_200_000_000));
    // A directory has no length of its own: the probe's size is stored.
    assert_eq!(size, Some(12_345));
    assert_eq!(data["Container"], "mpeg");
    assert_eq!(data["DefaultVideoStreamIndex"], 0);
    assert_eq!(h.stream_count(DVD).await, 1);

    let (ticks, _, data) = h.row(BLU_RAY).await;
    assert_eq!(data["VideoType"], "BluRay");
    assert_eq!(ticks, None);
    assert_eq!(h.stream_count(BLU_RAY).await, 0);

    // An unchanged rip with its media info is not probed again.
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.probed().len(), 1, "{:?}", h.probed());
}

/// Playback's forced probe (`GetPlaybackMediaSources`' `FullRefresh` with the
/// remote probe on) probes a DVD rip the scan could not, so the next
/// `PlaybackInfo` finds its video stream and forces nothing.
#[tokio::test(flavor = "multi_thread")]
async fn playbacks_forced_probe_fills_a_dvd_rip_the_scan_could_not() {
    let h = harness().await;
    h.encoder.failing.store(true, Ordering::SeqCst);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.stream_count(DVD).await, 0);

    h.encoder.failing.store(false, Ordering::SeqCst);
    let library = h.library();
    let probe = item_refresh_remote_probe(Arc::downgrade(&library));
    probe(h.rip(DVD)).await.expect("probe");

    assert_eq!(h.probed().last(), Some(&(h.rip(DVD), Some(VideoType::Dvd))));
    assert_eq!(h.stream_count(DVD).await, 1);
    assert_eq!(h.row(DVD).await.0, Some(1_200_000_000));
}

/// A stored DVD rip without media info (a failed probe, or a row an older
/// version scanned) is probed by the next ordinary scan: `IsMissingMediaInfo`,
/// since a disc rip has no file date to compare.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_scan_probes_a_dvd_rip_an_earlier_one_could_not() {
    let h = harness().await;
    h.encoder.failing.store(true, Ordering::SeqCst);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.stream_count(DVD).await, 0);

    h.encoder.failing.store(false, Ordering::SeqCst);
    h.scanner.scan_all().await.unwrap();
    assert_eq!(h.stream_count(DVD).await, 1);
    assert_eq!(h.row(DVD).await.0, Some(1_200_000_000));
}
