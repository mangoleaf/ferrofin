//! The library monitor's scan — the disk watcher and the *arr webhooks —
//! end to end through a real scan (`PLAN_SCAN_CHANGE_DETECTION` Phase 6W):
//! a reported path refreshes the nearest existing item at or above it and
//! validates that item's subtree, as upstream's `FileRefresher` does
//! (`FileRefresher.cs:135-208`: `GetAffectedBaseItem` → `ChangedExternally`
//! → `ProviderManager.RefreshItem`), and nothing else:
//!
//! - a new episode in an existing season: it is created, its season (the
//!   nearest existing item) refreshes through the decision, the series above
//!   it is context only — no provider asked for it, nothing of it written
//!   but its newest-media date — and the season's other episodes are only
//!   validated;
//! - a removed episode, or a removed season folder, prunes exactly its rows,
//!   and a virtual season its last loose episode leaves empty goes with it;
//! - a new season folder creates the season and its episodes, and its series
//!   (the nearest existing item) goes through the decision;
//! - a new loose episode of a series with no season folders gets the virtual
//!   season a library scan would give it;
//! - 1,000 new files are one scan that lists only their series' folders;
//! - the library-wide closing passes are left to the next validation, whose
//!   persisted selections pick the new work up;
//! - every item the scan processed is logged at `debug` with why.

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
use ferrofin_model::io::FileSystemEntryInfo;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository, MediaStreamRepository};

/// An ffprobe stand-in: every file has a video and an audio stream, a
/// `.srt` one subtitle stream.
#[derive(Default)]
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
        let path = request.media_source.path.clone().unwrap_or_default();
        if Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("srt"))
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
        Ok(MediaSourceInfo {
            run_time_ticks: Some(36_000_000_000),
            bitrate: Some(4_000_000),
            media_streams: vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("h264".to_owned()),
                    ..MediaStream::default()
                },
                MediaStream {
                    index: 1,
                    stream_type: MediaStreamType::Audio,
                    codec: Some("aac".to_owned()),
                    ..MediaStream::default()
                },
            ],
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(
        &self,
        _path: &str,
        _image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        unreachable!("no audio in these libraries")
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

/// A TMDB stand-in for TV, recording each request line: every series is
/// 1399, with trailers (so no backfill heuristic asks again on its own),
/// and every season lists its episodes.
struct Tmdb {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Tmdb {
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
                let line = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                log.lock().expect("lock").push(line.clone());
                let (status, body) = if line.contains("/search/tv") {
                    (
                        "200 OK",
                        r#"{"results": [{"id": 1399, "name": "Show"}]}"#.to_owned(),
                    )
                } else if line.contains("/tv/1399/season/") {
                    (
                        "200 OK",
                        r#"{"name": "Season", "overview": "A season.", "episodes": [
                            {"episode_number": 1, "name": "One", "overview": "First."},
                            {"episode_number": 2, "name": "Two", "overview": "Second."},
                            {"episode_number": 3, "name": "Three", "overview": "Third."}]}"#
                            .to_owned(),
                    )
                } else if line.contains("/tv/1399?") {
                    (
                        "200 OK",
                        r#"{"name": "Show", "overview": "About Show.",
                            "videos": {"results": [{"site": "YouTube", "type": "Trailer",
                                "key": "kShow", "name": "Trailer"}]}}"#
                            .to_owned(),
                    )
                } else {
                    ("404 Not Found", "{}".to_owned())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            base: format!("http://{addr}"),
            requests,
        }
    }

    /// The request lines since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().expect("lock"))
    }
}

/// The requests among `lines` that are about the series itself — its search,
/// its details, its images — rather than one of its seasons or episodes.
fn series_requests(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|l| {
            l.contains("/search/tv")
                || l.contains("/tv/1399?")
                || (l.contains("/tv/1399/") && !l.contains("/tv/1399/season/"))
        })
        .collect()
}

/// The real filesystem, recording every directory listed.
#[derive(Default)]
struct CountingFs {
    listed: Mutex<Vec<String>>,
}

impl CountingFs {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.listed.lock().expect("lock"))
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
        FerrofinFileSystem::new().try_get_file_system_entries(path)
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

/// A library of `kind` over `media`, scanned by a scanner with the probe
/// stand-in, TMDB, the item repository and the counting filesystem wired.
struct Fixture {
    db: Database,
    scanner: LibraryScanner,
    persistence: Arc<FerrofinItemPersistenceService>,
    tmdb: Tmdb,
    fs: Arc<CountingFs>,
    media: PathBuf,
}

impl Fixture {
    async fn new(root: &Path, media: &Path, kind: CollectionTypeOptions) -> Self {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
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
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let tmdb = Tmdb::spawn();
        let fs = Arc::new(CountingFs::default());
        let scanner = LibraryScanner::new(vf, fs.clone(), persistence.clone())
            .with_items(items)
            .with_probe(
                Arc::new(Probe) as Arc<dyn MediaEncoder>,
                Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                    as Arc<dyn MediaStreamRepository>,
                Arc::new(FerrofinChapterRepository::new(db.clone())),
            )
            .with_metadata(
                Arc::new(TmdbClient::new().with_base_url(&tmdb.base)),
                root.join("metadata"),
            )
            .with_progress_every(0);
        Self {
            db,
            scanner,
            persistence,
            tmdb,
            fs,
            media: media.to_path_buf(),
        }
    }

    /// A full scan, then the recorders cleared.
    async fn scanned(self) -> Self {
        let first = self.scanner.scan_all().await.expect("scan");
        assert!(first.created > 0, "{first:?}");
        self.tmdb.take();
        self.fs.take();
        self
    }

    /// The watcher's scan of `paths`.
    async fn report(&self, paths: &[&Path]) -> ScanOutcome {
        let paths: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.scanner.scan_paths(&paths).await.expect("scan")
    }

