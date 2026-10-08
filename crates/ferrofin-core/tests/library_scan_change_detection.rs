//! Per-item change detection, end to end through a real scan
//! (`PLAN_SCAN_CHANGE_DETECTION` Phase 4): the port of
//! `MetadataService.RefreshMetadata`'s provider selection and save rule.
//!
//! Over the real scanner, repositories and SQLite schema, with an ffprobe
//! stand-in that records every probed path, a TMDB stand-in that records every
//! request, and a trigger on every table that counts every row written:
//!
//! - an unchanged rescan probes nothing, asks no provider and writes nothing;
//! - a file whose mtime moved is probed, fetched and saved — only that one;
//! - an NFO newer than the last save re-reads only that item's local metadata;
//! - a sidecar subtitle added beside a video re-probes only that video;
//! - a never-refreshed item runs everything once, then goes quiet;
//! - an elapsed `AutomaticRefreshIntervalDays` refetches;
//! - a provider that fails leaves the item unstamped (retried next scan), one
//!   that finds nothing stamps it (owner decision D1).
//!
//! And the Phase 3L lock rules on the same path: an unlocked edit survives a
//! quiet rescan, a provider pass replaces only the fields outside the item's
//! `LockedFields`, and `LockData` refuses the remote providers while a new
//! local poster is still discovered.
//!
//! And the Phase 5 refresh modes a folder refresh scans with (the dashboard's
//! three choices, `ValidationOnly`, `None`): which providers run, how their
//! answer merges onto a stored row carrying an edited and an empty field, and
//! what is stamped and saved — and that a locked item is left alone in every
//! one of them.
//!
//! And Phase 5b: a file item's `POST /Items/{id}/Refresh` is the scan of its
//! own path — the same decision, merge, locks, probe, NFO and providers,
//! touching nothing else of its library — and "Identify → Apply" is that
//! scan with the chosen result pinning the lookup and the NFO skipped.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaStreamRepository, FerrofinVirtualFolderManager, LibraryScanner, ScanOutcome,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// An ffprobe stand-in recording each probed path: a video file has a video
/// and an audio stream, a `.srt` one subtitle stream.
#[derive(Default)]
struct RecordingProbe {
    probed: Mutex<Vec<String>>,
    metadata: Mutex<Option<ferrofin_model::media_info::MediaInfo>>,
}

impl RecordingProbe {
    /// The file names probed since the last call.
    fn take(&self) -> Vec<String> {
        let mut probed = self.probed.lock().expect("lock");
        let mut names: Vec<String> = probed
            .drain(..)
            .map(|p| {
                Path::new(&p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        names.sort_unstable();
        names
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
        self.probed.lock().expect("lock").push(path.clone());
        let streams = if Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("srt"))
        {
            vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Subtitle,
                codec: Some("subrip".to_owned()),
                ..MediaStream::default()
            }]
        } else {
            vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("h264".to_owned()),
                    width: Some(1920),
                    height: Some(1080),
                    ..MediaStream::default()
                },
                MediaStream {
                    index: 1,
                    stream_type: MediaStreamType::Audio,
                    codec: Some("aac".to_owned()),
                    ..MediaStream::default()
                },
            ]
        };
        Ok(MediaSourceInfo {
            run_time_ticks: Some(72_000_000_000),
            bitrate: Some(8_000_000),
            media_streams: streams,
            ..MediaSourceInfo::default()
        })
    }
    async fn get_media_info_full(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<ferrofin_model::media_info::MediaInfo, ServiceError> {
        let source = self.get_media_info(request).await?;
        let mut info = self
            .metadata
            .lock()
            .expect("metadata")
            .clone()
            .unwrap_or_default();
        info.media_source = MediaSourceInfo {
            name: info.media_source.name,
            container: info.media_source.container,
            ..source
        };
        Ok(info)
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

/// `/movie/{id}` details, trailers included, so no backfill heuristic (D2)
/// ever asks again on its own.
fn details_json(title: &str) -> String {
    format!(
        r#"{{"title": "{title}", "overview": "About {title}.", "vote_average": 8.0,
            "release_date": "1999-03-30",
            "videos": {{"results": [
                {{"site": "YouTube", "type": "Trailer", "key": "k{title}", "name": "Trailer"}}
            ]}}}}"#
    )
}

/// How the TMDB stand-in answers a movie's details.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// A full record.
    Found,
    /// The search finds nothing: an answer, not a failure.
    Nothing,
    /// `401` on every request for it: a failure.
    Fail,
}

/// A TMDB stand-in for two movies (The Matrix → 603, Heat → 949), recording
/// each request line.
struct Tmdb {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    heat: Arc<Mutex<Answer>>,
}

impl Tmdb {
    fn spawn() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let heat = Arc::new(Mutex::new(Answer::Found));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (log, answer) = (Arc::clone(&requests), Arc::clone(&heat));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                log.lock().expect("lock").push(line.clone());
                let heat = *answer.lock().expect("lock");
                let is_heat = line.contains("Heat") || line.contains("/movie/949");
                let (status, payload) = if is_heat && heat == Answer::Fail {
                    ("401 Unauthorized", "{}".to_owned())
                } else if line.contains("/search/movie") {
                    let hit = if is_heat {
                        if heat == Answer::Nothing {
                            r#"{"results": []}"#
                        } else {
                            r#"{"results": [{"id": 949, "title": "Heat"}]}"#
                        }
                    } else {
                        r#"{"results": [{"id": 603, "title": "The Matrix"}]}"#
                    };
                    ("200 OK", hit.to_owned())
                } else if line.contains("/find/tt0113277") {
                    (
                        "200 OK",
                        r#"{"movie_results": [{"id": 949}], "tv_results": []}"#.to_owned(),
                    )
                } else if line.contains("/movie/603?") {
                    ("200 OK", details_json("The Matrix"))
                } else if line.contains("/movie/949?") {
                    ("200 OK", details_json("Heat"))
                } else if line.contains("/movie/111?") {
                    // A record with no release date (and a trailer, so no
                    // backfill asks again on its own).
                    (
                        "200 OK",
                        r#"{"title": "Nameless", "overview": "About Nameless.",
                            "videos": {"results": [{"site": "YouTube", "type": "Trailer",
                                "key": "kNameless", "name": "Trailer"}]}}"#
                            .to_owned(),
                    )
                } else {
                    ("404 Not Found", "{}".to_owned())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        Self {
            base: format!("http://{addr}"),
            requests,
            heat,
        }
    }

    /// The request lines since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().expect("lock"))
    }

    fn answer_heat(&self, answer: Answer) {
        *self.heat.lock().expect("lock") = answer;
    }
}

/// A movie library of two titles, each in its own folder, scanned by a
/// scanner with the probe, TMDB and the item repository wired.
struct Fixture {
    db: Database,
    vf: Arc<dyn VirtualFolderManager>,
    scanner: LibraryScanner,
    probe: Arc<RecordingProbe>,
    tmdb: Tmdb,
    matrix: PathBuf,
    heat: PathBuf,
}

impl Fixture {
    async fn new(root: &Path, interval_days: i32) -> Self {
        Self::with_omdb(root, interval_days, None).await
    }

    /// [`new`](Self::new), with OMDb wired to `omdb` (a stand-in's base URL)
    /// when given.
    async fn with_omdb(root: &Path, interval_days: i32, omdb: Option<&str>) -> Self {
        let media = root.join("movies");
        let matrix = media
            .join("The Matrix (1999)")
            .join("The Matrix (1999).mkv");
        let heat = media.join("Heat (1995)").join("Heat (1995).mkv");
        for file in [&matrix, &heat] {
            std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
            std::fs::write(file, b"0123456789").expect("write");
        }
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                automatic_refresh_interval_days: interval_days,
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let probe = Arc::new(RecordingProbe::default());
        let tmdb = Tmdb::spawn();
        let scanner = LibraryScanner::new(
            Arc::clone(&vf),
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        )
        .with_items(items)
        .with_probe(
            Arc::clone(&probe) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&tmdb.base)),
            root.join("metadata"),
        )
        .with_progress_every(0);
        let scanner = match omdb {
            Some(base) => scanner.with_omdb(Arc::new(
                ferrofin_providers::OmdbClient::new("key").with_base_url(base),
            )),
            None => scanner,
        };
        count_writes(&db).await;
        Self {
            db,
            vf,
            scanner,
            probe,
            tmdb,
            matrix,
            heat,
        }
    }

    async fn scan(&self) -> ScanOutcome {
        self.scanner.scan_all().await.expect("scan")
    }

    async fn scan_with(&self, options: &MetadataRefreshOptions) -> ScanOutcome {
        self.scanner.scan_with(None, options).await.expect("scan")
    }

    fn id(path: &Path) -> String {
        guid_to_db(derive_item_id(BaseItemKind::Movie, &path.to_string_lossy()).expect("id"))
    }

    /// `(DateLastSaved, DateLastRefreshed)` as stored, per movie path.
    async fn stamps(&self, path: &Path) -> (Option<String>, Option<String>) {
        sqlx::query_as(
            r#"SELECT "DateLastSaved", "DateLastRefreshed" FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(Self::id(path))
        .fetch_one(self.db.pool())
        .await
        .expect("row")
    }

    async fn set(&self, path: &Path, column: &str, value: Option<String>) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"UPDATE "BaseItems" SET "{column}" = ?1 WHERE "Id" = ?2"#
        )))
        .bind(value)
        .bind(Self::id(path))
        .execute(self.db.writer())
        .await
        .expect("update");
        reset_writes(&self.db).await;
    }

    /// Rows written (inserted, updated or deleted) per table since the last
    /// call.
    async fn writes(&self) -> HashMap<String, i64> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
                .fetch_all(self.db.pool())
                .await
                .expect("writes");
        reset_writes(&self.db).await;
        rows.into_iter().collect()
    }
}

/// Counts every row written to every table from here on.
async fn count_writes(db: &Database) {
    sqlx::query(r#"CREATE TABLE "TestWrites" ("Tbl" TEXT PRIMARY KEY, "N" INTEGER NOT NULL)"#)
        .execute(db.writer())
        .await
        .expect("counter table");
    let tables: Vec<String> = sqlx::query_scalar(
        r#"SELECT "name" FROM sqlite_master WHERE "type" = 'table'
             AND "name" NOT LIKE 'sqlite_%' AND "name" <> 'TestWrites'"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("tables");
    for table in tables {
        sqlx::query(r#"INSERT INTO "TestWrites" VALUES (?1, 0)"#)
            .bind(&table)
            .execute(db.writer())
            .await
            .expect("counter row");
        for event in ["INSERT", "UPDATE", "DELETE"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"CREATE TRIGGER "TestWrites_{table}_{event}" AFTER {event} ON "{table}"
                   BEGIN UPDATE "TestWrites" SET "N" = "N" + 1 WHERE "Tbl" = '{table}'; END"#
            )))
            .execute(db.writer())
            .await
            .expect("trigger");
        }
    }
}

