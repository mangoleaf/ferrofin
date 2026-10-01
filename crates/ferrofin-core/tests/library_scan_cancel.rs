//! Cancelling a running scan (`POST /Library/Refresh` cancels the running
//! "Scan Media Library" task; the host's teardown stops it): the scan stops
//! cooperatively — between two items, between its closing passes, out of
//! a wait that comes before an item writes anything (its probe, its
//! provider requests), or out of its artwork fetches, the item then saved
//! with exactly the artwork on disk. An item is written whole or not at all, never
//! half-written (a row stamped as refreshed without the streams its probe
//! measured, which `IsMissingMediaInfo` would then never re-probe), and a
//! hung ffprobe cannot keep a cancelled scan alive. The next scan picks up
//! where it stopped.
//!
//! The same stopping points serve the scan queue's priority lane: an item
//! refresh waiting there runs between two closing passes too, and an item
//! refresh runs none of the library-wide ones.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaStreamRepository, FerrofinVirtualFolderManager, LibraryScanner, ScanCancel,
    ScanOutcome, ScanProgress, ScanRun,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};
use ferrofin_traits::providers::{
    DynamicMetadataLookup, DynamicMetadataProvider, DynamicMetadataResult, MetadataRefreshOptions,
};

/// An ffprobe stand-in that holds the FIRST probe it is asked for until
/// released (never, unless a test releases it: a hung ffprobe on a cold
/// NFS mount), reports when that probe has started, and when its wait was
/// dropped (the real probe's child is killed then, `kill_on_drop`).
struct HeldProbe {
    held: Mutex<Option<String>>,
    entered: tokio::sync::watch::Sender<bool>,
    release: tokio::sync::Semaphore,
    killed: Arc<tokio::sync::watch::Sender<bool>>,
}

impl HeldProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(None),
            entered: tokio::sync::watch::Sender::new(false),
            release: tokio::sync::Semaphore::new(0),
            killed: Arc::new(tokio::sync::watch::Sender::new(false)),
        })
    }
}

/// Flags its probe as killed when dropped before it finished.
struct KillFlag(Arc<tokio::sync::watch::Sender<bool>>, bool);

impl Drop for KillFlag {
    fn drop(&mut self) {
        if !self.1 {
            self.0.send_replace(true);
        }
    }
}