    /// Every `BaseItems` row: its id, and the columns a write moves.
    async fn rows(&self) -> HashMap<String, Row> {
        let rows: Vec<Row> = sqlx::query_as(
            r#"SELECT "Id", "Type", "ParentId", "DateLastSaved", "DateLastRefreshed",
                      "DateModified", "DateLastMediaAdded"
                 FROM "BaseItems" WHERE "TopParentId" IS NOT NULL AND "TopParentId" <> ''"#,
        )
        .fetch_all(self.db.pool())
        .await
        .expect("rows");
        rows.into_iter().map(|r| (r.0.clone(), r)).collect()
    }
}

/// `(Id, Type, ParentId, DateLastSaved, DateLastRefreshed, DateModified,
/// DateLastMediaAdded)`.
type Row = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn id(kind: BaseItemKind, path: &Path) -> String {
    guid_to_db(derive_item_id(kind, &path.to_string_lossy()).expect("id"))
}

fn touch(path: &Path, file: &str) {
    std::fs::create_dir_all(path).expect("mkdir");
    std::fs::write(path.join(file), b"0123").expect("write");
}

/// Moves a directory's mtime an hour on, so the drift is unambiguous on
/// every filesystem (some keep whole seconds only).
fn move_mtime(dir: &Path) {
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
    std::fs::File::open(dir)
        .expect("dir")
        .set_modified(later)
        .expect("set mtime");
}

/// Sets a directory's mtime back to what `rows` stored for `item`, so its
/// refresh decision sees no change.
fn restore_mtime(dir: &Path, stored: &Row) {
    let at = chrono::NaiveDateTime::parse_from_str(
        stored.5.as_deref().expect("stored DateModified"),
        "%Y-%m-%d %H:%M:%S%.f",
    )
    .expect("parse")
    .and_utc();
    let at = std::time::UNIX_EPOCH
        + std::time::Duration::from_nanos(
            u64::try_from(at.timestamp_nanos_opt().expect("in range")).expect("after the epoch"),
        );
    std::fs::File::open(dir)
        .expect("dir")
        .set_modified(at)
        .expect("set mtime");
}

/// A TV library: one series with two episodes in `Season 1` and two in
/// `Season 2`, and another series.
async fn tv(tmp: &Path) -> Fixture {
    let media = tmp.join("tv");
    for season in ["Season 1", "Season 2"] {
        for n in 1..=2 {
            let s = &season[7..];
            touch(
                &media.join("Show").join(season),
                &format!("Show S0{s}E0{n}.mkv"),
            );
        }
    }
    touch(&media.join("Other").join("Season 1"), "Other S01E01.mkv");
    Fixture::new(tmp, &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await
}

/// A new episode in an existing season: exactly one row is created; the
/// season — the nearest existing item, whose folder changed — refreshes
/// through the decision; the series is context only (no provider asked for
/// it, and of its row only the newest-media date the aggregate pass derives
/// moves); the season's other episodes are validated and not written.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_episode_is_created_and_only_its_season_refreshes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let series = fx.media.join("Show");
    let season = series.join("Season 1");
    let before = fx.rows().await;

    touch(&season, "Show S01E03.mkv");
    move_mtime(&season);
    let episode = season.join("Show S01E03.mkv");
    let outcome = fx.report(&[&episode]).await;
    assert_eq!(
        outcome,
        ScanOutcome {
            created: 1,
            updated: 1,
            // The series (context) and the season's two other episodes.
            unchanged: 3,
            ..ScanOutcome::default()
        }
    );
    let after = fx.rows().await;
    let created: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(created, vec![&id(BaseItemKind::Episode, &episode)]);
    assert_eq!(
        after[&id(BaseItemKind::Episode, &episode)].2.as_deref(),
        Some(id(BaseItemKind::Season, &season).as_str()),
        "parented to its season"
    );
    let written: Vec<&String> = before
        .keys()
        .filter(|k| after.get(*k) != before.get(*k))
        .collect();
    let season_id = id(BaseItemKind::Season, &season);
    let series_id = id(BaseItemKind::Series, &series);
    assert!(
        written.iter().all(|k| **k == season_id || **k == series_id),
        "only the season and the series' aggregate are written: {written:?}"
    );
    assert_ne!(
        after[&season_id].4, before[&season_id].4,
        "the season refreshed (its folder changed)"
    );
    let (series_before, series_after) = (&before[&series_id], &after[&series_id]);
    assert_eq!(
        (series_after.4.as_ref(), series_after.5.as_ref()),
        (series_before.4.as_ref(), series_before.5.as_ref()),
        "the series is not refreshed"
    );
    let asked = fx.tmdb.take();
    assert!(
        series_requests(&asked).is_empty(),
        "no provider is asked about the series: {asked:?}"
    );
}

/// A removed episode prunes exactly its rows (its streams with it); its
/// season refreshes, its siblings stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_episode_prunes_exactly_its_rows() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let season = fx.media.join("Show").join("Season 1");
    let gone = season.join("Show S01E02.mkv");
    let sibling = season.join("Show S01E01.mkv");
    let before = fx.rows().await;

    std::fs::remove_file(&gone).expect("rm");
    move_mtime(&season);
    let outcome = fx.report(&[&gone]).await;
    assert_eq!(outcome.removed, 1, "{outcome:?}");
    assert_eq!(outcome.created, 0, "{outcome:?}");
    let after = fx.rows().await;
    assert_eq!(after.len(), before.len() - 1);
    assert!(!after.contains_key(&id(BaseItemKind::Episode, &gone)));
    assert_eq!(
        after.get(&id(BaseItemKind::Episode, &sibling)),
        before.get(&id(BaseItemKind::Episode, &sibling)),
        "the sibling is untouched"
    );
    let streams: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "ItemId" = ?1"#)
            .bind(id(BaseItemKind::Episode, &gone))
            .fetch_one(fx.db.pool())
            .await
            .expect("count");
    assert_eq!(streams, 0, "its streams went with it");
    assert!(series_requests(&fx.tmdb.take()).is_empty());
}