async fn reset_writes(db: &Database) {
    sqlx::query(r#"UPDATE "TestWrites" SET "N" = 0"#)
        .execute(db.writer())
        .await
        .expect("reset");
}

/// Moves `path`'s mtime `seconds` from now.
fn touch(path: &Path, seconds: i64) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    let now = std::time::SystemTime::now();
    let at = if seconds >= 0 {
        now + std::time::Duration::from_secs(seconds.unsigned_abs())
    } else {
        now - std::time::Duration::from_secs(seconds.unsigned_abs())
    };
    file.set_modified(at).expect("set mtime");
}

fn db_time(at: chrono::DateTime<chrono::Utc>) -> String {
    ferrofin_db::store::datetime_to_db(at)
}

/// Everything the first scan does, done; the counters are then reset.
async fn scanned_once(tmp: &Path, interval_days: i32) -> Fixture {
    let fx = Fixture::new(tmp, interval_days).await;
    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            created: 2,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(
        fx.probe.take(),
        ["Heat (1995).mkv", "The Matrix (1999).mkv"]
    );
    assert!(!fx.tmdb.take().is_empty(), "a new item asks its providers");
    let _ = fx.writes().await;
    fx
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_rescan_probes_nothing_fetches_nothing_and_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let before = (fx.stamps(&fx.matrix).await, fx.stamps(&fx.heat).await);
    assert!(
        before.0.0.is_some(),
        "a new item's first refresh stamps DateLastSaved"
    );
    assert!(before.0.1.is_some(), "and DateLastRefreshed");

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            unchanged: 2,
            ..ScanOutcome::default()
        }
    );
    assert!(fx.probe.take().is_empty(), "no ffprobe");
    assert_eq!(fx.tmdb.take(), Vec::<String>::new(), "no provider request");
    let writes = fx.writes().await;
    assert!(writes.is_empty(), "no row written anywhere: {writes:?}");
    assert_eq!(
        (fx.stamps(&fx.matrix).await, fx.stamps(&fx.heat).await),
        before,
        "DateLastSaved (the Etag input) and DateLastRefreshed stay put"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_touched_file_is_probed_fetched_and_saved_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let heat_before = fx.stamps(&fx.heat).await;
    let matrix_before = fx.stamps(&fx.matrix).await;

    touch(&fx.matrix, 3_600);
    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(fx.probe.take(), ["The Matrix (1999).mkv"]);
    let requests = fx.tmdb.take();
    assert!(
        requests.iter().any(|r| r.contains("/movie/603?")),
        "the changed item runs its remote providers: {requests:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.contains("Heat") || r.contains("/movie/949")),
        "the unchanged one does not: {requests:?}"
    );
    assert_eq!(fx.stamps(&fx.heat).await, heat_before);
    assert_ne!(fx.stamps(&fx.matrix).await.0, matrix_before.0, "saved");

    // And it is quiet again.
    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.probe.take().is_empty());
    assert!(fx.tmdb.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_nfo_newer_than_the_last_save_rereads_only_that_items_local_metadata() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    // The last save was ten minutes ago, the NFO is written now: newer by
    // more than `BaseNfoProvider`'s one-minute tolerance.
    fx.set(
        &fx.heat,
        "DateLastSaved",
        Some(db_time(chrono::Utc::now() - chrono::TimeDelta::minutes(10))),
    )
    .await;
    let nfo = fx.heat.with_extension("nfo");
    std::fs::write(
        &nfo,
        "<movie><title>Heat</title><plot>A thief and a detective.</plot></movie>",
    )
    .expect("nfo");
    let matrix_before = fx.stamps(&fx.matrix).await;

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(fx.probe.take().is_empty(), "the NFO needs no probe");
    assert!(fx.tmdb.take().is_empty(), "and no remote provider");
    let overview: Option<String> =
        sqlx::query_scalar(r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(Fixture::id(&fx.heat))
            .fetch_one(fx.db.pool())
            .await
            .expect("overview");
    assert_eq!(overview.as_deref(), Some("A thief and a detective."));
    assert_eq!(fx.stamps(&fx.matrix).await, matrix_before);

    // Saving moved DateLastSaved past the NFO: quiet again.
    assert_eq!(fx.scan().await.unchanged, 2);

    // An NFO written within a minute of the last save is our own write.
    touch(&nfo, 30);
    assert_eq!(fx.scan().await.unchanged, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_sidecar_subtitle_reprobes_only_that_video() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let sidecar = fx.heat.with_file_name("Heat (1995).eng.srt");
    std::fs::write(&sidecar, b"1\n").expect("srt");

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(fx.probe.take(), ["Heat (1995).eng.srt", "Heat (1995).mkv"]);
    assert!(
        fx.tmdb.take().is_empty(),
        "a sidecar is not a reason to refetch"
    );
    let external: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "ItemId" = ?1 AND "IsExternal" = 1"#,
    )
    .bind(Fixture::id(&fx.heat))
    .fetch_one(fx.db.pool())
    .await
    .expect("streams");
    assert_eq!(external, 1);

    // The stored streams now name the sidecar: quiet again.
    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.probe.take().is_empty());

    // Removing it is a change too.
    std::fs::remove_file(&sidecar).expect("rm");
    assert_eq!(fx.scan().await.updated, 1);
    assert_eq!(fx.probe.take(), ["Heat (1995).mkv"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_never_refreshed_item_runs_everything_once_then_goes_quiet() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    // What every row of a database scanned before this change looks like.
    fx.set(&fx.heat, "DateLastRefreshed", None).await;

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(fx.probe.take(), ["Heat (1995).mkv"]);
    assert!(
        fx.tmdb.take().iter().any(|r| r.contains("/movie/949?")),
        "a first refresh runs every provider"
    );
    assert!(fx.stamps(&fx.heat).await.1.is_some(), "and stamps the row");

    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.probe.take().is_empty());
    assert!(fx.tmdb.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_elapsed_refresh_interval_refetches() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 30).await;
    assert_eq!(fx.scan().await.unchanged, 2, "not yet elapsed");
    assert!(fx.tmdb.take().is_empty());

    fx.set(
        &fx.matrix,
        "DateLastRefreshed",
        Some(db_time(chrono::Utc::now() - chrono::TimeDelta::days(31))),
    )
    .await;
    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    let requests = fx.tmdb.take();
    assert!(
        requests.iter().any(|r| r.contains("/movie/603?")),
        "{requests:?}"
    );
    assert_eq!(fx.probe.take(), ["The Matrix (1999).mkv"]);
    assert_eq!(fx.scan().await.unchanged, 2, "the refetch restamped it");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_provider_is_retried_but_one_that_found_nothing_is_not() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), 0).await;
    fx.tmdb.answer_heat(Answer::Fail);
    assert_eq!(fx.scan().await.created, 2);
    assert!(
        fx.stamps(&fx.heat).await.1.is_none(),
        "a refresh with a provider failure is not stamped"
    );
    assert!(fx.stamps(&fx.matrix).await.1.is_some());
    let _ = (fx.probe.take(), fx.tmdb.take());

    // Still failing: retried (as a first refresh), the other item quiet.
    let outcome = fx.scan().await;
    assert_eq!((outcome.updated, outcome.unchanged), (1, 1));
    let requests = fx.tmdb.take();
    assert!(!requests.is_empty() && requests.iter().all(|r| r.contains("Heat")));
    assert!(fx.stamps(&fx.heat).await.1.is_none());

    // TMDB now answers that it has nothing: that completes the refresh.
    fx.tmdb.answer_heat(Answer::Nothing);
    assert_eq!(fx.scan().await.updated, 1);
    let stamped = fx.stamps(&fx.heat).await;
    assert!(stamped.1.is_some(), "found nothing still stamps");
    let _ = fx.tmdb.take();

    // Upstream would never ask again. Ferrofin's kept backfill heuristic
    // (owner decision D2: no overview → ask TMDB) still does, on the stored
    // row — but an answer of nothing changes nothing, so nothing is written.
    assert_eq!(fx.scan().await.unchanged, 2);
    let requests = fx.tmdb.take();
    assert!(
        !requests.is_empty()
            && requests
                .iter()
                .all(|r| r.contains("/search/movie?") && r.contains("Heat")),
        "only the backfill's search for the title with no overview: {requests:?}"
    );
    assert_eq!(fx.stamps(&fx.heat).await, stamped);
}

/// A music album with a cover beside its tracks stores its shared,
/// content-addressed cover — the loop and the post-scan album pass agree on
/// it, so an unchanged rescan rewrites neither the album's images nor
/// anything else.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_album_with_a_cover_is_not_rewritten() {
    let tmp = tempfile::tempdir().expect("tmp");
    let music = tmp.path().join("music");
    let album = music.join("Great Winds").join("The Sour Kingdom (1996)");
    std::fs::create_dir_all(&album).expect("mkdir");
    std::fs::write(album.join("01 - Opening.mp3"), b"0123").expect("track");
    std::fs::write(album.join("cover.jpg"), b"\xFF\xD8\xFFcover").expect("cover");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
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
    .expect("add library");
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )))
        .with_probe(
            Arc::new(RecordingProbe::default()) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        .with_metadata_dir(tmp.path().join("metadata"))
        .with_progress_every(0);
    count_writes(&db).await;
    let first = scanner.scan_all().await.expect("scan");
    assert!(first.created >= 2, "{first:?}");
    reset_writes(&db).await;

    let rescan = scanner.scan_all().await.expect("rescan");
    assert_eq!(rescan.unchanged, first.created, "{rescan:?}");
    let writes: Vec<(String, i64)> =
        sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
            .fetch_all(db.pool())
            .await
            .expect("writes");
    assert!(writes.is_empty(), "no row written anywhere: {writes:?}");
}

/// Scans `media` as a library of `kind` with the probe stand-in, the item and
/// people repositories, and (when `tmdb` is given) TMDB; counts every row
/// written from here on.
async fn library(
    root: &Path,
    media: &Path,
    kind: CollectionTypeOptions,
    tmdb: Option<&str>,
) -> (Database, LibraryScanner) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Library",
        Some(kind),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    let mut scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )))
        .with_people(Arc::new(ferrofin_core::FerrofinPeopleRepository::new(
            db.clone(),
        )))
        .with_probe(
            Arc::new(RecordingProbe::default()) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        .with_metadata_dir(root.join("metadata"))
        .with_progress_every(0);
    if let Some(base) = tmdb {
        scanner = scanner.with_metadata(
            Arc::new(TmdbClient::new().with_base_url(base)),
            root.join("metadata"),
        );
    }
    count_writes(&db).await;
    (db, scanner)
}

async fn written(db: &Database) -> Vec<(String, i64)> {
    let rows = sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
        .fetch_all(db.pool())
        .await
        .expect("writes");
    reset_writes(db).await;
    rows
}