#[async_trait]
impl MediaEncoder for HeldProbe {
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
        let first = {
            let mut held = self.held.lock().expect("lock");
            let first = held.is_none();
            held.get_or_insert(path);
            first
        };
        if first {
            let mut flag = KillFlag(Arc::clone(&self.killed), false);
            self.entered.send_replace(true);
            self.release.acquire().await.expect("open").forget();
            flag.1 = true;
        }
        Ok(MediaSourceInfo {
            run_time_ticks: Some(72_000_000_000),
            bitrate: Some(8_000_000),
            media_streams: vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Video,
                codec: Some("h264".to_owned()),
                width: Some(1920),
                height: Some(1080),
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
        unreachable!("no audio in this library")
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
        unreachable!("no frame extraction in a scan")
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

/// `(movie name, stamped as refreshed, media streams stored)` per movie row.
async fn movies(db: &Database) -> Vec<(String, bool, i64)> {
    sqlx::query_as(
        r#"SELECT b."Name", b."DateLastRefreshed" IS NOT NULL,
                  (SELECT COUNT(*) FROM "MediaStreamInfos" s WHERE s."ItemId" = b."Id")
           FROM "BaseItems" b WHERE b."Type" LIKE '%Movies.Movie' ORDER BY b."Name""#,
    )
    .fetch_all(db.pool())
    .await
    .expect("movies")
}

/// A movie library of three titles on a fresh database, scanned by a
/// scanner whose probe is `probe` (one probe at a time) and whose plugin
/// artwork sources are `art`.
async fn movie_library(
    root: &Path,
    probe: &Arc<HeldProbe>,
    art: Vec<Arc<dyn DynamicMetadataProvider>>,
) -> (Database, Arc<LibraryScanner>) {
    let media = root.join("movies");
    for name in ["Alien (1979)", "Blade Runner (1982)", "Heat (1995)"] {
        let file = media.join(name).join(format!("{name}.mkv"));
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
        std::fs::write(&file, b"0123456789").expect("write");
    }
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
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
    let years = ferrofin_core::YearStore::new(
        persistence.clone(),
        ferrofin_core::item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: None,
        },
        root.join("metadata/Year"),
    );
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(items)
        .with_years(years)
        .with_probe(
            Arc::clone(probe) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        // One probe at a time, so no later movie's is already done.
        .with_probe_concurrency(1)
        .with_progress_every(0)
        .with_metadata_dir(root.join("metadata/library"))
        .with_dynamic_providers(art);
    (db, Arc::new(scanner))
}

/// Scans `target` with the defaults, cancellable through `cancel` and
/// reporting to `progress`.
async fn scan(
    scanner: &LibraryScanner,
    cancel: &ScanCancel,
    progress: Option<&ScanProgress>,
) -> ScanOutcome {
    let options = MetadataRefreshOptions::default();
    scanner
        .scan_target(
            &ScanTarget::All,
            match progress {
                Some(progress) => ScanRun::new(&options, &options, cancel).with_progress(progress),
                None => ScanRun::new(&options, &options, cancel),
            },
        )
        .await
        .expect("scan")
}

/// `Year` rows the closing years pass materialized.
async fn years(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" LIKE '%Entities.Year'"#)
        .fetch_one(db.pool())
        .await
        .expect("years")
}

/// A probe that never returns (a hung ffprobe) cannot keep a cancelled
/// scan alive: the wait for it races the cancellation, the item it belongs
/// to is not written at all (nothing had been), its ffprobe is killed, and
/// no other item is started. The next scan writes every item whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_probe_cannot_keep_a_cancelled_scan_alive() {
    let tmp = tempfile::tempdir().expect("tmp");
    let probe = HeldProbe::new();
    let (db, scanner) = movie_library(tmp.path(), &probe, Vec::new()).await;

    let cancel = ScanCancel::new();
    let running = tokio::spawn({
        let scanner = Arc::clone(&scanner);
        let cancel = cancel.clone();
        async move { scan(&scanner, &cancel, None).await }
    });
    let mut entered = probe.entered.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.wait_for(|e| *e))
        .await
        .expect("the first probe started")
        .expect("probe alive");
    // The probe never returns; only the cancellation ends the scan.
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), running)
        .await
        .expect("the cancelled scan ended although its probe hangs")
        .expect("joined");
    assert!(outcome.stopped, "{outcome:?}");
    assert_eq!(outcome.created, 0, "{outcome:?}");
    assert!(movies(&db).await.is_empty(), "nothing half-written");
    // The scan aborts the probe's task (`JoinHandle::abort`) and returns
    // without waiting for it — deliberately, so a task stuck in blocking I/O
    // can never hold a cancelled scan. The runtime drops the aborted task's
    // future, and the ffprobe child with it, on a worker right after, which
    // need not be before the scan has returned: wait for it, bounded.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.killed.subscribe().wait_for(|killed| *killed),
    )
    .await
    .expect("the hung probe was dropped (its ffprobe killed)")
    .expect("probe alive");

    // The next scan picks everything up; the probe answers from now on.
    scanner.scan_all().await.expect("rescan");
    assert_eq!(
        movies(&db).await,
        vec![
            ("Alien (1979)".to_owned(), true, 1),
            ("Blade Runner (1982)".to_owned(), true, 1),
            ("Heat (1995)".to_owned(), true, 1),
        ]
    );
}

/// A cancellation that lands during the last item (here: when the item walk
/// reports its 96 %) skips the pruning and the closing passes: a vanished
/// movie's row stays until the next scan, which prunes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_after_the_last_item_skips_the_pruning() {
    let tmp = tempfile::tempdir().expect("tmp");
    let probe = HeldProbe::new();
    probe.release.add_permits(1);
    let (db, scanner) = movie_library(tmp.path(), &probe, Vec::new()).await;
    scanner.scan_all().await.expect("first scan");
    std::fs::remove_dir_all(tmp.path().join("movies/Heat (1995)")).expect("remove");

    let cancel = ScanCancel::new();
    let at_96 = {
        let cancel = cancel.clone();
        move |percent: f64| {
            if percent >= 96.0 {
                cancel.cancel();
            }
        }
    };
    let outcome = scan(&scanner, &cancel, Some(&at_96)).await;
    assert!(outcome.stopped, "{outcome:?}");
    assert_eq!(outcome.removed, 0, "no pruning after the cancellation");
    assert_eq!(
        movies(&db).await.len(),
        3,
        "the vanished row is still there"
    );

    let outcome = scanner.scan_all().await.expect("rescan");
    assert_eq!(outcome.removed, 1, "{outcome:?}");
    assert_eq!(movies(&db).await.len(), 2);
}