/// A removed season folder prunes its whole subtree — the season and its
/// episodes — and nothing else; its series (the nearest existing item)
/// goes through the decision.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_season_folder_prunes_its_subtree() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let series = fx.media.join("Show");
    let season = series.join("Season 2");
    let before = fx.rows().await;

    std::fs::remove_dir_all(&season).expect("rm");
    move_mtime(&series);
    let outcome = fx.report(&[&season]).await;
    assert_eq!(
        outcome.removed, 3,
        "the season and its two episodes: {outcome:?}"
    );
    let after = fx.rows().await;
    let gone: Vec<&String> = before.keys().filter(|k| !after.contains_key(*k)).collect();
    assert_eq!(gone.len(), 3);
    assert!(gone.contains(&&id(BaseItemKind::Season, &season)));
    for n in 1..=2 {
        assert!(gone.contains(&&id(
            BaseItemKind::Episode,
            &season.join(format!("Show S02E0{n}.mkv"))
        )));
    }
    let other = fx
        .media
        .join("Other")
        .join("Season 1")
        .join("Other S01E01.mkv");
    assert_eq!(
        after.get(&id(BaseItemKind::Episode, &other)),
        before.get(&id(BaseItemKind::Episode, &other)),
        "another series is untouched"
    );
}

/// A new season folder with three episodes: the season and the episodes are
/// created. Its series is the nearest existing item, so it goes through the
/// refresh decision — upstream's `FileRefresher` refreshes it: with its
/// folder's mtime moved (D3) it refreshes, and with the mtime as stored it
/// asks and writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_season_folder_creates_the_season_and_its_episodes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let series = fx.media.join("Show");
    let series_id = id(BaseItemKind::Series, &series);
    let before = fx.rows().await;

    // The series' folder mtime as stored: the decision finds nothing to do.
    let season = series.join("Season 3");
    let episodes: Vec<PathBuf> = (1..=3)
        .map(|n| {
            touch(&season, &format!("Show S03E0{n}.mkv"));
            season.join(format!("Show S03E0{n}.mkv"))
        })
        .collect();
    restore_mtime(&series, &before[&series_id]);
    let mut reported: Vec<&Path> = vec![&season];
    reported.extend(episodes.iter().map(PathBuf::as_path));
    let outcome = fx.report(&reported).await;
    assert_eq!(
        outcome.created, 4,
        "the season and three episodes: {outcome:?}"
    );
    assert_eq!(outcome.removed, 0);
    let after = fx.rows().await;
    for episode in &episodes {
        assert_eq!(
            after[&id(BaseItemKind::Episode, episode)].2.as_deref(),
            Some(id(BaseItemKind::Season, &season).as_str())
        );
    }
    assert_eq!(
        after[&series_id].4, before[&series_id].4,
        "an unchanged series is not refreshed"
    );
    assert!(series_requests(&fx.tmdb.take()).is_empty());

    // Another new season, with the series' folder mtime moved: the decision
    // refreshes the series (D3), as upstream's refresh of it does.
    let season = series.join("Season 4");
    touch(&season, "Show S04E01.mkv");
    move_mtime(&series);
    let outcome = fx.report(&[&season]).await;
    assert_eq!(outcome.created, 2, "{outcome:?}");
    assert_eq!(outcome.updated, 1, "the series: {outcome:?}");
    assert_ne!(fx.rows().await[&series_id].4, after[&series_id].4);
}

/// A flat series — episodes in the series folder, no season folders. A new
/// loose episode of a new season number gets its virtual season, created
/// with it: the row a library scan would plan (same id, parent and
/// presentation key), so the next library scan changes nothing. A second
/// episode of that number is parented to the existing virtual season.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_loose_episode_gets_the_virtual_season_a_library_scan_would() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    touch(&media.join("Flat"), "Flat S01E01.mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let series = media.join("Flat");
    let seasons = |rows: &HashMap<String, Row>| -> Vec<String> {
        let mut ids: Vec<String> = rows
            .values()
            .filter(|r| r.1.ends_with("TV.Season"))
            .map(|r| r.0.clone())
            .collect();
        ids.sort();
        ids
    };
    let before = fx.rows().await;
    assert_eq!(seasons(&before).len(), 1, "virtual season 1");

    let episode = series.join("Flat S02E01.mkv");
    touch(&series, "Flat S02E01.mkv");
    let outcome = fx.report(&[&episode]).await;
    assert_eq!(
        outcome.created, 2,
        "the episode and its virtual season: {outcome:?}"
    );
    let after = fx.rows().await;
    let new_season: Vec<String> = seasons(&after)
        .into_iter()
        .filter(|s| !before.contains_key(s))
        .collect();
    assert_eq!(new_season.len(), 1);
    assert_eq!(
        after[&id(BaseItemKind::Episode, &episode)].2.as_deref(),
        Some(new_season[0].as_str()),
        "the episode's parent is its virtual season, stored"
    );
    assert_eq!(
        new_season[0],
        id(BaseItemKind::Season, &series.join("#virtual-season-2")),
        "the id the library scan derives"
    );

    // The library scan plans the same rows: nothing to create or update.
    let full = fx.scanner.scan_all().await.expect("scan");
    assert_eq!(
        (full.created, full.updated, full.removed),
        (0, 0, 0),
        "{full:?}"
    );

    // A second episode of season 2 uses the existing virtual season.
    let second = series.join("Flat S02E02.mkv");
    touch(&series, "Flat S02E02.mkv");
    let outcome = fx.report(&[&second]).await;
    assert_eq!(outcome.created, 1, "{outcome:?}");
    let rows = fx.rows().await;
    assert_eq!(seasons(&rows), seasons(&after));
    assert_eq!(
        rows[&id(BaseItemKind::Episode, &second)].2.as_deref(),
        Some(new_season[0].as_str())
    );
}

