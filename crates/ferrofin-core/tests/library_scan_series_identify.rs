//! Identify → Apply on a series (`POST /Items/RemoteSearch/Apply`), through
//! the scan of its folder: the refresh carries the chosen result to its
//! seasons and episodes too (`ProviderManager.RefreshItem` →
//! `ValidateChildren` with the same options), and `ApplySearchResult` hands
//! each of their lookups the result's ids as its series' ids
//! (`SeriesProviderIds = GetValidProviderIds(result.ProviderIds)`,
//! `MetadataService.cs:272-293`). So their providers resolve the series by
//! the chosen ids alone, not by an id the series' own refresh found.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner, ScanCancel, ScanPasses, ScanRun,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::providers::RemoteSearchResult;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::persistence::ItemRepository;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// `/tv/1399/season/1`: one episode.
const SEASON_JSON: &str = r#"{"id": 3624, "name": "Season 1", "overview": "The first season.",
    "episodes": [{"id": 63056, "episode_number": 1, "name": "Winter Is Coming",
    "overview": "Ned is summoned south.", "air_date": "2011-04-17"}]}"#;

/// A TheMovieDb stand-in that knows one series, 1399, also by its TheTVDB
/// id (`/find`), logging every request line.
fn spawn_tmdb() -> (String, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::clone(&log);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default().to_owned();
            let (status, payload) = if line.contains("/images") {
                (
                    "200 OK",
                    r#"{"posters":[],"backdrops":[],"stills":[],"logos":[]}"#,
                )
            } else if line.contains("/find/") {
                (
                    "200 OK",
                    r#"{"movie_results":[],"tv_results":[{"id":1399}]}"#,
                )
            } else if line.contains("/tv/1399/season/1/episode/") {
                ("200 OK", r#"{"credits":{"cast":[]}}"#)
            } else if line.contains("/tv/1399/season/1") {
                ("200 OK", SEASON_JSON)
            } else if line.contains("/search/tv") {
                ("200 OK", r#"{"results":[{"id":1399,"name":"GoT"}]}"#)
            } else if line.contains("/tv/1399") {
                (
                    "200 OK",
                    r#"{"id":1399,"name":"GoT","overview":"A series."}"#,
                )
            } else {
                ("404 Not Found", "{}")
            };
            requests.lock().expect("log").push(line);
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), log)
}

/// Identify a scanned series with a result carrying only `ids`. TheMovieDb
/// still matches the series itself (a TheTVDB id resolves through `/find`),
/// but its season and episode providers ask TheMovieDb only when the chosen
/// result names the series' TheMovieDb id: a TheTVDB result leaves them
/// without one for this refresh, as upstream's `TmdbSeasonProvider` and
/// `TmdbEpisodeProvider` find no `Tmdb` key in `SeriesProviderIds`. Their
/// image providers read the series item itself, so its artwork requests
/// are no lookup and are not counted.
#[rstest::rstest]
#[case::a_tvdb_result(&[("Tvdb", "121361")], "TheTVDB", false)]
#[case::a_tmdb_result(&[("Tmdb", "1399")], "TheMovieDb", true)]
#[tokio::test(flavor = "multi_thread")]
async fn a_series_identify_resolves_its_children_by_the_chosen_ids(
    #[case] ids: &[(&str, &str)],
    #[case] provider: &str,
    #[case] children_ask_tmdb: bool,
) {
    let (base, log) = spawn_tmdb();
    let tmp = tempfile::tempdir().expect("tmp");
    let tv = tmp.path().join("tv");
    let series = tv.join("GoT");
    let season = series.join("Season 01");
    std::fs::create_dir_all(&season).expect("mkdir");
    std::fs::write(season.join("GoT S01E01.mkv"), b"").expect("write");

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
        .with_items(items);
    let asked_children = || {
        log.lock()
            .expect("log")
            .iter()
            .any(|l| l.contains("/tv/1399/season/1") && !l.contains("/images"))
    };

    scanner.scan_all().await.expect("scan");
    assert!(asked_children(), "the first scan titles the episode");
    log.lock().expect("log").clear();

    let chosen = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::FullRefresh,
        replace_all_metadata: true,
        replace_all_images: true,
        remove_old_metadata: true,
        search_result: Some(RemoteSearchResult {
            name: Some("GoT".into()),
            provider_ids: Some(
                ids.iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect::<HashMap<_, _>>(),
            ),
            search_provider_name: Some(provider.into()),
            ..RemoteSearchResult::default()
        }),
        ..MetadataRefreshOptions::default()
    };
    let none = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::None,
        image_refresh_mode: MetadataRefreshMode::None,
        ..MetadataRefreshOptions::default()
    };
    scanner
        .scan_target(
            &ScanTarget::Items(vec![series.to_string_lossy().into_owned()]),
            ScanRun::new(&chosen, &none, &ScanCancel::new()).with_passes(ScanPasses::Touched),
        )
        .await
        .expect("identify");
    let asked = log.lock().expect("log").clone();
    assert!(
        asked.iter().any(|l| l.contains("/tv/1399?")),
        "TheMovieDb matched the series itself: {asked:?}"
    );
    assert_eq!(asked_children(), children_ask_tmdb, "{asked:?}");
}
