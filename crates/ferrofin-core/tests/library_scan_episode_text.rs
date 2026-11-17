//! What the episode providers' re-scan gate reads, end to end through a scan.
//!
//! `Planned.entity` is rebuilt from the filesystem on every scan, so its `Name`
//! is always the resolver's and its `Overview` always `None`. The gate that
//! stops every episode re-fetching its metadata on every scan therefore has to
//! consult the **stored** row. The scan reads stored rows one window of the
//! plan at a time (`ItemRepository::retrieve_items`) and hands each episode's
//! stored title and synopsis to the gate.
//!
//! The property with teeth: a rescan of an unchanged, already-titled episode
//! makes **no** episode request to TMDB. A regression anywhere on that path —
//! the window read, the id lookup, the type filter, the text lift — still
//! produces correct rows (TMDB just answers again), so only the request count
//! catches it.

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner, ScanOutcome,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// `/search/tv`: gives the series a TMDB id to hang its seasons off.
const SERIES_SEARCH_JSON: &str = r#"{"results": [{"id": 1399, "name": "GoT"}]}"#;

/// `/tv/{id}`: series details.
const SERIES_DETAILS_JSON: &str = r#"{"overview": "A series.", "genres": []}"#;

/// `/tv/{id}/season/1`: episode text without artwork. The independent
/// image-list routes below return no candidates.
const SEASON_JSON: &str = r#"{
    "name": "Season 1",
    "overview": "The first season.",
    "episodes": [
        {"id": 63056, "episode_number": 1, "name": "Winter Is Coming",
         "overview": "Ned is summoned south.", "air_date": "2011-04-17",
         "vote_average": 8.5},
        {"id": 63057, "episode_number": 2, "name": "The Kingsroad",
         "overview": "The party rides north.", "air_date": "2011-04-24"}
    ]
}"#;

/// `/tv/{id}/season/1/episode/{n}?append_to_response=credits,…`: requested per episode, and only
/// by the episode provider, so it counts exactly the episodes the gate let
/// through.
const CREDITS_JSON: &str =
    r#"{"credits":{"cast": [{"id": 1, "name": "Sean Bean", "character": "Ned"}]}}"#;

/// `/tv/{id}` for a series that has everything the backfill gates want
/// (an overview and a trailer), so no heuristic asks for it again.
const ENRICHED_SERIES_DETAILS_JSON: &str = r#"{"overview": "A series.", "genres": [],
    "videos": {"results": [{"site": "YouTube", "type": "Trailer", "key": "k", "name": "T"}]}}"#;

/// A TMDB stand-in counting the per-episode credits requests.
fn spawn_tmdb() -> (String, Arc<AtomicUsize>) {
    let (base, credits, _) = spawn_tmdb_with(SERIES_DETAILS_JSON);
    (base, credits)
}