/// The last loose episode of a virtual season is removed: its series (the
/// nearest existing item) is validated, and the emptied virtual season goes
/// with the episode — as a library scan prunes it, and as upstream's series
/// refresh removes an obsolete virtual season (`SeriesMetadataService.
/// RemoveObsoleteSeasons`). The other virtual season stays.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_virtual_seasons_last_episode_prunes_the_season_too() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    touch(&media.join("Flat"), "Flat S01E01.mkv");
    touch(&media.join("Flat"), "Flat S02E01.mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let series = media.join("Flat");
    let season = |n: u32| {
        id(
            BaseItemKind::Season,
            &series.join(format!("#virtual-season-{n}")),
        )
    };
    let before = fx.rows().await;
    assert!(before.contains_key(&season(1)) && before.contains_key(&season(2)));

    let gone = series.join("Flat S02E01.mkv");
    std::fs::remove_file(&gone).expect("rm");
    let outcome = fx.report(&[&gone]).await;
    assert_eq!(
        outcome.removed, 2,
        "the episode and its emptied season: {outcome:?}"
    );
    let after = fx.rows().await;
    assert!(!after.contains_key(&season(2)));
    assert!(!after.contains_key(&id(BaseItemKind::Episode, &gone)));
    assert!(
        after.contains_key(&season(1)),
        "the other virtual season stays"
    );
    // A library scan agrees: nothing left to prune or create.
    let full = fx.scanner.scan_all().await.expect("scan");
    assert_eq!((full.created, full.removed), (0, 0), "{full:?}");
}

/// 1,000 new files in one report are one scan: every file is created, and
/// the walk lists only the folders of the series they belong to — the
/// library root, the series and its season folders — never another series.
#[tokio::test(flavor = "multi_thread")]
async fn a_thousand_new_files_are_one_scan_of_their_series_only() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let series = fx.media.join("Show");
    let season = series.join("Season 9");
    let files: Vec<PathBuf> = (0..1_000)
        .map(|n| {
            let name = format!("Show S09E{n:04}.mkv");
            touch(&season, &name);
            season.join(name)
        })
        .collect();
    let reported: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let outcome = fx.report(&reported).await;
    assert_eq!(
        outcome.created, 1_001,
        "the season and 1,000 episodes: {outcome:?}"
    );
    let mut listed = fx.fs.take();
    listed.sort();
    listed.dedup();
    let root = fx.media.to_string_lossy().into_owned();
    let series_dir = series.to_string_lossy().into_owned();
    assert!(
        listed
            .iter()
            .all(|dir| *dir == root || dir.starts_with(&series_dir)),
        "listed outside the series: {listed:?}"
    );
    assert!(
        listed.len() <= 5,
        "the root, the series and its three season folders: {listed:?}"
    );
}

/// The library monitor's scan runs no library-wide closing pass: the new
/// studio a new movie's NFO names is created with the movie but not
/// refreshed — it waits, selected by its missing `DateLastRefreshed`, for
/// the next library validation's studio pass.
#[tokio::test(flavor = "multi_thread")]
async fn library_wide_work_waits_for_the_next_validation() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media.join("Heat (1995)"), "Heat (1995).mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies)
        .await
        .scanned()
        .await;
    let dir = media.join("Ronin (1998)");
    touch(&dir, "Ronin (1998).mkv");
    std::fs::write(
        dir.join("Ronin (1998).nfo"),
        "<movie><title>Ronin</title><studio>Brand New Studio</studio></movie>",
    )
    .expect("nfo");
    let outcome = fx.report(&[&dir.join("Ronin (1998).mkv")]).await;
    assert_eq!(outcome.created, 1, "{outcome:?}");
    let studios = fx
        .persistence
        .never_refreshed_ids(BaseItemKind::Studio, false)
        .await
        .expect("studios");
    let studio: Option<String> = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%.Studio' AND "Name" = 'Brand New Studio'"#,
    )
    .fetch_optional(fx.db.pool())
    .await
    .expect("studio");
    let studio = studio.expect("the NFO's studio is created with the movie");
    assert!(
        studios.iter().any(|s| guid_to_db(*s) == studio),
        "and left for the next validation's studio pass: {studios:?}"
    );
}

/// A log sink for [`every_processed_item_is_logged_with_why`].
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// `(file name, reason)` of each `path-scoped scan item` line since the
    /// last call.
    fn reasons(&self) -> Vec<(String, String)> {
        let text =
            String::from_utf8(std::mem::take(&mut *self.0.lock().expect("lock"))).expect("utf-8");
        let field = |line: &str, key: &str| {
            line.split(&format!("{key}=\""))
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default()
                .to_owned()
        };
        let mut reasons: Vec<(String, String)> = text
            .lines()
            .filter(|l| l.contains("path-scoped scan item"))
            .map(|l| {
                let path = field(l, "path");
                let name = path.rsplit('/').next().unwrap_or_default().to_owned();
                (name, field(l, "reason"))
            })
            .collect();
        reasons.sort();
        reasons
    }
}