/// A TMDB stand-in for The Matrix with NO trailers (so the kept D2
/// `wants_trailers` backfill asks again on every scan), a synopsis and a
/// cast that differ from the NFO's. Returns the base URL and a request count.
fn spawn_trailerless_tmdb() -> (String, Arc<Mutex<usize>>) {
    spawn_trailerless_tmdb_with(r#"[{"id": 7, "name": "Tmdb Actor", "character": "Neo"}]"#)
}

/// [`spawn_trailerless_tmdb`] with the given `credits.cast` JSON array.
fn spawn_trailerless_tmdb_with(cast: &'static str) -> (String, Arc<Mutex<usize>>) {
    let requests = Arc::new(Mutex::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let counter = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            *counter.lock().expect("lock") += 1;
            let (status, payload) = if line.contains("/search/movie") {
                (
                    "200 OK",
                    r#"{"results": [{"id": 603, "title": "The Matrix"}]}"#.to_owned(),
                )
            } else if line.contains("/movie/603?") {
                (
                    "200 OK",
                    format!(
                        r#"{{"title": "The Matrix", "overview": "TMDB's synopsis.",
                        "genres": [{{"name": "Action"}}], "credits": {{"cast": {cast}}}}}"#
                    ),
                )
            } else {
                ("404 Not Found", "{}".to_owned())
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), requests)
}

/// The D2 backfill runs the local readers with the remote providers
/// (`MetadataService.cs:689-693`): an NFO's synopsis and cast are kept over
/// TMDB's, and a backfill pass that changes nothing writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_backfill_pass_keeps_the_nfo_and_writes_nothing_when_nothing_changed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media
        .join("The Matrix (1999)")
        .join("The Matrix (1999).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    std::fs::write(
        file.with_extension("nfo"),
        "<movie><title>The Matrix</title><plot>The NFO's synopsis.</plot>\
         <actor><name>Nfo Actor</name><role>Neo</role></actor></movie>",
    )
    .expect("nfo");
    let (base, requests) = spawn_trailerless_tmdb();
    let (db, scanner) = library(
        tmp.path(),
        &media,
        CollectionTypeOptions::movies,
        Some(&base),
    )
    .await;

    let overview_and_cast = || async {
        let id = Fixture::id(&file);
        let overview: Option<String> =
            sqlx::query_scalar(r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&id)
                .fetch_one(db.pool())
                .await
                .expect("overview");
        let cast: Vec<String> = sqlx::query_scalar(
            r#"SELECT p."Name" FROM "PeopleBaseItemMap" m JOIN "Peoples" p ON p."Id" = m."PeopleId"
               WHERE m."ItemId" = ?1 ORDER BY m."ListOrder""#,
        )
        .bind(&id)
        .fetch_all(db.pool())
        .await
        .expect("cast");
        (overview, cast)
    };

    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    let first = overview_and_cast().await;
    assert_eq!(first.0.as_deref(), Some("The NFO's synopsis."));
    assert_eq!(first.1, ["Nfo Actor"]);
    let _ = written(&db).await;
    let asked = *requests.lock().expect("lock");

    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(
        *requests.lock().expect("lock") > asked,
        "no trailers: the backfill asked TMDB again"
    );
    assert_eq!(
        overview_and_cast().await,
        first,
        "the NFO's values are kept"
    );
    assert_eq!(written(&db).await, Vec::<(String, i64)>::new());
}

/// `MergePeople` on the credits a scan saves (`MetadataService.cs:850,1003`,
/// `:1429-1470`): the NFO's cast stands, and TMDB's answer only fills what
/// the NFO left out — a role here — for the people it credits too. TMDB's
/// other people are not added.
#[tokio::test(flavor = "multi_thread")]
async fn tmdb_credits_fill_the_nfo_cast_without_adding_to_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media
        .join("The Matrix (1999)")
        .join("The Matrix (1999).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    std::fs::write(
        file.with_extension("nfo"),
        "<movie><title>The Matrix</title>\
         <actor><name>Keanu Reeves</name></actor>\
         <actor><name>Nfo Actor</name><role>Self</role></actor></movie>",
    )
    .expect("nfo");
    let (base, _requests) = spawn_trailerless_tmdb_with(
        r#"[{"id": 6384, "name": "keanu reeves", "character": "Neo"},
            {"id": 2975, "name": "Laurence Fishburne", "character": "Morpheus"},
            {"id": 7, "name": "Nfo Actor", "character": "Someone Else"}]"#,
    );
    let (db, scanner) = library(
        tmp.path(),
        &media,
        CollectionTypeOptions::movies,
        Some(&base),
    )
    .await;

    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    let cast: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT p."Name", m."Role" FROM "PeopleBaseItemMap" m
           JOIN "Peoples" p ON p."Id" = m."PeopleId"
           WHERE m."ItemId" = ?1 ORDER BY m."ListOrder""#,
    )
    .bind(Fixture::id(&file))
    .fetch_all(db.pool())
    .await
    .expect("cast");
    assert_eq!(
        cast,
        [
            ("Keanu Reeves".to_owned(), "Neo".to_owned()),
            ("Nfo Actor".to_owned(), "Self".to_owned()),
        ]
    );
}

/// Local image validation on an unchanged item: a new or replaced
/// `poster.jpg` is picked up and saved; a deleted one is removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_replaced_or_deleted_poster_is_validated_on_an_unchanged_item() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media.join("Heat (1995)").join("Heat (1995).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies, None).await;
    let images = || async {
        sqlx::query_as::<_, (String, Option<String>)>(
            r#"SELECT "Path", "DateModified" FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#,
        )
        .bind(Fixture::id(&file))
        .fetch_all(db.pool())
        .await
        .expect("images")
    };
    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    assert!(images().await.is_empty());

    let poster = file.with_file_name("poster.jpg");
    std::fs::write(&poster, b"\xFF\xD8\xFFposter").expect("poster");
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    let found = images().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].0.ends_with("poster.jpg"));
    let _ = written(&db).await;
    assert_eq!(scanner.scan_all().await.expect("rescan").unchanged, 1);
    assert_eq!(written(&db).await, Vec::<(String, i64)>::new());

    touch(&poster, 3_600);
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    assert_ne!(images().await[0].1, found[0].1, "the replacement's mtime");

    std::fs::remove_file(&poster).expect("rm");
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    assert!(
        images().await.is_empty(),
        "a vanished image's row is removed"
    );
    assert_eq!(scanner.scan_all().await.expect("rescan").unchanged, 1);
}

/// A new episode in an existing season: it is created, its season (whose
/// directory mtime moved) is refreshed, and its sibling is left alone. The
/// series is written only for the date of its newest episode
/// (`UpdateDateLastMediaAdded`, which upstream's refresh of the series
/// saves), and so its `DateLastSaved` moves.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_episode_refreshes_its_season_and_leaves_its_siblings_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let tv = tmp.path().join("tv");
    let season = tv.join("Show").join("Season 1");
    std::fs::create_dir_all(&season).expect("mkdir");
    std::fs::write(season.join("Show S01E01.mkv"), b"0123").expect("write");
    let (db, scanner) = library(tmp.path(), &tv, CollectionTypeOptions::tvshows, None).await;
    assert_eq!(scanner.scan_all().await.expect("scan").created, 3);
    let saved = |path: PathBuf, kind: BaseItemKind| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                r#"SELECT "DateLastSaved" FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(
                derive_item_id(kind, &path.to_string_lossy()).expect("id"),
            ))
            .fetch_one(db.pool())
            .await
            .expect("row")
        }
    };
    let sibling = season.join("Show S01E01.mkv");
    let before = (
        saved(tv.join("Show"), BaseItemKind::Series).await,
        saved(sibling.clone(), BaseItemKind::Episode).await,
        saved(season.clone(), BaseItemKind::Season).await,
    );
    // The directory mtime moves by whole seconds only on some filesystems;
    // make the drift unambiguous.
    std::fs::write(season.join("Show S01E02.mkv"), b"0123").expect("new episode");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
    std::fs::File::open(&season)
        .expect("season dir")
        .set_modified(later)
        .expect("touch dir");

    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            created: 1,
            updated: 1,
            unchanged: 2,
            ..ScanOutcome::default()
        }
    );
    assert_ne!(
        saved(tv.join("Show"), BaseItemKind::Series).await,
        before.0,
        "its DateLastMediaAdded moved"
    );
    assert_eq!(saved(sibling, BaseItemKind::Episode).await, before.1);
    assert_ne!(saved(season, BaseItemKind::Season).await, before.2);
}

/// A locked item: an unchanged rescan leaves it alone like any other; a
/// changed file still refreshes its file facts (the probe is a forced
/// provider) but no remote provider runs for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_item_is_quiet_and_asks_no_provider() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Id" = ?1"#)
        .bind(Fixture::id(&fx.heat))
        .execute(fx.db.writer())
        .await
        .expect("lock");
    let _ = fx.writes().await;
    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.writes().await.is_empty());

    touch(&fx.heat, 3_600);
    assert_eq!(fx.scan().await.updated, 1);
    assert_eq!(fx.probe.take(), ["Heat (1995).mkv"]);
    assert!(
        fx.tmdb.take().is_empty(),
        "no remote provider runs for a locked item"
    );
    assert_eq!(fx.scan().await.unchanged, 2);
}

/// A backfill pass compares the cast the way `update_people` writes it:
/// deduped on (name, type), names trimmed and matched case-insensitively.
/// TMDB credits one actor in two roles all the time; that must not read as a
/// changed cast (and rewrite the title) on every scan.
#[tokio::test(flavor = "multi_thread")]
async fn a_cast_with_one_person_in_two_roles_is_unchanged_on_a_backfill_pass() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media
        .join("The Matrix (1999)")
        .join("The Matrix (1999).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    let (base, requests) = spawn_trailerless_tmdb_with(
        r#"[{"id": 7, "name": "Tmdb Actor", "character": "Neo"},
            {"id": 7, "name": "Tmdb Actor", "character": "Thomas Anderson"},
            {"id": 8, "name": " tmdb actor ", "character": "Echo"},
            {"id": 9, "name": "Other Actor", "character": "Trinity"}]"#,
    );
    let (db, scanner) = library(
        tmp.path(),
        &media,
        CollectionTypeOptions::movies,
        Some(&base),
    )
    .await;
    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    let _ = written(&db).await;
    let asked = *requests.lock().expect("lock");

    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(
        *requests.lock().expect("lock") > asked,
        "the backfill asked again"
    );
    assert_eq!(written(&db).await, Vec::<(String, i64)>::new());
}