/// [`spawn_tmdb`] with the given series details, also counting the series
/// searches.
fn spawn_tmdb_with(series_details: &'static str) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let credits = Arc::new(AtomicUsize::new(0));
    let searches = Arc::new(AtomicUsize::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let counter = Arc::clone(&credits);
    let search_counter = Arc::clone(&searches);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            // Image-list requests share the episode prefix but are not metadata
            // requests. L14 acquires artwork independently of season text.
            let (status, payload) = if req.contains("/images") {
                ("200 OK", r#"{"posters":[],"backdrops":[],"stills":[]}"#)
            } else if req.contains("/episode/") {
                counter.fetch_add(1, Ordering::SeqCst);
                ("200 OK", CREDITS_JSON)
            } else if req.contains("/season/") {
                ("200 OK", SEASON_JSON)
            } else if req.contains("/search/tv") {
                search_counter.fetch_add(1, Ordering::SeqCst);
                ("200 OK", SERIES_SEARCH_JSON)
            } else if req.contains("/tv/") {
                ("200 OK", series_details)
            } else {
                ("404 Not Found", "{}")
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), credits, searches)
}

#[tokio::test]
async fn a_rescan_hands_the_stored_episode_text_to_the_gate() {
    let (base, credits) = spawn_tmdb();
    let tmp = tempfile::tempdir().expect("tmp");
    let tv = tmp.path().join("tv");
    let season = tv.join("GoT/Season 01");
    std::fs::create_dir_all(&season).expect("mkdir");
    for ep in ["GoT S01E01.mkv", "GoT S01E02.mkv"] {
        std::fs::write(season.join(ep), b"").expect("write");
    }

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "TV",
        Some(CollectionTypeOptions::tvshows),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: tv.to_string_lossy().into_owned(),
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
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .with_items(Arc::clone(&items));

    // First scan: every episode is new, so TMDB titles each one.
    let first = scanner.scan_all().await.expect("first scan");
    assert_eq!(
        first.created, 4,
        "series, season and two episodes: {first:?}"
    );
    assert_eq!(
        credits.load(Ordering::SeqCst),
        2,
        "one credits request per new episode"
    );
    let episode_id = derive_item_id(
        BaseItemKind::Episode,
        &season.join("GoT S01E01.mkv").to_string_lossy(),
    )
    .expect("episode id");
    let stored = items
        .retrieve_item(episode_id)
        .await
        .expect("read")
        .expect("episode row");
    assert_eq!(stored.name.as_deref(), Some("Winter Is Coming"));
    assert_eq!(stored.overview.as_deref(), Some("Ned is summoned south."));
    let first = stored;
    assert!(first.premiere_date.is_some());
    assert_eq!(first.production_year, Some(2011));
    assert_eq!(first.community_rating, Some(8.5));

    // Rescan: the stored title and synopsis reach the gate, so no episode is
    // fetched again, and the save — the stored row with this pass merged on —
    // keeps everything TMDB supplied: the text, and (the regression the old
    // hand-carried gate had) the air date, year and rating.
    //
    // Since change detection, the unchanged season and episodes run no
    // provider at all. The series has no remote trailers, so the kept
    // `wants_trailers` backfill (owner decision D2) asks TMDB again — but
    // TMDB answers exactly what is stored, so nothing is written for it
    // either.
    let rescan = scanner.scan_all().await.expect("rescan");
    assert_eq!(
        rescan,
        ScanOutcome {
            unchanged: 4,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(
        credits.load(Ordering::SeqCst),
        2,
        "an already-titled episode must not be re-requested on a rescan"
    );
    let stored = items
        .retrieve_item(episode_id)
        .await
        .expect("read")
        .expect("episode row");
    assert_eq!(stored.name.as_deref(), Some("Winter Is Coming"));
    assert_eq!(stored.overview.as_deref(), Some("Ned is summoned south."));
    assert_eq!(stored.premiere_date, first.premiere_date);
    assert_eq!(stored.production_year, Some(2011));
    assert_eq!(stored.community_rating, Some(8.5));
}

/// A changed episode under an unchanged series: the episode runs every
/// provider, the series none. The episode still resolves, through the TMDB id
/// the series' earlier scan recorded — nothing matched the series this scan.
#[tokio::test]
async fn a_changed_episode_under_an_unchanged_series_resolves_through_the_recorded_id() {
    let (base, credits, searches) = spawn_tmdb_with(ENRICHED_SERIES_DETAILS_JSON);
    let tmp = tempfile::tempdir().expect("tmp");
    let tv = tmp.path().join("tv");
    let season = tv.join("GoT").join("Season 1");
    std::fs::create_dir_all(&season).expect("mkdir");
    for ep in ["GoT S01E01.mkv", "GoT S01E02.mkv"] {
        std::fs::write(season.join(ep), b"").expect("write");
    }
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "TV",
        Some(CollectionTypeOptions::tvshows),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: tv.to_string_lossy().into_owned(),
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
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .with_items(Arc::clone(&items));

    assert_eq!(scanner.scan_all().await.expect("first").created, 4);
    assert_eq!(credits.load(Ordering::SeqCst), 2);
    let searches_before = searches.load(Ordering::SeqCst);
    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            unchanged: 4,
            ..ScanOutcome::default()
        },
        "an enriched series and its unchanged episodes are left alone"
    );

    let file = season.join("GoT S01E01.mkv");
    std::fs::File::options()
        .write(true)
        .open(&file)
        .expect("open")
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3_600))
        .expect("touch");
    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            updated: 1,
            unchanged: 3,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(
        credits.load(Ordering::SeqCst),
        3,
        "the changed episode asked TMDB for its credits again"
    );
    assert_eq!(
        searches.load(Ordering::SeqCst),
        searches_before,
        "without searching for the unchanged series"
    );
    let id = derive_item_id(BaseItemKind::Episode, &file.to_string_lossy()).expect("id");
    let row = items.retrieve_item(id).await.expect("read").expect("row");
    assert_eq!(row.name.as_deref(), Some("Winter Is Coming"));
}