/// Every item a watcher scan processed has one `debug!` line naming why:
/// `created`, the decision's trigger (`mtime`), `unchanged` for an item only
/// validated, `context` for a folder above the refreshed item, and
/// `pruned` for a removed row.
#[tokio::test]
async fn every_processed_item_is_logged_with_why() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let season = fx.media.join("Show").join("Season 1");
    let captured = Captured::default();
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("ferrofin_core=debug")
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    touch(&season, "Show S01E03.mkv");
    move_mtime(&season);
    fx.report(&[&season.join("Show S01E03.mkv")]).await;
    let pair = |name: &str, reason: &str| (name.to_owned(), reason.to_owned());
    assert_eq!(
        captured.reasons(),
        vec![
            pair("Season 1", "mtime"),
            pair("Show", "context"),
            pair("Show S01E01.mkv", "unchanged"),
            pair("Show S01E02.mkv", "unchanged"),
            pair("Show S01E03.mkv", "created"),
        ]
    );

    std::fs::remove_file(season.join("Show S01E02.mkv")).expect("rm");
    fx.report(&[&season.join("Show S01E02.mkv")]).await;
    let reasons = captured.reasons();
    assert!(
        reasons.contains(&pair("Show S01E02.mkv", "pruned")),
        "{reasons:?}"
    );
    assert!(reasons.contains(&pair("Show", "context")), "{reasons:?}");
}

/// Series whose folder names share the removed one's prefix.
const LOOKALIKES: [&str; 3] = ["Show (2019)", "Show-2", "Show.2"];

/// A whole top-level series folder removed: nothing stored is left above it
/// short of the library, so the changed path itself is the root. It and its
/// seasons and episodes are pruned; the series whose folder names merely
/// start alike are untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_series_folder_prunes_it_and_no_lookalike() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    touch(&media.join("Show").join("Season 1"), "Show S01E01.mkv");
    touch(&media.join("Show").join("Season 2"), "Show S02E01.mkv");
    for name in LOOKALIKES {
        touch(
            &media.join(name).join("Season 1"),
            &format!("{name} S01E01.mkv"),
        );
    }
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let before = fx.rows().await;
    let series = media.join("Show");
    std::fs::remove_dir_all(&series).expect("rm");
    let outcome = fx.report(&[&series]).await;
    assert_eq!(
        outcome.removed, 5,
        "the series, its two seasons and two episodes: {outcome:?}"
    );
    let after = fx.rows().await;
    assert!(!after.contains_key(&id(BaseItemKind::Series, &series)));
    for name in LOOKALIKES {
        let other = media.join(name);
        let episode = other.join("Season 1").join(format!("{name} S01E01.mkv"));
        for key in [
            id(BaseItemKind::Series, &other),
            id(BaseItemKind::Season, &other.join("Season 1")),
            id(BaseItemKind::Episode, &episode),
        ] {
            assert_eq!(after.get(&key), before.get(&key), "{name} is untouched");
        }
    }
    assert_eq!(after.len(), before.len() - 5);
}

/// A flat series — loose episodes grouped into virtual seasons — removed
/// whole: its virtual seasons, which have no path, go with it and are
/// counted and announced as removed (not only swept by the `ParentId`
/// cascade).
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_flat_series_prunes_its_virtual_seasons_too() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    touch(&media.join("Flat"), "Flat S01E01.mkv");
    touch(&media.join("Flat"), "Flat S02E01.mkv");
    touch(&media.join("Kept").join("Season 1"), "Kept S01E01.mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let before = fx.rows().await;
    let series = media.join("Flat");
    std::fs::remove_dir_all(&series).expect("rm");
    let outcome = fx.report(&[&series]).await;
    assert_eq!(
        outcome.removed, 5,
        "the series, its two virtual seasons and two episodes: {outcome:?}"
    );
    let after = fx.rows().await;
    assert_eq!(after.len(), before.len() - 5);
    assert!(
        after.values().all(|r| !r.1.ends_with("TV.Season")
            || r.2.as_deref() != Some(id(BaseItemKind::Series, &series).as_str())),
        "no virtual season of the removed series is left"
    );
}

/// A movie file removed while its folder stays (a poster left behind): the
/// folder is the root, the movie is pruned, the other movie is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_movie_file_in_a_remaining_folder_is_pruned() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let heat_dir = media.join("Heat (1995)");
    touch(&heat_dir, "Heat (1995).mkv");
    std::fs::write(heat_dir.join("poster.jpg"), b"\xFF\xD8\xFFposter").expect("poster");
    touch(&media.join("Ronin (1998)"), "Ronin (1998).mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies)
        .await
        .scanned()
        .await;
    let before = fx.rows().await;
    let heat = heat_dir.join("Heat (1995).mkv");
    std::fs::remove_file(&heat).expect("rm");
    let outcome = fx.report(&[&heat]).await;
    assert_eq!(outcome.removed, 1, "{outcome:?}");
    let after = fx.rows().await;
    assert!(!after.contains_key(&id(BaseItemKind::Movie, &heat)));
    let ronin = media.join("Ronin (1998)").join("Ronin (1998).mkv");
    assert_eq!(
        after.get(&id(BaseItemKind::Movie, &ronin)),
        before.get(&id(BaseItemKind::Movie, &ronin))
    );
}