/// `RunPostScanTasks` takes the scan's token: a cancellation between two
/// closing passes stops before the next one (here the years pass never
/// runs), and the progress never reaches 100 %.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_between_closing_passes_skips_the_rest() {
    let tmp = tempfile::tempdir().expect("tmp");
    let probe = HeldProbe::new();
    probe.release.add_permits(1);
    let (db, scanner) = movie_library(tmp.path(), &probe, Vec::new()).await;

    let cancel = ScanCancel::new();
    let reported = Arc::new(Mutex::new(Vec::new()));
    let after_first_pass = {
        let cancel = cancel.clone();
        let reported = Arc::clone(&reported);
        move |percent: f64| {
            reported.lock().expect("reported").push(percent);
            if percent > 96.0 {
                cancel.cancel();
            }
        }
    };
    let outcome = scan(&scanner, &cancel, Some(&after_first_pass)).await;
    assert!(outcome.stopped, "{outcome:?}");
    assert_eq!(outcome.created, 3, "every item was scanned");
    assert_eq!(years(&db).await, 0, "the years pass never ran");
    let reported = reported.lock().expect("reported").clone();
    assert!(reported.contains(&96.0), "{reported:?}");
    assert!(reported.iter().all(|p| *p < 100.0), "{reported:?}");
    assert!(
        reported.windows(2).all(|w| w[0] <= w[1]),
        "progress only moves forward: {reported:?}"
    );

    let outcome = scanner.scan_all().await.expect("rescan");
    assert!(!outcome.stopped);
    assert_eq!(years(&db).await, 3, "the next scan runs the passes");
}

/// An item refresh's closing passes are the touched items' only: the
/// library-wide passes (here the years pass) never run for it; the next
/// library scan runs them.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_runs_no_library_wide_closing_pass() {
    let tmp = tempfile::tempdir().expect("tmp");
    let probe = HeldProbe::new();
    probe.release.add_permits(1);
    let (db, scanner) = movie_library(tmp.path(), &probe, Vec::new()).await;
    let heat = tmp.path().join("movies/Heat (1995)/Heat (1995).mkv");
    let options = MetadataRefreshOptions::default();
    let outcome = scanner
        .scan_target(
            &ScanTarget::Items(vec![heat.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &options, &ScanCancel::new())
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await
        .expect("item refresh");
    assert_eq!(outcome.created, 1, "{outcome:?}");
    assert_eq!(years(&db).await, 0, "no years pass");

    scanner.scan_all().await.expect("scan");
    assert_eq!(years(&db).await, 3);
}

/// A priority lane holding one item refresh, handed out once `ready`.
struct ReadyLane {
    ready: std::sync::atomic::AtomicBool,
    pending: Mutex<Option<ferrofin_core::LaneRefresh>>,
    served: Mutex<Vec<ScanOutcome>>,
}

impl ferrofin_core::PriorityLane for ReadyLane {
    fn next(&self) -> Option<ferrofin_core::LaneRefresh> {
        if self.ready.load(std::sync::atomic::Ordering::SeqCst) {
            self.pending.lock().expect("lock").take()
        } else {
            None
        }
    }
    fn done(
        &self,
        _refresh: ferrofin_core::LaneRefresh,
        outcome: &Result<ScanOutcome, ServiceError>,
    ) {
        let outcome = *outcome.as_ref().expect("served refresh");
        self.served.lock().expect("lock").push(outcome);
    }
}

/// A refresh that arrives while the scan runs its closing passes is served
/// between two of them — it does not wait for the rest — and the passes
/// then go on.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_is_served_between_closing_passes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let probe = HeldProbe::new();
    probe.release.add_permits(1);
    let (db, scanner) = movie_library(tmp.path(), &probe, Vec::new()).await;
    let heat = tmp.path().join("movies/Heat (1995)/Heat (1995).mkv");
    let options = MetadataRefreshOptions::default();
    let lane = Arc::new(ReadyLane {
        ready: std::sync::atomic::AtomicBool::new(false),
        pending: Mutex::new(Some(ferrofin_core::LaneRefresh {
            target: ScanTarget::Items(vec![heat.to_string_lossy().into_owned()]),
            options: options.clone(),
            ancestors: options.clone(),
            passes: ferrofin_core::ScanPasses::Touched,
            cancel: ScanCancel::new(),
            key: 1,
        })),
        served: Mutex::new(Vec::new()),
    });
    // Ready once the first closing pass is done (past 96 %).
    let after_first_pass = {
        let lane = Arc::clone(&lane);
        move |percent: f64| {
            if percent > 96.0 {
                lane.ready.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    };
    let cancel = ScanCancel::new();
    let outcome = scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel)
                .with_progress(&after_first_pass)
                .with_lane(lane.as_ref()),
        )
        .await
        .expect("scan");
    assert!(!outcome.stopped);
    let served = lane.served.lock().expect("lock").clone();
    assert_eq!(served.len(), 1, "served between two closing passes");
    assert_eq!(
        served[0].unchanged, 1,
        "Heat was already scanned: {served:?}"
    );
    assert_eq!(years(&db).await, 3, "the passes went on");
}

/// PNG magic: the artwork pass stores only bytes it recognizes as an image.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nART";