/// A movie row's stored `(CommunityRating, Overview)`.
async fn rating_and_overview(fx: &Fixture, path: &Path) -> (Option<f64>, Option<String>) {
    sqlx::query_as(r#"SELECT "CommunityRating", "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(Fixture::id(path))
        .fetch_one(fx.db.pool())
        .await
        .expect("row")
}

/// Phase 3L: an edit made without `LockData` survives a rescan that runs no
/// provider (the item is unchanged). A rescan whose providers run (the file
/// changed) replaces the unlocked fields with the provider's values, as
/// upstream's Default merge does, but leaves a field in the item's
/// `LockedFields` as the user set it.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_survives_a_quiet_rescan_and_a_provider_pass_only_where_locked() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let id = Fixture::id(&fx.matrix);
    fx.set(&fx.matrix, "Overview", Some("My overview".into()))
        .await;

    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.tmdb.take().is_empty());
    assert_eq!(
        rating_and_overview(&fx, &fx.matrix).await.1.as_deref(),
        Some("My overview"),
        "no provider ran: the edit stands"
    );
    fx.set(&fx.matrix, "CommunityRating", Some("5".into()))
        .await;

    // Lock the Overview field (`MetadataField.Overview = 6`), then change
    // the file so the providers run.
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (6, ?1)"#)
        .bind(&id)
        .execute(fx.db.writer())
        .await
        .expect("lock overview");
    touch(&fx.matrix, 3_600);
    assert_eq!(fx.scan().await.updated, 1);
    assert!(!fx.tmdb.take().is_empty(), "the providers ran");
    assert_eq!(
        rating_and_overview(&fx, &fx.matrix).await,
        (Some(8.0), Some("My overview".into())),
        "the unlocked rating is replaced; the locked overview is kept"
    );

    // Unlocked, the next provider pass replaces the overview too.
    sqlx::query(r#"DELETE FROM "BaseItemMetadataFields" WHERE "ItemId" = ?1"#)
        .bind(&id)
        .execute(fx.db.writer())
        .await
        .expect("unlock");
    touch(&fx.matrix, 7_200);
    assert_eq!(fx.scan().await.updated, 1);
    assert_eq!(
        rating_and_overview(&fx, &fx.matrix).await.1.as_deref(),
        Some("About The Matrix.")
    );
}

/// Phase 3L: `LockData` refuses every remote provider — even when the file
/// changed, which would run them all for an unlocked item — but the local
/// image validation still runs, so a new `poster.jpg` is discovered
/// (`ProviderManager.CanRefreshImages` enables every `ILocalImageProvider`
/// before its `IsLocked` check).
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_item_asks_no_provider_but_discovers_a_new_local_poster() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let id = Fixture::id(&fx.heat);
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Id" = ?1"#)
        .bind(&id)
        .execute(fx.db.writer())
        .await
        .expect("lock");
    let images = || async {
        sqlx::query_scalar::<_, String>(
            r#"SELECT "Path" FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#,
        )
        .bind(&id)
        .fetch_all(fx.db.pool())
        .await
        .expect("images")
    };
    assert!(images().await.is_empty());

    std::fs::write(fx.heat.with_file_name("poster.jpg"), b"\xFF\xD8\xFFposter").expect("poster");
    touch(&fx.heat, 3_600);
    assert_eq!(fx.scan().await.updated, 1);
    assert!(
        fx.tmdb.take().is_empty(),
        "no remote provider runs for a locked item"
    );
    let found = images().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ends_with("poster.jpg"));
}

/// Phase 3L: a `Cast` field lock keeps the stored credits through a
/// provider pass that returns a different cast (upstream leaves
/// `metadata.People` null under the lock, and `SaveItemAsync` writes people
/// only when non-null). Unlocked, the next pass takes the provider's cast.
#[tokio::test(flavor = "multi_thread")]
async fn a_cast_lock_keeps_the_stored_credits_through_a_provider_pass() {
    use ferrofin_db::entities::base_items::PeopleEntity;
    use ferrofin_traits::persistence::PeopleRepository as _;
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media
        .join("The Matrix (1999)")
        .join("The Matrix (1999).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    let (base, _requests) = spawn_trailerless_tmdb();
    let (db, scanner) = library(
        tmp.path(),
        &media,
        CollectionTypeOptions::movies,
        Some(&base),
    )
    .await;
    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    let people = ferrofin_core::FerrofinPeopleRepository::new(db.clone());
    let item = derive_item_id(BaseItemKind::Movie, &file.to_string_lossy()).expect("id");
    let names = || async {
        people
            .get_people_batch(&[item])
            .await
            .expect("people")
            .remove(&item)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names().await, ["Tmdb Actor"]);

    // The user's own cast, locked (`MetadataField.Cast = 0`).
    people
        .update_people(
            item,
            &[PeopleEntity {
                name: "My Actor".into(),
                person_type: Some("Actor".into()),
                ..PeopleEntity::default()
            }],
        )
        .await
        .expect("user cast");
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (0, ?1)"#)
        .bind(guid_to_db(item))
        .execute(db.writer())
        .await
        .expect("lock cast");
    // A backfill pass (no trailers) and a full provider pass (file changed).
    scanner.scan_all().await.expect("backfill rescan");
    assert_eq!(names().await, ["My Actor"], "the backfill kept the lock");
    touch(&file, 3_600);
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    assert_eq!(
        names().await,
        ["My Actor"],
        "the provider pass kept the lock"
    );

    sqlx::query(r#"DELETE FROM "BaseItemMetadataFields""#)
        .execute(db.writer())
        .await
        .expect("unlock");
    touch(&file, 7_200);
    scanner.scan_all().await.expect("rescan");
    assert_eq!(
        names().await,
        ["Tmdb Actor"],
        "unlocked: the provider's cast"
    );
}

/// Phase 3L: an NFO's `<lockdata>` and `<lockedfields>` (the parser reads
/// both, as `BaseNfoParser` does). `<lockdata>true` makes the pass
/// `isLocalLocked` — no remote provider runs for it — and the merge's
/// metadata-settings half locks the item; `<lockedfields>` are unioned into
/// the item's stored set (`MetadataService.cs:873,1365-1379`).
#[tokio::test(flavor = "multi_thread")]
async fn an_nfo_lockdata_locks_the_item_and_its_lockedfields_join_the_set() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), 0).await;
    std::fs::write(
        fx.matrix.with_extension("nfo"),
        "<movie><title>The Matrix</title><plot>From the NFO.</plot>\
         <lockdata>true</lockdata><lockedfields>Overview|Cast</lockedfields></movie>",
    )
    .expect("nfo");
    std::fs::write(
        fx.heat.with_extension("nfo"),
        "<movie><title>Heat</title><lockedfields>Genres</lockedfields></movie>",
    )
    .expect("nfo");
    assert_eq!(fx.scan().await.created, 2);
    let requests = fx.tmdb.take();
    // The remote metadata provider (the `/movie/{id}` details fetch) does
    // not run for the NFO-locked item. Its remote image lookup still may:
    // upstream picks the image providers before the pass, on the item's
    // stored `IsLocked`.
    assert!(
        requests.iter().all(|r| !r.contains("/movie/603")),
        "no remote metadata for the NFO-locked item: {requests:?}"
    );
    assert!(requests.iter().any(|r| r.contains("Heat")), "{requests:?}");
    let row: (i64, Option<String>) =
        sqlx::query_as(r#"SELECT "IsLocked", "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(Fixture::id(&fx.matrix))
            .fetch_one(fx.db.pool())
            .await
            .expect("row");
    assert_eq!(row, (1, Some("From the NFO.".to_owned())));
    let locks = |path: &Path| {
        let id = Fixture::id(path);
        let db = fx.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                r#"SELECT "Id" FROM "BaseItemMetadataFields" WHERE "ItemId" = ?1 ORDER BY "Id""#,
            )
            .bind(id)
            .fetch_all(db.pool())
            .await
            .expect("locks")
        }
    };
    // Cast = 0, Overview = 6; Genres = 1.
    assert_eq!(locks(&fx.matrix).await, [0, 6]);
    assert_eq!(locks(&fx.heat).await, [1]);
    let heat_locked: i64 =
        sqlx::query_scalar(r#"SELECT "IsLocked" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(Fixture::id(&fx.heat))
            .fetch_one(fx.db.pool())
            .await
            .expect("row");
    assert_eq!(heat_locked, 0, "no <lockdata>: not locked");
    let _ = fx.writes().await;
    assert_eq!(fx.scan().await.unchanged, 2, "and a rescan is quiet");
}

/// A movie row's stored `(Overview, CommunityRating, Tagline)`.
async fn edited_fields(fx: &Fixture, path: &Path) -> (Option<String>, Option<f64>, Option<String>) {
    sqlx::query_as(
        r#"SELECT "Overview", "CommunityRating", "Tagline" FROM "BaseItems" WHERE "Id" = ?1"#,
    )
    .bind(Fixture::id(path))
    .fetch_one(fx.db.pool())
    .await
    .expect("row")
}

/// The Matrix after its first scan, then edited: the overview rewritten, the
/// rating emptied, and a tagline added that no provider supplies. The
/// refresh stamp is moved back an hour so a new one shows.
async fn edited_matrix(tmp: &Path) -> Fixture {
    let fx = scanned_once(tmp, 0).await;
    fx.set(&fx.matrix, "Overview", Some("My overview".into()))
        .await;
    fx.set(&fx.matrix, "CommunityRating", None).await;
    fx.set(&fx.matrix, "Tagline", Some("My tagline".into()))
        .await;
    let an_hour_ago = db_time(chrono::Utc::now() - chrono::TimeDelta::hours(1));
    fx.set(&fx.matrix, "DateLastRefreshed", Some(an_hour_ago.clone()))
        .await;
    fx.set(&fx.heat, "DateLastRefreshed", Some(an_hour_ago))
        .await;
    fx
}

/// The options `POST /Items/{id}/Refresh` builds for a mode pair.
fn item_refresh(mode: MetadataRefreshMode, replace_all: bool) -> MetadataRefreshOptions {
    MetadataRefreshOptions::for_item_refresh(mode, mode, replace_all, false, false)
}

/// "Scan for new and updated files" (`Default`/`Default`), `ValidationOnly`
/// and `None` run no provider for an unchanged item: nothing is asked,
/// nothing written, the edits stand.
#[tokio::test(flavor = "multi_thread")]
async fn default_validation_only_and_none_ask_no_provider() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    let before = edited_fields(&fx, &fx.matrix).await;
    for mode in [
        MetadataRefreshMode::Default,
        MetadataRefreshMode::ValidationOnly,
        MetadataRefreshMode::None,
    ] {
        assert_eq!(
            fx.scan_with(&item_refresh(mode, false)).await,
            ScanOutcome {
                unchanged: 2,
                ..ScanOutcome::default()
            },
            "{mode:?}"
        );
        assert!(fx.tmdb.take().is_empty(), "{mode:?}: no provider request");
        assert!(fx.probe.take().is_empty(), "{mode:?}: no probe");
        assert!(fx.writes().await.is_empty(), "{mode:?}: nothing written");
        assert_eq!(edited_fields(&fx, &fx.matrix).await, before, "{mode:?}");
    }
}