/// A sidecar beside a loose movie in the library root (`Heat (1995).eng.srt`
/// next to `Heat (1995).mkv`): nothing sits between it and the library, so
/// upstream validates the library folder (`FileRefresher.cs:182-208`). Here
/// the stored items of that folder whose file stem the sidecar's name starts
/// with are refreshed instead — the movie re-probes for its new subtitle —
/// without walking the library; the other movie is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_sidecar_beside_a_movie_in_the_library_root_refreshes_that_movie() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media, "Heat (1995).mkv");
    touch(&media, "Heat 2 (2027).mkv");
    touch(&media, "Alien (1979).mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies)
        .await
        .scanned()
        .await;
    let before = fx.rows().await;
    let heat = media.join("Heat (1995).mkv");
    let sidecar = media.join("Heat (1995).eng.srt");
    std::fs::write(&sidecar, b"1\n").expect("srt");
    let outcome = fx.report(&[&sidecar]).await;
    assert_eq!(
        (outcome.created, outcome.updated, outcome.removed),
        (0, 1, 0),
        "the movie the sidecar belongs to: {outcome:?}"
    );
    let external: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "ItemId" = ?1 AND "IsExternal" = 1"#,
    )
    .bind(id(BaseItemKind::Movie, &heat))
    .fetch_one(fx.db.pool())
    .await
    .expect("streams");
    assert_eq!(external, 1, "the subtitle is the movie's stream now");
    let after = fx.rows().await;
    for other in ["Heat 2 (2027).mkv", "Alien (1979).mkv"] {
        let key = id(BaseItemKind::Movie, &media.join(other));
        assert_eq!(after.get(&key), before.get(&key), "{other} is untouched");
    }
    // The library root was listed once, to resolve the movie; no other
    // folder was.
    let listed = fx.fs.take();
    assert!(
        listed.iter().all(|d| Path::new(d) == media.as_path()),
        "{listed:?}"
    );

    // Its removal is a change too.
    std::fs::remove_file(&sidecar).expect("rm");
    let outcome = fx.report(&[&sidecar]).await;
    assert_eq!(outcome.updated, 1, "{outcome:?}");
}

/// A library location that is still there but lists empty — an NFS mount
/// that dropped, leaving its empty mountpoint — is inaccessible, not
/// emptied (`Folder.IsLibraryFolderAccessible`, `Folder.cs:400-415`): a
/// library scan prunes nothing under it, and neither does a webhook for a
/// series in it.
///
/// A flat series' virtual seasons, which have no path of their own, are
/// held back with their series: removing one would take the episodes
/// parented to it with it (the `ParentId` cascade).
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_library_location_prunes_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    touch(&media.join("Show").join("Season 1"), "Show S01E01.mkv");
    touch(&media.join("Flat"), "Flat S01E01.mkv");
    touch(&media.join("Flat"), "Flat S02E01.mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let before = fx.rows().await;
    assert_eq!(
        before.len(),
        8,
        "2 series, 3 seasons (2 virtual), 3 episodes"
    );
    // The mount drops: the location is an empty directory.
    let parked = tmp.path().join("parked");
    std::fs::rename(&fx.media, &parked).expect("park");
    std::fs::create_dir(&fx.media).expect("empty mountpoint");

    let outcome = fx.scanner.scan_all().await.expect("scan");
    assert_eq!(outcome.removed, 0, "{outcome:?}");
    assert_eq!(fx.rows().await, before, "nothing pruned or written");

    for gone in [fx.media.join("Show"), fx.media.join("Flat")] {
        let outcome = fx.report(&[&gone]).await;
        assert_eq!(outcome.removed, 0, "{outcome:?}");
        assert_eq!(fx.rows().await, before, "nothing pruned or written");
    }

    // Back: an unchanged library again.
    std::fs::remove_dir(&fx.media).expect("rmdir");
    std::fs::rename(&parked, &fx.media).expect("unpark");
    let outcome = fx.scanner.scan_all().await.expect("scan");
    assert_eq!((outcome.created, outcome.removed), (0, 0), "{outcome:?}");
}

/// A user who has played `item`, the way `UserData` stores it.
async fn play(db: &Database, item: &str) {
    let user = "0000000A-0000-0000-0000-00000000000A";
    sqlx::query(
        r#"INSERT OR IGNORE INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'u', 'U')"#,
    )
    .bind(user)
    .execute(db.writer())
    .await
    .expect("user");
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?1, 0, 1, 0, 1)"#,
    )
    .bind(item)
    .bind(user)
    .execute(db.writer())
    .await
    .expect("user data");
}

async fn user_data(db: &Database) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "UserData""#)
        .fetch_one(db.pool())
        .await
        .expect("count")
}

#[rstest::rstest]
#[case::one_scan(0)]
#[case::destination_first(1)]
#[case::source_first(2)]
#[case::restore_original_path(3)]
#[tokio::test(flavor = "multi_thread")]
async fn retention_survives_file_moves_and_restores(#[case] order: u8) {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("movies");
    let original_dir = media.join("Heat (1995)");
    let destination_dir = if order == 3 {
        original_dir.clone()
    } else {
        media.join("Moved Heat (1995)")
    };
    let seed_movie = |dir: &Path| {
        touch(dir, "Heat (1995).mkv");
        std::fs::write(dir.join("movie.nfo"),
            b"<movie><title>Heat</title><uniqueid type=\"tmdb\" default=\"true\">949</uniqueid></movie>").unwrap();
    };
    seed_movie(&original_dir);
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies)
        .await
        .scanned()
        .await;
    let original = original_dir.join("Heat (1995).mkv");
    let destination = destination_dir.join("Heat (1995).mkv");
    let old_id = id(BaseItemKind::Movie, &original);
    play(&fx.db, &old_id).await;
    // Simulate the real provider-keyed snapshot; the original-path case
    // deliberately has only the GUID key, without a provider fallback.
    sqlx::query(
        r#"UPDATE "UserData" SET "CustomDataKey" = ?1, "IsFavorite" = 1,
           "PlaybackPositionTicks" = 1234567 WHERE "ItemId" = ?2"#,
    )
    .bind(if order == 3 {
        old_id.to_lowercase()
    } else {
        "949".to_owned()
    })
    .bind(&old_id)
    .execute(fx.db.writer())
    .await
    .unwrap();
    if order == 1 {
        seed_movie(&destination_dir);
        fx.report(&[&destination]).await;
    }
    std::fs::remove_file(&original).unwrap();
    if order >= 2 {
        fx.report(&[&original]).await;
        assert!(!fx.rows().await.contains_key(&old_id));
        assert_eq!(
            user_data(&fx.db).await,
            1,
            "history detached before destination exists"
        );
    }
    if order != 1 {
        seed_movie(&destination_dir);
    }
    if order == 0 {
        fx.scanner.scan_all().await.unwrap();
    } else if order == 1 {
        // Destination was indexed by an earlier scan; this event only
        // covers the source folder, so recovery must search outside it.
        fx.report(&[&original]).await;
    } else {
        fx.report(&[&destination]).await;
    }
    let new_id = id(BaseItemKind::Movie, &destination);
    let rows: Vec<(bool, bool, i64, Option<String>)> = sqlx::query_as(
        r#"SELECT "Played", "IsFavorite", "PlaybackPositionTicks", "RetentionDate"
           FROM "UserData" WHERE "ItemId" = ?1"#,
    )
    .bind(&new_id)
    .fetch_all(fx.db.pool())
    .await
    .unwrap();
    assert!(!rows.is_empty(), "history recovered at {new_id}");
    assert!(rows.iter().all(|row| *row == (true, true, 1234567, None)));
    let before = user_data(&fx.db).await;
    fx.scanner.scan_all().await.unwrap();
    assert_eq!(
        user_data(&fx.db).await,
        before,
        "unchanged rescan is idempotent"
    );
}