/// A plugin artwork source. `hang` never answers its FIRST request (an image
/// host behind a rate limiter's retries), reporting when it started and
/// whether its wait was dropped; otherwise it answers a Primary at once.
struct PluginArt {
    name: &'static str,
    hang: Option<std::sync::atomic::AtomicBool>,
    entered: tokio::sync::watch::Sender<bool>,
    dropped: Arc<tokio::sync::watch::Sender<bool>>,
}

impl PluginArt {
    fn new(name: &'static str, hang: bool) -> Arc<Self> {
        Arc::new(Self {
            name,
            hang: hang.then(|| std::sync::atomic::AtomicBool::new(true)),
            entered: tokio::sync::watch::Sender::new(false),
            dropped: Arc::new(tokio::sync::watch::Sender::new(false)),
        })
    }
}

#[async_trait]
impl DynamicMetadataProvider for PluginArt {
    fn name(&self) -> &str {
        self.name
    }
    fn library_gated(&self) -> bool {
        true
    }
    async fn lookup(
        &self,
        _item: &DynamicMetadataLookup,
    ) -> Result<Option<DynamicMetadataResult>, ServiceError> {
        Ok(None)
    }
    async fn images(
        &self,
        _item: &DynamicMetadataLookup,
        wanted: &[ImageType],
    ) -> Result<Vec<(ImageType, Vec<u8>)>, ServiceError> {
        let Some(first) = &self.hang else {
            let primary = wanted.contains(&ImageType::Primary);
            return Ok(primary
                .then(|| (ImageType::Primary, PNG.to_vec()))
                .into_iter()
                .collect());
        };
        if !first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let _flag = KillFlag(Arc::clone(&self.dropped), false);
        self.entered.send_replace(true);
        std::future::pending().await
    }
}

/// `(image type, path)` of every stored image row.
async fn image_rows(db: &Database) -> Vec<(i64, String)> {
    sqlx::query_as(r#"SELECT "ImageType", "Path" FROM "BaseItemImageInfos" ORDER BY "Path""#)
        .fetch_all(db.pool())
        .await
        .expect("images")
}

/// An image source that never answers cannot keep a cancelled scan alive
/// (nor a shutdown waiting on it): the artwork fetch races the
/// cancellation. The artwork already on disk by then (the other source's
/// Primary) is what the item is saved with — no row points at a missing
/// file, no downloaded file is left unrecorded — and the cut-short refresh
/// is not stamped, so the next scan repeats it. No other item is started.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_image_provider_cannot_keep_a_cancelled_scan_alive() {
    let tmp = tempfile::tempdir().expect("tmp");
    let probe = HeldProbe::new();
    probe.release.add_permits(1);
    let hung = PluginArt::new("slow-art", true);
    let art: Vec<Arc<dyn DynamicMetadataProvider>> = vec![
        PluginArt::new("quick-art", false),
        Arc::clone(&hung) as Arc<dyn DynamicMetadataProvider>,
    ];
    let (db, scanner) = movie_library(tmp.path(), &probe, art).await;

    let cancel = ScanCancel::new();
    let running = tokio::spawn({
        let scanner = Arc::clone(&scanner);
        let cancel = cancel.clone();
        async move { scan(&scanner, &cancel, None).await }
    });
    let mut entered = hung.entered.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.wait_for(|e| *e))
        .await
        .expect("the hung image request started")
        .expect("provider alive");
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), running)
        .await
        .expect("the cancelled scan ended although its image request hangs")
        .expect("joined");
    assert!(outcome.stopped, "{outcome:?}");
    assert_eq!(outcome.created, 1, "{outcome:?}");
    assert!(*hung.dropped.borrow(), "the hung request was dropped");
    let saved = movies(&db).await;
    assert_eq!(saved.len(), 1, "no other item started: {saved:?}");
    assert!(
        !saved[0].1,
        "the cut-short refresh is not stamped: {saved:?}"
    );
    assert_eq!(saved[0].2, 1, "the item is whole: {saved:?}");
    let images = image_rows(&db).await;
    assert_eq!(images.len(), 1, "the quick Primary only: {images:?}");
    assert_eq!(images[0].0, ImageType::Primary as i64, "{images:?}");
    assert!(Path::new(&images[0].1).exists(), "{images:?}");

    // The next scan finishes it, and the rest; the source answers now.
    let outcome = scanner.scan_all().await.expect("rescan");
    assert!(!outcome.stopped, "{outcome:?}");
    let saved = movies(&db).await;
    assert_eq!(saved.len(), 3, "{saved:?}");
    assert!(
        saved
            .iter()
            .all(|(_, stamped, streams)| *stamped && *streams == 1),
        "{saved:?}"
    );
}