/// "Search for missing metadata" (`FullRefresh`, no replace): every provider
/// runs for every item, the answer only fills what is empty — the edited
/// overview and the tagline stay, the emptied rating is filled — and every
/// item is saved (`ForceSave`) with a new `DateLastRefreshed`.
#[tokio::test(flavor = "multi_thread")]
async fn search_for_missing_metadata_fills_gaps_and_keeps_edits() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    let stamped = fx.stamps(&fx.matrix).await.1;

    let outcome = fx
        .scan_with(&item_refresh(MetadataRefreshMode::FullRefresh, false))
        .await;
    assert_eq!(
        outcome,
        ScanOutcome {
            updated: 2,
            ..ScanOutcome::default()
        },
        "ForceSave saves every item"
    );
    let asked = fx.tmdb.take();
    assert!(
        asked.iter().any(|l| l.contains("/movie/603?"))
            && asked.iter().any(|l| l.contains("/movie/949?")),
        "every provider runs for every item: {asked:?}"
    );
    assert_eq!(
        fx.probe.take(),
        ["Heat (1995).mkv", "The Matrix (1999).mkv"],
        "the probe runs too"
    );
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await,
        (
            Some("My overview".into()),
            Some(8.0),
            Some("My tagline".into())
        )
    );
    assert_ne!(
        fx.stamps(&fx.matrix).await.1,
        stamped,
        "DateLastRefreshed is stamped"
    );
}

/// "Replace all metadata" (`FullRefresh` + `ReplaceAllMetadata` +
/// `RemoveOldMetadata`): the answer replaces the row — the edited overview
/// and the emptied rating take TMDB's values — and what no provider
/// re-supplied (the tagline) is cleared. A locked field keeps its value.
#[tokio::test(flavor = "multi_thread")]
async fn replace_all_metadata_replaces_and_clears_but_keeps_locked_fields() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    let replace = item_refresh(MetadataRefreshMode::FullRefresh, true);

    assert_eq!(fx.scan_with(&replace).await.updated, 2);
    assert!(!fx.tmdb.take().is_empty());
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await,
        (Some("About The Matrix.".into()), Some(8.0), None)
    );

    // Lock the Overview field (`MetadataField.Overview = 6`) and edit again.
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (6, ?1)"#)
        .bind(Fixture::id(&fx.matrix))
        .execute(fx.db.writer())
        .await
        .expect("lock overview");
    fx.set(&fx.matrix, "Overview", Some("My overview".into()))
        .await;
    fx.set(&fx.matrix, "CommunityRating", Some("5".into()))
        .await;
    fx.set(&fx.matrix, "Tagline", Some("My tagline".into()))
        .await;
    assert_eq!(fx.scan_with(&replace).await.updated, 2);
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await,
        (Some("My overview".into()), Some(8.0), None),
        "the locked overview is kept; the rest is replaced"
    );
}

/// "Replace all metadata" erases only when something replaces the old
/// values: when every remote provider failed with no answer, the stored row
/// is kept (`MetadataService.cs:897-906`), and the failed pass is not
/// stamped, so it is retried.
#[tokio::test(flavor = "multi_thread")]
async fn replace_all_metadata_keeps_the_row_when_every_provider_failed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    fx.set(&fx.heat, "Overview", Some("Kept".into())).await;
    fx.set(&fx.heat, "Tagline", Some("Kept tagline".into()))
        .await;
    let stamped = fx.stamps(&fx.heat).await.1;
    fx.tmdb.answer_heat(Answer::Fail);

    fx.scan_with(&item_refresh(MetadataRefreshMode::FullRefresh, true))
        .await;
    assert_eq!(
        edited_fields(&fx, &fx.heat).await,
        (Some("Kept".into()), Some(8.0), Some("Kept tagline".into()))
    );
    assert_eq!(fx.stamps(&fx.heat).await.1, stamped, "not stamped");
}

/// A movie row's `(Name, Overview, CommunityRating, Tagline, Data)`.
type Metadata = (
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<String>,
);

/// A locked item (`LockData`) under each of the dashboard's three choices —
/// "Scan for new and updated files", "Search for missing metadata" and
/// "Replace all metadata": no remote metadata provider is asked for it, and
/// its metadata stays exactly as it was — the edited overview, the emptied
/// rating, the tagline no provider supplies and its `Data` (the trailers) —
/// while the unlocked title beside it is refreshed (`CanRefreshMetadata`,
/// `ProviderManager.cs:588-592`; `RefreshWithProviders` returns on
/// `IsLocked`, `MetadataService.cs:785-788`). Its remote image providers
/// run on an image full refresh only (`CanRefreshImages`,
/// `ProviderManager.cs:438`).
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_item_keeps_its_metadata_and_asks_no_provider_in_every_mode() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Id" = ?1"#)
        .bind(Fixture::id(&fx.matrix))
        .execute(fx.db.writer())
        .await
        .expect("lock");
    let metadata = || async {
        sqlx::query_as::<_, Metadata>(
            r#"SELECT "Name", "Overview", "CommunityRating", "Tagline", "Data"
               FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(Fixture::id(&fx.matrix))
        .fetch_one(fx.db.pool())
        .await
        .expect("row")
    };
    let before = metadata().await;
    assert!(
        before
            .4
            .as_deref()
            .is_some_and(|data| data.contains("RemoteTrailers")),
        "{before:?}"
    );

    for (mode, replace_all) in [
        (MetadataRefreshMode::Default, false),
        (MetadataRefreshMode::FullRefresh, false),
        (MetadataRefreshMode::FullRefresh, true),
    ] {
        fx.scan_with(&item_refresh(mode, replace_all)).await;
        let asked = fx.tmdb.take();
        // TMDB's details (`append_to_response`) or a search by the title is
        // the metadata provider; the bare `/movie/{id}` lookup is its image
        // provider (`TmdbClient::images_by_id`).
        let for_matrix = |line: &&String| line.contains("/movie/603?") || line.contains("Matrix");
        assert!(
            !asked
                .iter()
                .filter(for_matrix)
                .any(|l| l.contains("append_to_response") || l.contains("/search/")),
            "{mode:?}, replace all {replace_all}: no metadata asked for the locked item: \
             {asked:?}"
        );
        assert_eq!(
            asked.iter().filter(for_matrix).count(),
            usize::from(mode == MetadataRefreshMode::FullRefresh),
            "{mode:?}, replace all {replace_all}: its image provider runs on an image full \
             refresh only: {asked:?}"
        );
        if mode == MetadataRefreshMode::FullRefresh {
            assert!(
                asked.iter().any(|l| l.contains("/movie/949?")),
                "{mode:?}, replace all {replace_all}: the unlocked title is refreshed: {asked:?}"
            );
        }
        assert_eq!(
            metadata().await,
            before,
            "{mode:?}, replace all {replace_all}"
        );
        let _ = fx.probe.take();
    }
}

/// `POST /Items/{id}/Refresh` on a file item, as the library manager queues
/// it (`ScanRequest::item_refresh`): the scan of the item itself with the
/// request's options, the folders above it carried along with `None`/`None`,
/// no pruning and only the touched items' closing passes.
async fn refresh_item(fx: &Fixture, path: &Path, options: &MetadataRefreshOptions) -> ScanOutcome {
    use ferrofin_core::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    fx.scanner
        .scan_target(
            &ScanTarget::Items(vec![path.to_string_lossy().into_owned()]),
            ScanRun::new(options, &none, &ScanCancel::new())
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await
        .expect("item refresh")
}

/// An item's own refresh removes nothing (upstream's `RefreshSingleItem`
/// deletes no item): with its file gone, the row — and the user data on it —
/// stays for the library scan or a folder refresh, which do remove it.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_removes_nothing() {
    use ferrofin_core::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    std::fs::remove_file(&fx.heat).expect("rm");
    let options = item_refresh(MetadataRefreshMode::Default, false);
    let outcome = refresh_item(&fx, &fx.heat, &options).await;
    assert_eq!(
        outcome,
        ScanOutcome::default(),
        "nothing planned, nothing removed"
    );
    assert!(
        fx.stamps(&fx.heat).await.1.is_some(),
        "the row is still there"
    );

    let folder = fx
        .heat
        .parent()
        .expect("dir")
        .to_string_lossy()
        .into_owned();
    let outcome = fx
        .scanner
        .scan_target(
            &ScanTarget::Paths(vec![folder]),
            ScanRun::new(&options, &options, &ScanCancel::new())
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await
        .expect("folder refresh");
    assert_eq!(outcome.removed, 1, "a folder refresh prunes: {outcome:?}");
}

/// A file item's "Scan for new and updated files" refresh of an unchanged
/// item is quiet — no provider, no probe, no write — and plans nothing but
/// that item.
#[tokio::test(flavor = "multi_thread")]
async fn a_default_item_refresh_of_an_unchanged_item_is_quiet() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    let before = edited_fields(&fx, &fx.matrix).await;
    assert_eq!(
        refresh_item(
            &fx,
            &fx.matrix,
            &item_refresh(MetadataRefreshMode::Default, false)
        )
        .await,
        ScanOutcome {
            unchanged: 1,
            ..ScanOutcome::default()
        },
        "only the item is planned"
    );
    assert!(fx.tmdb.take().is_empty());
    assert!(fx.probe.take().is_empty());
    assert!(fx.writes().await.is_empty());
    assert_eq!(edited_fields(&fx, &fx.matrix).await, before);
}

/// A file item's "Search for missing metadata": its providers and its probe
/// run (upstream probes inside `RefreshMetadata`, so the handler's separate
/// re-probe is gone), the answer fills only the emptied rating, the edits
/// stay, the item is saved and stamped — and nothing else of the library is
/// asked about or written.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_searching_for_missing_metadata_fills_gaps_keeps_edits_and_probes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    let stamped = fx.stamps(&fx.matrix).await.1;
    let heat = fx.stamps(&fx.heat).await;

    let outcome = refresh_item(
        &fx,
        &fx.matrix,
        &item_refresh(MetadataRefreshMode::FullRefresh, false),
    )
    .await;
    assert_eq!(
        outcome,
        ScanOutcome {
            updated: 1,
            ..ScanOutcome::default()
        }
    );
    let asked = fx.tmdb.take();
    assert!(asked.iter().any(|l| l.contains("/movie/603?")), "{asked:?}");
    assert!(
        asked
            .iter()
            .all(|l| !l.contains("Heat") && !l.contains("/949")),
        "nothing about another item: {asked:?}"
    );
    assert_eq!(fx.probe.take(), ["The Matrix (1999).mkv"]);
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await,
        (
            Some("My overview".into()),
            Some(8.0),
            Some("My tagline".into())
        )
    );
    assert_ne!(fx.stamps(&fx.matrix).await.1, stamped, "stamped");
    assert_eq!(fx.stamps(&fx.heat).await, heat, "the sibling is untouched");
}

/// A file item's "Replace all metadata": TMDB's answer replaces the edited
/// overview and clears the tagline nothing re-supplied — except a locked
/// field, which keeps the user's value.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_replacing_all_metadata_replaces_but_keeps_locked_fields() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    let replace = item_refresh(MetadataRefreshMode::FullRefresh, true);
    assert_eq!(refresh_item(&fx, &fx.matrix, &replace).await.updated, 1);
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await,
        (Some("About The Matrix.".into()), Some(8.0), None)
    );

    // `MetadataField.Overview = 6`.
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (6, ?1)"#)
        .bind(Fixture::id(&fx.matrix))
        .execute(fx.db.writer())
        .await
        .expect("lock overview");
    fx.set(&fx.matrix, "Overview", Some("My overview".into()))
        .await;
    fx.set(&fx.matrix, "Tagline", Some("My tagline".into()))
        .await;
    assert_eq!(refresh_item(&fx, &fx.matrix, &replace).await.updated, 1);
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await,
        (Some("My overview".into()), Some(8.0), None),
        "the locked overview is kept; the rest is replaced"
    );
}