/// Restores a folder's permissions when dropped, so the temp dir can go.
struct Unlistable(PathBuf);

impl Unlistable {
    fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
        Self(dir.to_path_buf())
    }
}

impl Drop for Unlistable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// A flat series whose second season's loose episodes sit in a non-season
/// subfolder (`Flat/Batch2/`) that fails to list: the episodes are unknown,
/// not gone, so their virtual season — which no listing planned — must not
/// be removed, since the `ParentId` cascade would take the episodes, and
/// their played state, with it. Upstream removes a virtual season only when
/// it has no episode left (`SeriesMetadataService.cs:172-207`). Neither a
/// library scan nor a changed-path scan loses a row.
#[tokio::test(flavor = "multi_thread")]
async fn an_unlistable_subfolder_never_takes_its_episodes_with_their_virtual_season() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    let series = media.join("Flat");
    touch(&series, "Flat S01E01.mkv");
    touch(&series.join("Batch2"), "Flat S02E01.mkv");
    touch(&series.join("Batch2"), "Flat S02E02.mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let before = fx.rows().await;
    assert_eq!(before.len(), 6, "the series, 2 virtual seasons, 3 episodes");
    let played = id(
        BaseItemKind::Episode,
        &series.join("Batch2").join("Flat S02E01.mkv"),
    );
    play(&fx.db, &played).await;
    play(
        &fx.db,
        &id(BaseItemKind::Episode, &series.join("Flat S01E01.mkv")),
    )
    .await;

    let _guard = Unlistable::new(&series.join("Batch2"));
    let outcome = fx.scanner.scan_all().await.expect("scan");
    assert_eq!(outcome.removed, 0, "library scan: {outcome:?}");
    assert_eq!(fx.rows().await.len(), 6, "library scan: no row lost");
    assert_eq!(
        user_data(&fx.db).await,
        2,
        "library scan: played state kept"
    );

    std::fs::write(series.join("new.nfo"), b"<tvshow/>").expect("nfo");
    let outcome = fx.report(&[&series.join("new.nfo")]).await;
    assert_eq!(outcome.removed, 0, "changed-path scan: {outcome:?}");
    assert_eq!(fx.rows().await.len(), 6, "changed-path scan: no row lost");
    assert_eq!(
        user_data(&fx.db).await,
        2,
        "changed-path scan: played state kept"
    );
}

/// The guard holds up the whole chain: an episode that stays keeps its
/// virtual season and that season's series, both of which would otherwise
/// go (the series folder is gone). The episode here is kept because it is
/// no row the scan weighs (its path lies outside the scanned folder).
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_episode_keeps_its_virtual_season_and_series() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    let series = media.join("Flat");
    touch(&series, "Flat S01E01.mkv");
    touch(&media.join("Elsewhere"), "Flat S01E02.mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows)
        .await
        .scanned()
        .await;
    let season = id(BaseItemKind::Season, &series.join("#virtual-season-1"));
    let elsewhere = id(
        BaseItemKind::Episode,
        &media.join("Elsewhere").join("Flat S01E02.mkv"),
    );
    // The other folder's episode is filed under the flat series' season.
    sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?1 WHERE "Id" = ?2"#)
        .bind(&season)
        .bind(&elsewhere)
        .execute(fx.db.writer())
        .await
        .expect("reparent");
    let before = fx.rows().await;

    std::fs::remove_dir_all(&series).expect("rm");
    let outcome = fx.report(&[&series]).await;
    assert_eq!(
        outcome.removed, 1,
        "only the series' own episode: {outcome:?}"
    );
    let after = fx.rows().await;
    for kept in [id(BaseItemKind::Series, &series), season, elsewhere] {
        assert!(after.contains_key(&kept), "{kept} kept");
    }
    assert_eq!(after.len(), before.len() - 1);
}

/// Sidecar matching ignores case, as upstream's external-file matching does
/// (`MediaInfoResolver.cs:259`): `heat (1995).eng.srt` belongs to
/// `Heat (1995).mkv`.
#[tokio::test(flavor = "multi_thread")]
async fn a_sidecar_finds_its_movie_whatever_the_case() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media, "Heat (1995).mkv");
    touch(&media, "Alien (1979).mkv");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies)
        .await
        .scanned()
        .await;
    let sidecar = media.join("heat (1995).eng.srt");
    std::fs::write(&sidecar, b"1\n").expect("srt");
    let outcome = fx.report(&[&sidecar]).await;
    assert_eq!(
        (outcome.created, outcome.updated, outcome.removed),
        (0, 1, 0),
        "{outcome:?}"
    );
}