/// A new multi-episode file (`S01E01-E02`) is titled from every episode in
/// its range: `TmdbEpisodeProvider` fetches each episode up to
/// `IndexNumberEnd` and joins their names and overviews with ` / `
/// (`TmdbEpisodeProvider.cs:106-158`), and the lookup has the range end from
/// the file name before the providers run (`FillMissingEpisodeNumbersFromPath`
/// in `BeforeMetadataRefresh`). Only the first episode's own request is made.
/// A later refresh looks up the stored range end, or the file name's under
/// "Replace all metadata".
#[tokio::test]
async fn a_multi_episode_file_is_titled_from_every_episode_in_its_range() {
    let (base, credits) = spawn_tmdb();
    let tmp = tempfile::tempdir().expect("tmp");
    let (db, items, scanner, file) = multi_episode_library(tmp.path(), &base, None, None).await;
    let id = derive_item_id(BaseItemKind::Episode, &file.to_string_lossy()).expect("id");
    let joined = |row: &ferrofin_db::entities::base_items::BaseItemEntity| {
        (
            row.name.clone(),
            row.overview.clone(),
            row.data
                .as_deref()
                .is_some_and(|d| d.contains(r#""IndexNumberEnd":2"#)),
        )
    };
    let expected = (
        Some("Winter Is Coming / The Kingsroad".to_owned()),
        Some("Ned is summoned south. / The party rides north.".to_owned()),
        true,
    );

    assert_eq!(scanner.scan_all().await.expect("first").created, 3);
    assert_eq!(credits.load(Ordering::SeqCst), 1, "the first episode's own");
    let row = items.retrieve_item(id).await.expect("read").expect("row");
    assert_eq!(joined(&row), expected);
    assert_eq!(
        row.community_rating,
        Some(8.5),
        "the first episode's rating"
    );

    // On a refresh the stored range end is the lookup's, even where the file
    // name disagrees (`FillMissingEpisodeNumbersFromPath` fills only a
    // missing one): a stored end of 1 looks up episode 1 alone, whose
    // overview "Search for missing metadata" fills in. Under "Replace all metadata"
    // the file name's end replaces it (`forceRefresh`).
    sqlx::query(
        r#"UPDATE "BaseItems" SET "Data" = json_set("Data", '$.IndexNumberEnd', 1),
           "Name" = NULL, "Overview" = NULL WHERE "Path" = ?1"#,
    )
    .bind(file.to_string_lossy().into_owned())
    .execute(db.pool())
    .await
    .expect("narrow the stored range");
    refresh(&scanner, &file, false).await;
    let row = items.retrieve_item(id).await.expect("read").expect("row");
    assert_eq!(
        row.overview.as_deref(),
        Some("Ned is summoned south."),
        "the stored end"
    );
    refresh(&scanner, &file, true).await;
    let row = items.retrieve_item(id).await.expect("read").expect("row");
    assert_eq!(joined(&row), expected, "the file name's end");
}

/// A full metadata refresh of the item at `path`, replacing all metadata or
/// not.
async fn refresh(scanner: &LibraryScanner, path: &std::path::Path, replace_all: bool) {
    use ferrofin_core::{ScanCancel, ScanPasses, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: replace_all,
        ..MetadataRefreshOptions::default()
    };
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    scanner
        .scan_target(
            &ScanTarget::Items(vec![path.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &none, &ScanCancel::new()).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("refresh");
}

/// A TV library holding one multi-episode file, `GoT S01E01-E02.mkv` (with
/// `nfo` beside it when given), and a scanner over it with TMDb at `base` and,
/// when given, `probe` as its prober. Returns the database, the item store,
/// the scanner and the file's path.
async fn multi_episode_library(
    root: &std::path::Path,
    base: &str,
    nfo: Option<&str>,
    probe: Option<Arc<dyn ferrofin_traits::media_encoding::MediaEncoder>>,
) -> (
    Database,
    Arc<dyn ItemRepository>,
    LibraryScanner,
    std::path::PathBuf,
) {
    let tv = root.join("tv");
    let season = tv.join("GoT").join("Season 1");
    std::fs::create_dir_all(&season).expect("mkdir");
    let file = season.join("GoT S01E01-E02.mkv");
    std::fs::write(&file, b"").expect("write");
    if let Some(nfo) = nfo {
        std::fs::write(season.join("GoT S01E01-E02.nfo"), nfo).expect("write nfo");
    }
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("default"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "TV",
        Some(CollectionTypeOptions::tvshows),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: tv.to_string_lossy().into_owned(),
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
    let mut scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(base)),
            root.join("metadata"),
        )
        .with_items(Arc::clone(&items));
    if let Some(probe) = probe {
        scanner = scanner.with_probe(
            probe,
            Arc::new(ferrofin_core::FerrofinMediaStreamRepository::new(
                db.clone(),
            )),
            Arc::new(ferrofin_core::FerrofinChapterRepository::new(db.clone())),
        );
    }
    (db, items, scanner, file)
}

/// Probes every file as a short H.264 video, counting the probes.
struct Probe(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl ferrofin_traits::media_encoding::MediaEncoder for Probe {
    fn encoder_path(&self) -> String {
        "ffmpeg".to_owned()
    }
    fn probe_path(&self) -> String {
        "ffprobe".to_owned()
    }
    async fn set_ffmpeg_path(&self) -> Result<bool, ferrofin_traits::error::ServiceError> {
        Ok(true)
    }
    async fn get_media_info(
        &self,
        _: &ferrofin_traits::media_encoding::MediaInfoRequest,
    ) -> Result<ferrofin_model::dto::MediaSourceInfo, ferrofin_traits::error::ServiceError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ferrofin_model::dto::MediaSourceInfo {
            run_time_ticks: Some(30_000_000),
            media_streams: vec![ferrofin_model::entities_media::MediaStream {
                index: 0,
                stream_type: ferrofin_model::entities::MediaStreamType::Video,
                codec: Some("h264".to_owned()),
                width: Some(640),
                height: Some(480),
                ..Default::default()
            }],
            ..Default::default()
        })
    }
    async fn extract_audio_image(
        &self,
        _: &str,
        _: Option<i32>,
    ) -> Result<String, ferrofin_traits::error::ServiceError> {
        Err(ferrofin_traits::error::ServiceError::backend("no cover"))
    }
    async fn extract_video_image(
        &self,
        _: &str,
        _: &str,
        _: &ferrofin_model::dto::MediaSourceInfo,
        _: &ferrofin_model::entities_media::MediaStream,
        _: Option<ferrofin_model::entities::Video3DFormat>,
        _: Option<i64>,
    ) -> Result<String, ferrofin_traits::error::ServiceError> {
        Err(ferrofin_traits::error::ServiceError::backend("no frame"))
    }
    fn get_input_argument(
        &self,
        input_file: &str,
        _: &ferrofin_model::dto::MediaSourceInfo,
    ) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _: i64) -> String {
        String::new()
    }
    async fn convert_image(
        &self,
        _: &str,
        _: &str,
    ) -> Result<(), ferrofin_traits::error::ServiceError> {
        Ok(())
    }
}

/// A probed multi-episode file's lookup has the range end `BeforeMetadataRefresh`
/// left on the item, never its NFO's (`MergeNewData` hands the providers a
/// local reader's ids only): new, the file name's (`S01E01-E02`, not the NFO's
/// end of 1); stored, the stored row's (1 here, which the file name only fills
/// when missing), so "Search for missing metadata" fills episode 1's overview
/// alone.
#[tokio::test]
async fn a_probed_multi_episode_file_looks_up_the_items_range_not_its_nfos() {
    let (base, _) = spawn_tmdb();
    let tmp = tempfile::tempdir().expect("tmp");
    let nfo = "<episodedetails><season>1</season><episode>1</episode>\
               <episodenumberend>1</episodenumberend></episodedetails>";
    let probes = Arc::new(AtomicUsize::new(0));
    let (db, items, scanner, file) = multi_episode_library(
        tmp.path(),
        &base,
        Some(nfo),
        Some(Arc::new(Probe(Arc::clone(&probes)))),
    )
    .await;
    let id = derive_item_id(BaseItemKind::Episode, &file.to_string_lossy()).expect("id");

    scanner.scan_all().await.expect("first");
    let row = items.retrieve_item(id).await.expect("read").expect("row");
    assert_eq!(probes.load(Ordering::SeqCst), 1, "the new file was probed");
    assert_eq!(
        row.name.as_deref(),
        Some("Winter Is Coming / The Kingsroad"),
        "the file name's range, not the NFO's"
    );

    sqlx::query(
        r#"UPDATE "BaseItems" SET "Data" = json_set("Data", '$.IndexNumberEnd', 1),
           "Overview" = NULL WHERE "Path" = ?1"#,
    )
    .bind(file.to_string_lossy().into_owned())
    .execute(db.pool())
    .await
    .expect("narrow the stored range");
    refresh(&scanner, &file, false).await;
    let row = items.retrieve_item(id).await.expect("read").expect("row");
    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "the full refresh probed it"
    );
    assert_eq!(
        row.overview.as_deref(),
        Some("Ned is summoned south."),
        "the stored range"
    );
}