/// A file item's "Replace all metadata" whose provider fails keeps the
/// stored row (erasing is only safe when something replaces it) and is not
/// stamped, so it is retried.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_replacing_all_keeps_the_row_nothing_answered_for() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    fx.set(&fx.heat, "Overview", Some("Kept".into())).await;
    fx.set(&fx.heat, "Tagline", Some("Kept tagline".into()))
        .await;
    let stamped = fx.stamps(&fx.heat).await.1;
    fx.tmdb.answer_heat(Answer::Fail);

    refresh_item(
        &fx,
        &fx.heat,
        &item_refresh(MetadataRefreshMode::FullRefresh, true),
    )
    .await;
    assert_eq!(
        edited_fields(&fx, &fx.heat).await,
        (Some("Kept".into()), Some(8.0), Some("Kept tagline".into()))
    );
    assert_eq!(fx.stamps(&fx.heat).await.1, stamped, "not stamped");
}

/// A file item's refresh reads its NFO — the single-item refresh never did
/// (it was TMDB-only): a Default refresh reads an NFO written since the last
/// save (`BaseNfoProvider.HasChanged`) and its values replace the stored
/// ones, with no remote provider asked; a full refresh reads it as well.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_reads_the_items_nfo() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    std::fs::write(
        fx.matrix.with_extension("nfo"),
        "<movie><title>The Matrix</title><plot>From the NFO.</plot><mpaa>R</mpaa></movie>",
    )
    .expect("nfo");
    // The last save an hour ago: the NFO is newer by more than a minute.
    let an_hour_ago = db_time(chrono::Utc::now() - chrono::TimeDelta::hours(1));
    fx.set(&fx.matrix, "DateLastSaved", Some(an_hour_ago)).await;

    let outcome = refresh_item(
        &fx,
        &fx.matrix,
        &item_refresh(MetadataRefreshMode::Default, false),
    )
    .await;
    assert_eq!(outcome.updated, 1);
    assert!(fx.tmdb.take().is_empty(), "local metadata only");
    let row: (Option<String>, Option<String>) =
        sqlx::query_as(r#"SELECT "Overview", "OfficialRating" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(Fixture::id(&fx.matrix))
            .fetch_one(fx.db.pool())
            .await
            .expect("row");
    assert_eq!(row, (Some("From the NFO.".into()), Some("R".into())));

    fx.set(&fx.matrix, "OfficialRating", None).await;
    refresh_item(
        &fx,
        &fx.matrix,
        &item_refresh(MetadataRefreshMode::FullRefresh, false),
    )
    .await;
    let rating: Option<String> =
        sqlx::query_scalar(r#"SELECT "OfficialRating" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(Fixture::id(&fx.matrix))
            .fetch_one(fx.db.pool())
            .await
            .expect("row");
    assert_eq!(rating.as_deref(), Some("R"), "the full refresh read it too");
}

/// The options `POST /Items/RemoteSearch/Apply/{id}` refreshes with for
/// `result`.
fn apply(result: ferrofin_model::providers::RemoteSearchResult) -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        search_result: Some(result),
        remove_old_metadata: true,
        ..MetadataRefreshOptions::default()
    }
}

/// The item's stored provider ids.
async fn provider_ids(fx: &Fixture, path: &Path) -> Vec<(String, String)> {
    sqlx::query_as(
        r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders" WHERE "ItemId" = ?1
             ORDER BY "ProviderId""#,
    )
    .bind(Fixture::id(path))
    .fetch_all(fx.db.pool())
    .await
    .expect("ids")
}

/// "Identify → Apply" through the scan of the item's path: the chosen TMDB
/// id is fetched directly (no search by the item's own name, no id it
/// carried before), the NFO beside it is not read ("Do not execute local
/// providers if we are identifying"), the answer replaces the row and
/// clears what it did not supply, a locked field keeps the user's value, and
/// the item ends up carrying the chosen id.
#[tokio::test(flavor = "multi_thread")]
async fn identify_pins_the_chosen_id_skips_the_nfo_and_keeps_locked_fields() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = edited_matrix(tmp.path()).await;
    std::fs::write(
        fx.matrix.with_extension("nfo"),
        "<movie><title>The Matrix</title><plot>From the NFO.</plot><mpaa>R</mpaa>\
         <tmdbid>603</tmdbid></movie>",
    )
    .expect("nfo");
    // `MetadataField.Tags = 5` is not the one locked; `Overview = 6` is.
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (6, ?1)"#)
        .bind(Fixture::id(&fx.matrix))
        .execute(fx.db.writer())
        .await
        .expect("lock overview");

    let chosen = ferrofin_model::providers::RemoteSearchResult {
        name: Some("Heat".into()),
        production_year: Some(1995),
        provider_ids: Some(HashMap::from([("Tmdb".to_owned(), "949".to_owned())])),
        search_provider_name: Some("TheMovieDb".into()),
        ..Default::default()
    };
    let outcome = refresh_item(&fx, &fx.matrix, &apply(chosen)).await;
    assert_eq!(outcome.updated, 1);
    let asked = fx.tmdb.take();
    assert!(asked.iter().any(|l| l.contains("/movie/949?")), "{asked:?}");
    assert!(
        asked
            .iter()
            .all(|l| !l.contains("/search/") && !l.contains("/movie/603")),
        "the chosen id is fetched, never the old one or a search: {asked:?}"
    );
    let row: (Option<String>, Option<f64>, Option<String>, Option<String>) = sqlx::query_as(
        r#"SELECT "Overview", "CommunityRating", "Tagline", "OfficialRating"
             FROM "BaseItems" WHERE "Id" = ?1"#,
    )
    .bind(Fixture::id(&fx.matrix))
    .fetch_one(fx.db.pool())
    .await
    .expect("row");
    assert_eq!(
        row,
        (Some("My overview".into()), Some(8.0), None, None),
        "locked overview kept; the rest replaced; no NFO value"
    );
    assert_eq!(
        provider_ids(&fx, &fx.matrix).await,
        [("Tmdb".to_owned(), "949".to_owned())],
        "the chosen ids replaced the old ones with the refresh's save"
    );
}

/// An Identify result that carries only an IMDb id (OMDb's) is resolved by
/// TMDB through `/find` (`TmdbMovieProvider.GetMetadata`) rather than by a
/// search on the item's name.
#[tokio::test(flavor = "multi_thread")]
async fn identify_with_an_imdb_id_resolves_through_tmdb_find() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let chosen = ferrofin_model::providers::RemoteSearchResult {
        name: Some("Heat".into()),
        provider_ids: Some(HashMap::from([("Imdb".to_owned(), "tt0113277".to_owned())])),
        search_provider_name: Some("The Open Movie Database".into()),
        ..Default::default()
    };
    refresh_item(&fx, &fx.matrix, &apply(chosen)).await;
    let asked = fx.tmdb.take();
    assert!(
        asked.iter().any(|l| l.contains("/find/tt0113277")),
        "{asked:?}"
    );
    assert!(asked.iter().all(|l| !l.contains("/search/")), "{asked:?}");
    assert_eq!(
        edited_fields(&fx, &fx.matrix).await.0.as_deref(),
        Some("About Heat.")
    );
}

/// An OMDb stand-in knowing one title, Heat (`tt0113277`), recording each
/// request line; any other lookup finds nothing.
struct Omdb {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Omdb {
    fn spawn() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                log.lock().expect("lock").push(line.clone());
                let payload = if line.contains("i=tt0113277") {
                    r#"{"Title":"Heat","Year":"1995","Plot":"From OMDb.","imdbRating":"8.3",
                        "imdbID":"tt0113277","Response":"True",
                        "Ratings":[{"Source":"Rotten Tomatoes","Value":"87%"}]}"#
                } else {
                    r#"{"Response":"False","Error":"Movie not found!"}"#
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        Self {
            base: format!("http://{addr}/"),
            requests,
        }
    }

    /// The request lines since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().expect("lock"))
    }
}

/// An identified movie's overview, community rating, critic rating,
/// premiere date and `Data`.
type IdentifiedRow = (
    Option<String>,
    Option<f64>,
    Option<f64>,
    Option<String>,
    Option<String>,
);

/// "Identify → Apply" of an OMDb result: OMDb runs first ("When
/// identifying, run the provider the user picked first", `MetadataService.
/// cs:876-882`) and TheMovieDb still runs after it (`ExecuteRemoteProviders`
/// runs every provider, `:968-1023`) — resolving the title by the IMDb id
/// OMDb's answer carries (`MergeNewData`). The first answer wins a field
/// and the later one fills what it left empty (`MergeData(result, temp, [],
/// false, false)`, `:1003`): OMDb's plot and ratings stand, TMDB adds the
/// premiere date and the trailer OMDb has none of. The first-hit chain
/// stopped at OMDb's answer and never asked TMDB.
#[tokio::test(flavor = "multi_thread")]
async fn identify_with_omdb_first_then_tmdb_fills_what_omdb_left() {
    let tmp = tempfile::tempdir().expect("tmp");
    let omdb = Omdb::spawn();
    let fx = Fixture::with_omdb(tmp.path(), 0, Some(&omdb.base)).await;
    fx.scan().await;
    let _ = (fx.tmdb.take(), omdb.take(), fx.writes().await);

    let chosen = ferrofin_model::providers::RemoteSearchResult {
        name: Some("Heat".into()),
        provider_ids: Some(HashMap::from([("Imdb".to_owned(), "tt0113277".to_owned())])),
        search_provider_name: Some("The Open Movie Database".into()),
        ..Default::default()
    };
    refresh_item(&fx, &fx.matrix, &apply(chosen)).await;

    let by_omdb = omdb.take();
    let by_the_moviedb = fx.tmdb.take();
    assert!(
        by_omdb.iter().any(|l| l.contains("i=tt0113277")),
        "OMDb looked the chosen id up: {by_omdb:?}"
    );
    assert!(
        by_the_moviedb.iter().any(|l| l.contains("/find/tt0113277"))
            && by_the_moviedb.iter().any(|l| l.contains("/movie/949?")),
        "TheMovieDb ran after OMDb, by the IMDb id: {by_the_moviedb:?}"
    );
    assert!(
        by_the_moviedb.iter().all(|l| !l.contains("/search/")),
        "no search by name: {by_the_moviedb:?}"
    );
    let row: IdentifiedRow = sqlx::query_as(
        r#"SELECT "Overview", "CommunityRating", "CriticRating", "PremiereDate", "Data"
             FROM "BaseItems" WHERE "Id" = ?1"#,
    )
    .bind(Fixture::id(&fx.matrix))
    .fetch_one(fx.db.pool())
    .await
    .expect("row");
    assert_eq!(row.0.as_deref(), Some("From OMDb."), "OMDb's plot, first");
    assert!(
        row.1.is_some_and(|r| (r - 8.3).abs() < 1e-4),
        "OMDb's IMDb rating, not TMDB's 8.0: {:?}",
        row.1
    );
    assert_eq!(row.2, Some(87.0), "OMDb's Rotten Tomatoes score");
    assert!(
        row.3
            .as_deref()
            .is_some_and(|d| d.starts_with("1999-03-30")),
        "TMDB's release date fills the gap: {:?}",
        row.3
    );
    assert!(
        row.4
            .as_deref()
            .is_some_and(|d| d.contains("RemoteTrailers")),
        "TMDB's trailer fills the gap: {:?}",
        row.4
    );
    let ids = provider_ids(&fx, &fx.matrix).await;
    assert!(
        ids.contains(&("Imdb".to_owned(), "tt0113277".to_owned()))
            && ids.contains(&("Tmdb".to_owned(), "949".to_owned())),
        "{ids:?}"
    );
}