/// Inserts a bare `BaseItems` row — an adopted database's shapes the
/// planner never writes (a playlist, a collection, an extra filed under
/// another item, a missing episode).
#[allow(clippy::too_many_arguments)]
async fn insert_row(
    db: &Database,
    id: &str,
    type_: &str,
    parent: Option<&str>,
    owner: Option<&str>,
    top_parent: Option<&str>,
    path: Option<&str>,
    is_virtual: bool,
) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "ParentId", "OwnerId", "ExtraType",
            "TopParentId", "Path", "Name")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, ?3, ?4, ?5, ?6, ?7, ?8, 'x')"#,
    )
    .bind(id)
    .bind(type_)
    .bind(i64::from(is_virtual))
    .bind(parent)
    .bind(owner)
    .bind(owner.map(|_| 1_i64))
    .bind(top_parent)
    .bind(path)
    .execute(db.writer())
    .await
    .expect("insert row");
}

async fn link(db: &Database, container: &str, child: &str) {
    sqlx::query(
        r#"INSERT INTO "LinkedChildren" ("ParentId", "SortOrder", "ChildId", "ChildType")
           VALUES (?1, 0, ?2, 0)"#,
    )
    .bind(container)
    .bind(child)
    .execute(db.writer())
    .await
    .expect("link");
}

/// A stored row's `TopParentId`: its library.
async fn top_parent(db: &Database, id: &str) -> Option<String> {
    sqlx::query_scalar(r#"SELECT "TopParentId" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("top parent")
}

async fn count(db: &Database, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(db.pool())
        .await
        .expect("count")
}

/// A removed season folder whose episode sits in a playlist and a
/// collection and owns an extra filed under the series (an adopted
/// Jellyfin shape): the season, its episodes and the extra all go — the
/// delete clears the membership rows and takes the closure in one
/// transaction (upstream `ItemPersistenceService.DeleteItem`) — and the
/// count is exact. Before, the season went first and the cascade tripped
/// the foreign keys still naming the episode, so nothing was reported
/// removed and the season stayed a ghost.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_season_in_a_playlist_and_a_collection_is_pruned_whole() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let series = fx.media.join("Show");
    let season = series.join("Season 2");
    let episode = id(BaseItemKind::Episode, &season.join("Show S02E01.mkv"));
    let series_id = id(BaseItemKind::Series, &series);
    let library = top_parent(&fx.db, &series_id).await;
    let (playlist, boxset, extra) = (
        "00000000-0000-0000-0000-0000000000A1",
        "00000000-0000-0000-0000-0000000000A2",
        "00000000-0000-0000-0000-0000000000A3",
    );
    insert_row(
        &fx.db,
        playlist,
        "MediaBrowser.Controller.Playlists.Playlist",
        None,
        None,
        None,
        None,
        false,
    )
    .await;
    insert_row(
        &fx.db,
        boxset,
        "MediaBrowser.Controller.Entities.Movies.BoxSet",
        None,
        None,
        None,
        None,
        false,
    )
    .await;
    let extra_path = season.join("extras").join("Behind.mkv");
    insert_row(
        &fx.db,
        extra,
        "MediaBrowser.Controller.Entities.Video",
        Some(&series_id),
        Some(&episode),
        library.as_deref(),
        Some(&extra_path.to_string_lossy()),
        false,
    )
    .await;
    link(&fx.db, playlist, &episode).await;
    link(&fx.db, boxset, &episode).await;
    let before = count(&fx.db, r#"SELECT COUNT(*) FROM "BaseItems""#).await;

    std::fs::remove_dir_all(&season).expect("rm");
    let outcome = fx.report(&[&season]).await;
    assert_eq!(
        outcome.removed, 4,
        "the season, its two episodes and the extra: {outcome:?}"
    );
    assert_eq!(
        count(&fx.db, r#"SELECT COUNT(*) FROM "BaseItems""#).await,
        before - 4
    );
    assert_eq!(
        count(&fx.db, r#"SELECT COUNT(*) FROM "LinkedChildren""#).await,
        0,
        "the membership rows went with the episode"
    );
    assert_eq!(
        count(
            &fx.db,
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" IN
               ('00000000-0000-0000-0000-0000000000A1', '00000000-0000-0000-0000-0000000000A2')"#
        )
        .await,
        2,
        "the playlist and the collection stay"
    );
    // The next library scan finds nothing left to prune.
    let full = fx.scanner.scan_all().await.expect("scan");
    assert_eq!(full.removed, 0, "{full:?}");
}

/// An adopted Jellyfin missing episode — a virtual `Episode` with no path —
/// is no file this scan could find gone: upstream's validation removes only
/// file children (`Folder.cs:569`) and keeps numbered missing episodes
/// (`SeriesMetadataService.cs:237`). A watcher event in its season and a
/// library scan both leave it.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_episode_without_a_path_survives_every_scan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = tv(tmp.path()).await;
    let season = fx.media.join("Show").join("Season 1");
    let season_id = id(BaseItemKind::Season, &season);
    let top = top_parent(&fx.db, &season_id).await;
    let missing = "00000000-0000-0000-0000-0000000000B1";
    insert_row(
        &fx.db,
        missing,
        "MediaBrowser.Controller.Entities.TV.Episode",
        Some(&season_id),
        None,
        top.as_deref(),
        None,
        true,
    )
    .await;
    let present = || async {
        count(
            &fx.db,
            r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Id" = '00000000-0000-0000-0000-0000000000B1'"#,
        )
        .await
    };

    touch(&season, "Show S01E03.mkv");
    move_mtime(&season);
    let outcome = fx.report(&[&season.join("Show S01E03.mkv")]).await;
    assert_eq!((outcome.created, outcome.removed), (1, 0), "{outcome:?}");
    assert_eq!(present().await, 1, "a watcher event keeps it");
    let full = fx.scanner.scan_all().await.expect("scan");
    assert_eq!(full.removed, 0, "{full:?}");
    assert_eq!(present().await, 1, "a library scan keeps it");
}