/// One of the fixture's two movies, as the other one is identified as.
#[derive(Clone, Copy)]
struct Title {
    tmdb: &'static str,
    name: &'static str,
    year: i32,
}

const MATRIX: Title = Title {
    tmdb: "603",
    name: "The Matrix",
    year: 1999,
};
const HEAT: Title = Title {
    tmdb: "949",
    name: "Heat",
    year: 1995,
};

/// The options "Identify → Apply" refreshes with when `title` is chosen.
fn identify_as(title: Title) -> MetadataRefreshOptions {
    apply(ferrofin_model::providers::RemoteSearchResult {
        name: Some(title.name.into()),
        production_year: Some(title.year),
        provider_ids: Some(HashMap::from([("Tmdb".to_owned(), title.tmdb.to_owned())])),
        search_provider_name: Some("TheMovieDb".into()),
        ..Default::default()
    })
}

/// A [`PriorityLane`] that, once the scan it is served by has fetched the
/// first of the fixture's movies from TMDB, hands out one "Identify →
/// Apply" of the *other* movie — the one the scan has read but not reached
/// yet — as the first one, recording which it picked and how it ended.
struct IdentifyTheOther {
    requests: Arc<Mutex<Vec<String>>>,
    paths: [(PathBuf, Title); 2],
    picked: Mutex<Option<(PathBuf, Title)>>,
    served: Mutex<Vec<Result<ScanOutcome, String>>>,
}

impl IdentifyTheOther {
    fn new(fx: &Fixture) -> Self {
        Self {
            requests: Arc::clone(&fx.tmdb.requests),
            paths: [(fx.matrix.clone(), MATRIX), (fx.heat.clone(), HEAT)],
            picked: Mutex::new(None),
            served: Mutex::new(Vec::new()),
        }
    }

    /// The movie identified, and the title it was identified as.
    fn picked(&self) -> (PathBuf, Title) {
        self.picked
            .lock()
            .expect("lock")
            .clone()
            .expect("an Apply was served")
    }
}

impl ferrofin_core::PriorityLane for IdentifyTheOther {
    fn next(&self) -> Option<ferrofin_core::LaneRefresh> {
        let mut picked = self.picked.lock().expect("lock");
        if picked.is_some() {
            return None;
        }
        let fetched = |t: Title| {
            let details = format!("/movie/{}?", t.tmdb);
            self.requests
                .lock()
                .expect("lock")
                .iter()
                .any(|l| l.contains(&details))
        };
        let [(matrix, _), (heat, _)] = &self.paths;
        let (path, title) = if fetched(MATRIX) {
            (heat.clone(), MATRIX)
        } else if fetched(HEAT) {
            (matrix.clone(), HEAT)
        } else {
            return None;
        };
        *picked = Some((path.clone(), title));
        let none = MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::None,
            image_refresh_mode: MetadataRefreshMode::None,
            ..MetadataRefreshOptions::default()
        };
        Some(ferrofin_core::LaneRefresh {
            target: ferrofin_traits::library::ScanTarget::Items(vec![
                path.to_string_lossy().into_owned(),
            ]),
            options: identify_as(title),
            ancestors: none,
            passes: ferrofin_core::ScanPasses::Touched,
            cancel: ferrofin_core::ScanCancel::new(),
            key: 1,
        })
    }
    fn done(
        &self,
        _refresh: ferrofin_core::LaneRefresh,
        outcome: &Result<ScanOutcome, ServiceError>,
    ) {
        let outcome = outcome.as_ref().copied().map_err(ToString::to_string);
        self.served.lock().expect("lock").push(outcome);
    }
}

/// Runs a library scan with `options` that serves `lane` between its items.
async fn scan_serving(
    fx: &Fixture,
    options: &MetadataRefreshOptions,
    lane: &IdentifyTheOther,
) -> ScanOutcome {
    use ferrofin_core::{ScanCancel, ScanRun};
    let cancel = ScanCancel::new();
    fx.scanner
        .scan_target(
            &ferrofin_traits::library::ScanTarget::All,
            ScanRun::new(options, options, &cancel).with_lane(lane),
        )
        .await
        .expect("scan")
}

/// The identified movie carries the chosen title's id and record.
async fn assert_identified(fx: &Fixture, lane: &IdentifyTheOther) {
    let (path, title) = lane.picked();
    assert_eq!(
        provider_ids(fx, &path).await,
        [("Tmdb".to_owned(), title.tmdb.to_owned())],
        "{} keeps the chosen id",
        path.display()
    );
    assert_eq!(
        edited_fields(fx, &path).await.0,
        Some(format!("About {}.", title.name)),
        "{} keeps the chosen record",
        path.display()
    );
}

/// An Apply served inside a library's first scan, after that scan read the
/// second movie as new (no row) and before it reached it: the scan reads
/// the row the Apply wrote before going on, so it neither looks the movie
/// up by its own name nor saves its first-scan answer over the identified
/// one.
#[tokio::test(flavor = "multi_thread")]
async fn an_apply_served_inside_a_first_scan_survives_the_rest_of_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), 0).await;
    let lane = IdentifyTheOther::new(&fx);
    let outcome = scan_serving(&fx, &MetadataRefreshOptions::default(), &lane).await;
    let served = lane.served.lock().expect("lock").clone();
    assert_eq!(served.len(), 1, "served inside the scan");
    assert_eq!(
        served[0].as_ref().expect("apply").created,
        1,
        "served before the scan reached the movie"
    );
    assert_eq!(outcome.created, 1, "the scan created only the other one");
    let (path, _) = lane.picked();
    let own_name = if path == fx.heat { "Heat" } else { "Matrix" };
    let asked = fx.tmdb.take();
    let searches: Vec<&String> = asked.iter().filter(|l| l.contains("/search/")).collect();
    assert_eq!(
        searches.len(),
        1,
        "one search, for the scanned movie: {asked:?}"
    );
    assert!(
        !searches[0].contains(own_name),
        "the identified movie was never searched by its own name: {asked:?}"
    );
    assert_identified(&fx, &lane).await;
}

/// An Apply served inside a "Replace all metadata" library scan, after that
/// scan read the second movie's row and ids and before it reached it: the
/// scan refreshes that movie by the ids now stored, so the identified ids
/// and record survive it, and its old id is never fetched again.
#[tokio::test(flavor = "multi_thread")]
async fn an_apply_served_inside_a_replace_all_scan_survives_the_rest_of_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let lane = IdentifyTheOther::new(&fx);
    let replace_all = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        ..MetadataRefreshOptions::default()
    };
    scan_serving(&fx, &replace_all, &lane).await;
    assert_eq!(lane.served.lock().expect("lock").len(), 1);
    let (_, title) = lane.picked();
    // Its old id is the other title's, which nothing in this scan fetches
    // but a save of what the scan read before the Apply.
    let old = if title.tmdb == MATRIX.tmdb {
        HEAT
    } else {
        MATRIX
    };
    let asked = fx.tmdb.take();
    let details = format!("/movie/{}?", old.tmdb);
    assert!(
        asked.iter().all(|l| !l.contains(&details)),
        "the old id is never fetched again: {asked:?}"
    );
    assert_identified(&fx, &lane).await;
}

/// A TMDB stand-in for one show folder that two TMDB shows could be: the
/// search finds 100 ("Wrong Show", episodes `W1`…), 200 is "Right Show"
/// (episodes `R1`…). Records each request line.
fn spawn_two_shows_tmdb() -> (String, Arc<Mutex<Vec<String>>>) {
    fn season(prefix: &str) -> String {
        let episodes: Vec<String> = (1..=3)
            .map(|n| {
                format!(
                    r#"{{"id": {n}, "episode_number": {n}, "name": "{prefix}{n}",
                        "overview": "{prefix}{n} overview.", "air_date": "2001-01-0{n}"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"name": "Season 1", "overview": "{prefix} season.", "episodes": [{}]}}"#,
            episodes.join(",")
        )
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default().to_owned();
            log.lock().expect("lock").push(line.clone());
            let (status, payload) = if line.contains("/credits") {
                (
                    "200 OK",
                    r#"{"cast": [], "crew": [], "guest_stars": []}"#.to_owned(),
                )
            } else if line.contains("/search/tv") {
                (
                    "200 OK",
                    r#"{"results": [{"id": 100, "name": "Wrong Show"}]}"#.to_owned(),
                )
            } else if line.contains("/tv/100/season/1?") {
                ("200 OK", season("W"))
            } else if line.contains("/tv/200/season/1?") {
                ("200 OK", season("R"))
            } else if line.contains("/tv/100?") {
                (
                    "200 OK",
                    r#"{"name": "Wrong Show", "overview": "About Wrong."}"#.to_owned(),
                )
            } else if line.contains("/tv/200?") {
                (
                    "200 OK",
                    r#"{"name": "Right Show", "overview": "About Right."}"#.to_owned(),
                )
            } else {
                ("404 Not Found", "{}".to_owned())
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), requests)
}

/// A lane holding one refresh, handed out once `ready` says so.
struct ReadyWhen {
    ready: Box<dyn Fn() -> bool + Send + Sync>,
    pending: Mutex<Option<ferrofin_core::LaneRefresh>>,
    served: Mutex<Vec<Result<ScanOutcome, String>>>,
}

impl ferrofin_core::PriorityLane for ReadyWhen {
    fn next(&self) -> Option<ferrofin_core::LaneRefresh> {
        if (self.ready)() {
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
        let outcome = outcome.as_ref().copied().map_err(ToString::to_string);
        self.served.lock().expect("lock").push(outcome);
    }
}

/// A TV library of one show folder (three episodes) over the two-shows
/// TMDB stand-in, scanned once: `(db, scanner, show folder, requests)`.
async fn two_shows_library(
    tmp: &Path,
) -> (Database, LibraryScanner, PathBuf, Arc<Mutex<Vec<String>>>) {
    let shows = tmp.join("shows");
    let show = shows.join("Show");
    for n in 1..=3 {
        let file = show.join("Season 1").join(format!("Show S01E0{n}.mkv"));
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
        std::fs::write(&file, b"0123456789").expect("write");
    }
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Shows",
        Some(CollectionTypeOptions::tvshows),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: shows.to_string_lossy().into_owned(),
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
    let (base, requests) = spawn_two_shows_tmdb();
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(items)
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.join("metadata"),
        )
        .with_progress_every(0);
    scanner.scan_all().await.expect("first scan");
    (db, scanner, show, requests)
}

/// An Identify of a series served inside a "Replace all metadata" scan,
/// after that scan fetched the season of the show it had matched before:
/// the episodes it reaches afterwards take the identified show's names —
/// the scan drops the season details it cached for the series along with
/// its match, instead of mixing the old show's episodes in.
#[tokio::test(flavor = "multi_thread")]
async fn an_identified_series_episodes_never_take_the_old_shows_season() {
    use ferrofin_core::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    let tmp = tempfile::tempdir().expect("tmp");
    let (db, scanner, show, requests) = two_shows_library(tmp.path()).await;
    let names = |db: Database| async move {
        let rows: Vec<(Option<String>,)> = sqlx::query_as(
            r#"SELECT "Name" FROM "BaseItems" WHERE "Type" LIKE '%TV.Episode'
                 ORDER BY "IndexNumber""#,
        )
        .fetch_all(db.pool())
        .await
        .expect("episodes");
        rows.into_iter()
            .map(|(n,)| n.unwrap_or_default())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(db.clone()).await, ["W1", "W2", "W3"]);
    requests.lock().expect("lock").clear();

    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    let right = ferrofin_model::providers::RemoteSearchResult {
        name: Some("Right Show".into()),
        provider_ids: Some(HashMap::from([("Tmdb".to_owned(), "200".to_owned())])),
        search_provider_name: Some("TheMovieDb".into()),
        ..Default::default()
    };
    let seen = Arc::clone(&requests);
    let lane = ReadyWhen {
        // The scan has fetched the old show's season: it is cached now.
        ready: Box::new(move || {
            seen.lock()
                .expect("lock")
                .iter()
                .any(|l| l.contains("/tv/100/season/1?"))
        }),
        pending: Mutex::new(Some(ferrofin_core::LaneRefresh {
            target: ScanTarget::Paths(vec![show.to_string_lossy().into_owned()]),
            options: apply(right),
            ancestors: none,
            passes: ferrofin_core::ScanPasses::Touched,
            cancel: ScanCancel::new(),
            key: 1,
        })),
        served: Mutex::new(Vec::new()),
    };
    let replace_all = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        ..MetadataRefreshOptions::default()
    };
    let cancel = ScanCancel::new();
    scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&replace_all, &replace_all, &cancel).with_lane(&lane),
        )
        .await
        .expect("scan");
    let served = lane.served.lock().expect("lock").clone();
    assert_eq!(served.len(), 1, "served inside the scan");
    assert!(served[0].is_ok(), "{served:?}");
    assert_eq!(
        names(db.clone()).await,
        ["R1", "R2", "R3"],
        "every episode is the identified show's: {:?}",
        requests.lock().expect("lock")
    );
}

/// A movie row's stored `ProductionYear`.
async fn year(fx: &Fixture, path: &Path) -> Option<i64> {
    sqlx::query_scalar(r#"SELECT "ProductionYear" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(Fixture::id(path))
        .fetch_one(fx.db.pool())
        .await
        .expect("row")
}

/// A movie's year the user cleared comes back from its path only on a pass
/// that runs `BeforeMetadataRefresh` — one with providers to run (here the
/// probe of a file that changed; TMDB fails, so it supplies no year) — and
/// then from its folder's name, as upstream's `Movie.BeforeMetadataRefresh`
/// derives it. A Default rescan with nothing to run writes nothing back.
#[tokio::test(flavor = "multi_thread")]
async fn a_cleared_movie_year_refills_only_on_a_pass_that_runs_providers() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    assert_eq!(year(&fx, &fx.heat).await, Some(1999), "TMDB's release date");
    fx.set(&fx.heat, "ProductionYear", None).await;

    fx.scan().await;
    assert_eq!(year(&fx, &fx.heat).await, None);
    assert!(fx.writes().await.is_empty(), "nothing written back");

    fx.tmdb.answer_heat(Answer::Fail);
    touch(&fx.heat, 120);
    fx.scan().await;
    assert_eq!(
        year(&fx, &fx.heat).await,
        Some(1995),
        "the year in the folder's name"
    );
}

/// "Identify → Apply" with a record that has no year clears the year
/// (`RemoveOldMetadata`); a Default rescan with nothing to run leaves it
/// cleared, and the next pass that runs providers derives it from the
/// folder's name again, as upstream does.
#[tokio::test(flavor = "multi_thread")]
async fn an_identify_that_clears_the_year_holds_through_a_quiet_rescan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let nameless = ferrofin_model::providers::RemoteSearchResult {
        name: Some("Nameless".into()),
        provider_ids: Some(HashMap::from([("Tmdb".to_owned(), "111".to_owned())])),
        search_provider_name: Some("TheMovieDb".into()),
        ..Default::default()
    };
    refresh_item(&fx, &fx.matrix, &apply(nameless)).await;
    assert_eq!(year(&fx, &fx.matrix).await, None, "the record has no year");
    let _ = fx.writes().await;

    fx.scan().await;
    assert_eq!(year(&fx, &fx.matrix).await, None);
    assert!(fx.writes().await.is_empty(), "nothing written back");

    touch(&fx.matrix, 120);
    fx.scan().await;
    assert_eq!(year(&fx, &fx.matrix).await, Some(1999), "the folder's year");
}

/// A locked season with no number is renumbered from its folder on a
/// "Replace all metadata" refresh: a locked item keeps its local providers,
/// so `BeforeMetadataRefresh` runs for it — while nothing is read or
/// fetched for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_seasons_number_is_refilled_on_a_full_refresh() {
    use ferrofin_core::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    let tmp = tempfile::tempdir().expect("tmp");
    let (db, scanner, show, _requests) = two_shows_library(tmp.path()).await;
    let season = r#""Type" LIKE '%TV.Season'"#;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"UPDATE "BaseItems" SET "IsLocked" = 1, "IndexNumber" = NULL WHERE {season}"#
    )))
    .execute(db.writer())
    .await
    .expect("lock");
    let replace_all = item_refresh(MetadataRefreshMode::FullRefresh, true);
    let cancel = ScanCancel::new();
    scanner
        .scan_target(
            &ScanTarget::Paths(vec![show.join("Season 1").to_string_lossy().into_owned()]),
            ScanRun::new(&replace_all, &replace_all, &cancel)
                .with_passes(ferrofin_core::ScanPasses::Touched),
        )
        .await
        .expect("folder refresh");
    let number: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        r#"SELECT "IndexNumber" FROM "BaseItems" WHERE {season}"#
    )))
    .fetch_one(db.pool())
    .await
    .expect("season");
    assert_eq!(number, Some(1));
}

/// Jellyfin stores a local movie version as a generic Video with a different
/// path-derived id. Scanning it as a new Movie duplicated it in the library.
#[tokio::test(flavor = "multi_thread")]
async fn adopted_generic_video_version_keeps_its_identity_across_scans() {
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    let tmp = tempfile::tempdir().unwrap();
    let fx = scanned_once(tmp.path(), 0).await;
    let primary = Fixture::id(&fx.matrix);
    let path = fx
        .matrix
        .parent()
        .unwrap()
        .join("The Matrix (1999) - alternate.mkv");
    std::fs::write(&path, b"0123456789").unwrap();
    let mut alternate: BaseItemEntity = sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Id"=?"#)
        .bind(&primary)
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    let id = derive_item_id(BaseItemKind::Video, &path.to_string_lossy()).unwrap();
    alternate.id = guid_to_db(id);
    alternate.path = Some(path.to_string_lossy().into_owned());
    alternate.type_ = "MediaBrowser.Controller.Entities.Video".to_owned();
    alternate.is_movie = false;
    alternate.primary_version_id = Some(primary.clone());
    let persistence = FerrofinItemPersistenceService::new(fx.db.clone());
    persistence.save_items(&[alternate]).await.unwrap();
    for _ in 0..2 {
        fx.scan().await;
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as(r#"SELECT "Id", "PrimaryVersionId" FROM "BaseItems" WHERE "Path"=?"#)
                .bind(path.to_string_lossy().as_ref())
                .fetch_all(fx.db.pool())
                .await
                .unwrap();
        assert_eq!(rows, vec![(guid_to_db(id), Some(primary.clone()))]);
    }
}

/// An NFO wins initial discovery. On Search for missing metadata the prober
/// first changes the stored title, which the fill-only NFO merge then keeps.
/// A Name lock still preserves the user's title on a changed-file scan.
#[tokio::test]
async fn embedded_title_obeys_refresh_merge_and_name_locks() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(tmp.path(), 0).await;
    let mut options = fixture
        .vf
        .get_virtual_folders()
        .await
        .unwrap()
        .remove(0)
        .library_options
        .unwrap();
    options.enable_embedded_titles = true;
    options.type_options = vec![ferrofin_model::configuration::TypeOptions {
        type_: Some("Movie".to_owned()),
        ..Default::default()
    }];
    fixture
        .vf
        .update_library_options("Movies", &options)
        .await
        .unwrap();
    let mut info = ferrofin_model::media_info::MediaInfo::default();
    info.media_source.name = Some("Embedded".to_owned());
    info.media_source.container = Some("matroska".to_owned());
    *fixture.probe.metadata.lock().unwrap() = Some(info);
    std::fs::write(
        fixture.heat.with_extension("nfo"),
        "<movie><title>NFO title</title></movie>",
    )
    .unwrap();
    fixture.scan().await;
    let names = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT Name FROM BaseItems WHERE Type LIKE '%Movie' ORDER BY Path",
        )
        .fetch_all(fixture.db.pool())
        .await
        .unwrap()
    };
    assert_eq!(names().await, ["NFO title", "Embedded"]);
    fixture
        .set(&fixture.matrix, "Name", Some("Edited".to_owned()))
        .await;
    touch(&fixture.matrix, 5);
    fixture
        .scan_with(&MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
            image_refresh_mode: MetadataRefreshMode::None,
            ..Default::default()
        })
        .await;
    assert_eq!(names().await, ["Embedded", "Embedded"]);
    fixture
        .set(&fixture.matrix, "Name", Some("Locked".to_owned()))
        .await;
    sqlx::query("INSERT INTO BaseItemMetadataFields (Id, ItemId) VALUES (?1, ?2)")
        .bind(ferrofin_db::enums::metadata_field::to_i32(
            ferrofin_model::entities::MetadataField::Name,
        ))
        .bind(Fixture::id(&fixture.matrix))
        .execute(fixture.db.writer())
        .await
        .unwrap();
    touch(&fixture.matrix, 10);
    fixture.scan().await;
    assert_eq!(names().await, ["Embedded", "Locked"]);
}
