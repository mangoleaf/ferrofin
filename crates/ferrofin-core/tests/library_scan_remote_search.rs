//! The name the scan searches TheMovieDb and OMDb by, for an item with no id.
//!
//! An own-folder movie is named after its folder, year included
//! (`MovieResolver.FindMovie`), so its name is `Split (2016)`. Upstream's
//! providers search by `LibraryManager.ParseName` of it: the title without
//! the year or release clutter, with the year in the name as the search year
//! when the item has none (`TmdbMovieProvider.GetMetadata`,
//! `TmdbSeriesProvider.GetMetadata`, `OmdbItemProvider.GetSearchResultsInternal`).
//! TMDb finds nothing for `Split 2016`, so these stand-ins check the query
//! string itself.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_providers::{OmdbClient, TmdbClient};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// Every request line the stand-in received.
type Requests = Arc<Mutex<Vec<String>>>;

/// A provider stand-in on a local port: `answer` maps each request line to a
/// status and a JSON body. Returns the base URL and the request lines.
fn spawn(answer: fn(&str) -> (&'static str, String)) -> (String, Requests) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default().to_owned();
            log.lock().expect("log").push(line.clone());
            let (status, payload) = answer(&line);
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), requests)
}

/// The decoded query parameters of the first request line whose path
/// contains `path`.
fn query_of(requests: &Requests, path: &str) -> Vec<(String, String)> {
    let lines = requests.lock().expect("log").clone();
    let line = lines
        .iter()
        .find(|line| line.contains(path))
        .unwrap_or_else(|| panic!("no {path} request in {lines:?}"));
    let target = line.split(' ').nth(1).expect("request target");
    reqwest::Url::parse(&format!("http://stand-in{target}"))
        .expect("url")
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .filter(|(key, _)| key != "api_key" && key != "apikey")
        .collect()
}

/// The value of `key` in `query`.
fn param<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.as_str())
}

/// Creates the empty file `rel` (and its folders) under `root`.
fn touch(root: &Path, rel: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dirs");
    std::fs::write(path, b"").expect("fixture file");
}

/// The provider ids stored for the item at `path`, as `Provider=Value`.
async fn ids_of(db: &Database, path: &Path) -> Vec<String> {
    sqlx::query_scalar(
        r#"SELECT p."ProviderId" || '=' || p."ProviderValue" FROM "BaseItemProviders" p
           JOIN "BaseItems" b ON b."Id" = p."ItemId"
           WHERE b."Path" = ?1 ORDER BY 1"#,
    )
    .bind(path.to_string_lossy().into_owned())
    .fetch_all(db.pool())
    .await
    .expect("provider ids")
}

/// A library of `kind` over `media` on a fresh in-memory database, and a
/// scanner over it with no remote provider wired yet.
async fn library(
    root: &Path,
    media: &Path,
    kind: CollectionTypeOptions,
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
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items);
    (db, scanner)
}

/// `/search/movie` lists a title that only shares the year's neighbourhood
/// and a prefix ("Split Decision", 2016) ahead of the exact title from
/// another year ("Split", 2010); `/movie/20` is the exact title's record.
fn tmdb_movie(line: &str) -> (&'static str, String) {
    if line.contains("/search/movie") {
        (
            "200 OK",
            r#"{"results": [
                {"id": 10, "title": "Split Decision", "release_date": "2016-05-01"},
                {"id": 20, "title": "Split", "release_date": "2010-01-01"}
            ]}"#
            .to_owned(),
        )
    } else if line.contains("/movie/20?") {
        (
            "200 OK",
            r#"{"id": 20, "title": "Split", "overview": "From TMDB.",
                "release_date": "2010-01-01"}"#
                .to_owned(),
        )
    } else {
        ("404 Not Found", "{}".to_owned())
    }
}

/// TMDb is searched with the folder name parsed: `query=Split`, with the
/// year the name carries, and the hits are scored against `Split`, so the
/// exact title (8) beats a prefix hit in the right year (4 + 2). Searched
/// with `Split 2016` and scored against it, no title scores and the first
/// hit in the year's range wins.
#[tokio::test]
async fn a_movie_folder_searches_tmdb_by_its_parsed_name() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media, "Split (2016)/Split (2016).mkv");
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, requests) = spawn(tmdb_movie);

    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .scan_all()
        .await
        .expect("scan");

    let search = query_of(&requests, "/search/movie");
    assert_eq!(param(&search, "query"), Some("Split"), "{search:?}");
    assert_eq!(param(&search, "year"), Some("2016"), "{search:?}");
    assert_eq!(
        ids_of(&db, &media.join("Split (2016)/Split (2016).mkv")).await,
        ["Tmdb=20"],
        "the exact title wins: {:?}",
        requests.lock().expect("log")
    );
}

/// `/search/tv` answers only the parsed name, as TMDb finds nothing for
/// `Show 2026`.
fn tmdb_series(line: &str) -> (&'static str, String) {
    if line.contains("/search/tv?") && line.contains("query=Show&") {
        (
            "200 OK",
            r#"{"results": [{"id": 30, "name": "Show", "first_air_date": "2026-01-05"}]}"#
                .to_owned(),
        )
    } else if line.contains("/search/tv?") {
        ("200 OK", r#"{"results": []}"#.to_owned())
    } else if line.contains("/tv/30?") {
        (
            "200 OK",
            r#"{"id": 30, "name": "Show", "overview": "From TMDB."}"#.to_owned(),
        )
    } else {
        ("404 Not Found", "{}".to_owned())
    }
}

/// A series folder with a bare year (`Show 2026`) keeps it in its name; the
/// series search parses it out (`TmdbSeriesProvider.GetMetadata`).
#[tokio::test]
async fn a_series_folder_with_a_bare_year_searches_tmdb_by_its_parsed_name() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    touch(&media, "Show 2026/Season 1/Show 2026 S01E01.mkv");
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::tvshows).await;
    let (base, requests) = spawn(tmdb_series);

    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .scan_all()
        .await
        .expect("scan");

    let search = query_of(&requests, "/search/tv");
    assert_eq!(param(&search, "query"), Some("Show"), "{search:?}");
    assert_eq!(
        param(&search, "first_air_date_year"),
        Some("2026"),
        "{search:?}"
    );
    assert_eq!(ids_of(&db, &media.join("Show 2026")).await, ["Tmdb=30"]);
}

/// A movie and a series whose IMDb id (here from the folder name) TMDb's
/// `/find` cannot resolve, answering 404 (`TMDbLib`'s `null`, not a
/// failure, under `ThrowApiExceptions = false`): the movie gets no TMDb
/// metadata and no name search (`TmdbMovieProvider.GetMetadata` searches only
/// without an IMDb id, `TmdbMovieProvider.cs:160,178-190`), while the series
/// goes on to its name search (`TmdbSeriesProvider.cs:178-213`).
#[rstest::rstest]
#[case::movie(
    CollectionTypeOptions::movies,
    tmdb_movie as fn(&str) -> (&'static str, String),
    "Split (2016) [imdbid-tt0000404]/Split (2016).mkv",
    "Split (2016) [imdbid-tt0000404]/Split (2016).mkv",
    "/search/movie",
    &["Imdb=tt0000404"]
)]
#[case::series(
    CollectionTypeOptions::tvshows,
    tmdb_series as fn(&str) -> (&'static str, String),
    "Show 2026 [imdbid-tt0000404]/Season 1/Show 2026 S01E01.mkv",
    "Show 2026 [imdbid-tt0000404]",
    "/search/tv",
    &["Imdb=tt0000404", "Tmdb=30"]
)]
#[tokio::test]
async fn an_imdb_id_tmdb_cannot_resolve_stops_only_a_movies_name_search(
    #[case] kind: CollectionTypeOptions,
    #[case] tmdb: fn(&str) -> (&'static str, String),
    #[case] file: &str,
    #[case] item: &str,
    #[case] search: &str,
    #[case] ids: &[&str],
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("media");
    touch(&media, file);
    let (db, scanner) = library(tmp.path(), &media, kind).await;
    let (base, requests) = spawn(tmdb);

    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .scan_all()
        .await
        .expect("scan");

    let asked = requests.lock().expect("log").clone();
    assert!(
        asked.iter().any(|line| line.contains("/find/tt0000404")),
        "{asked:?}"
    );
    assert_eq!(
        asked.iter().any(|line| line.contains(search)),
        kind == CollectionTypeOptions::tvshows,
        "{asked:?}"
    );
    assert_eq!(ids_of(&db, &media.join(item)).await, ids);
}

/// Whether the item at `path` has its `DateLastRefreshed` stamp.
async fn refreshed(db: &Database, path: &Path) -> bool {
    sqlx::query_scalar(
        r#"SELECT "DateLastRefreshed" IS NOT NULL FROM "BaseItems" WHERE "Path" = ?1"#,
    )
    .bind(path.to_string_lossy().into_owned())
    .fetch_one(db.pool())
    .await
    .expect("stamp")
}

/// `/find` answers with no title; anything else as [`tmdb_movie`].
fn tmdb_find_empty(line: &str) -> (&'static str, String) {
    if line.contains("/find/") {
        (
            "200 OK",
            r#"{"movie_results": [], "tv_results": []}"#.to_owned(),
        )
    } else {
        tmdb_movie(line)
    }
}

/// Whether [`tmdb_find_recovers`]' `/find` answers yet.
static FIND_ANSWERS: AtomicBool = AtomicBool::new(false);

/// `/find` fails (500) until [`FIND_ANSWERS`] is set, then resolves to the
/// exact title, `/movie/20`, which `/search/movie` would also match by name.
fn tmdb_find_recovers(line: &str) -> (&'static str, String) {
    if !line.contains("/find/") {
        tmdb_movie(line)
    } else if FIND_ANSWERS.load(Ordering::SeqCst) {
        (
            "200 OK",
            r#"{"movie_results": [{"id": 20}], "tv_results": []}"#.to_owned(),
        )
    } else {
        ("500 Internal Server Error", "{}".to_owned())
    }
}

/// A movie whose IMDb id TMDb's `/find` answers with no title is not searched
/// by name, and the refresh is stamped: an empty answer is not a failure
/// (`TmdbMovieProvider.cs:178-190` returns an empty result).
#[tokio::test]
async fn a_movie_imdb_id_tmdb_finds_nothing_for_is_stamped_unmatched() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = "Split (2016) [imdbid-tt0000404]/Split (2016).mkv";
    touch(&media, file);
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, requests) = spawn(tmdb_find_empty);

    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .scan_all()
        .await
        .expect("scan");

    let asked = requests.lock().expect("log").clone();
    assert!(
        asked.iter().any(|l| l.contains("/find/tt0000404")),
        "{asked:?}"
    );
    assert!(asked.iter().all(|l| !l.contains("/search/")), "{asked:?}");
    let path = media.join(file);
    assert_eq!(ids_of(&db, &path).await, ["Imdb=tt0000404"]);
    assert!(refreshed(&db, &path).await, "no provider failed");
}

/// A movie whose `/find` request fails (a 500 through every retry) is not
/// searched by name either, but the failure is reported: the refresh is not
/// stamped, so the next scan asks `/find` again and matches it then.
/// Upstream's `FindByExternalIdAsync` throws for a 5xx (`TMDbLib`'s
/// `GeneralHttpException`; only a 404 is `null`), out of
/// `TmdbMovieProvider.GetMetadata`; `ExecuteRemoteProviders` counts it
/// (`Failures++`, `MetadataService.cs:1017-1021`), and `RefreshMetadata`
/// leaves `DateLastRefreshed` unset (`:188-190`, `:221-223`).
#[tokio::test]
async fn a_failed_tmdb_find_is_asked_again_by_the_next_scan_not_searched_by_name() {
    // Every retry of the 500 is still sent; the limiter's own suite tests
    // the backoff between them.
    ferrofin_providers::rate_limit::test_support::skip_backoff();
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = "Split (2016) [imdbid-tt0000500]/Split (2016).mkv";
    touch(&media, file);
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, requests) = spawn(tmdb_find_recovers);
    // TMDb's image provider asks `/find` too, and its failure would keep the
    // refresh unstamped on its own: off, so the stamp is the metadata
    // provider's.
    let scanner = scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .with_metadata_options(|| {
            vec![ferrofin_model::configuration::MetadataOptions {
                item_type: Some("Movie".to_owned()),
                disabled_image_fetchers: vec!["TheMovieDb".to_owned()],
                ..Default::default()
            }]
        });
    let path = media.join(file);

    scanner.scan_all().await.expect("scan");
    let asked = std::mem::take(&mut *requests.lock().expect("log"));
    assert!(
        asked.iter().any(|l| l.contains("/find/tt0000500")),
        "{asked:?}"
    );
    assert!(asked.iter().all(|l| !l.contains("/search/")), "{asked:?}");
    assert_eq!(ids_of(&db, &path).await, ["Imdb=tt0000500"]);
    assert!(
        !refreshed(&db, &path).await,
        "the failure leaves it unstamped"
    );

    FIND_ANSWERS.store(true, Ordering::SeqCst);
    scanner.scan_all().await.expect("second scan");
    let asked = requests.lock().expect("log").clone();
    assert!(
        asked.iter().any(|l| l.contains("/find/tt0000500")),
        "{asked:?}"
    );
    assert!(asked.iter().all(|l| !l.contains("/search/")), "{asked:?}");
    assert_eq!(ids_of(&db, &path).await, ["Imdb=tt0000500", "Tmdb=20"]);
    assert!(refreshed(&db, &path).await);
}

/// The series metadata options with TMDb's image provider off: it asks
/// `/find` too, and its failure would keep the refresh unstamped on its own.
fn series_without_tmdb_images() -> Vec<ferrofin_model::configuration::MetadataOptions> {
    vec![ferrofin_model::configuration::MetadataOptions {
        item_type: Some("Series".to_owned()),
        disabled_image_fetchers: vec!["TheMovieDb".to_owned()],
        ..Default::default()
    }]
}

/// `/find` answers with no title, by either id; anything else as
/// [`tmdb_series`].
fn tmdb_series_find_empty(line: &str) -> (&'static str, String) {
    if line.contains("/find/") {
        (
            "200 OK",
            r#"{"movie_results": [], "tv_results": []}"#.to_owned(),
        )
    } else {
        tmdb_series(line)
    }
}

/// A series whose IMDb id TMDb's `/find` answers with no title goes on to the
/// TVDB `/find`, then to the name search, and the refresh is stamped: an
/// empty answer is not a failure (`TmdbSeriesProvider.cs:178-213`).
#[tokio::test]
async fn a_series_find_answered_empty_goes_on_to_the_tvdb_find_and_the_search() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    let folder = "Show 2026 [imdbid-tt0000404] [tvdbid-77]";
    touch(&media, &format!("{folder}/Season 1/Show 2026 S01E01.mkv"));
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::tvshows).await;
    let (base, requests) = spawn(tmdb_series_find_empty);

    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .with_metadata_options(series_without_tmdb_images)
        .scan_all()
        .await
        .expect("scan");

    let asked = requests.lock().expect("log").clone();
    for path in ["/find/tt0000404", "/find/77", "/search/tv"] {
        assert!(asked.iter().any(|l| l.contains(path)), "{path}: {asked:?}");
    }
    let path = media.join(folder);
    assert_eq!(
        ids_of(&db, &path).await,
        ["Imdb=tt0000404", "Tmdb=30", "Tvdb=77"]
    );
    assert!(refreshed(&db, &path).await, "no provider failed");
}

/// Whether [`tmdb_series_find_recovers`]' `/find` answers yet.
static SERIES_FIND_ANSWERS: AtomicBool = AtomicBool::new(false);

/// `/find` fails (500) until [`SERIES_FIND_ANSWERS`] is set, then resolves
/// the IMDb id to `/tv/31`, a show the name search (`/tv/30`) would not pick.
fn tmdb_series_find_recovers(line: &str) -> (&'static str, String) {
    if line.contains("/tv/31?") {
        (
            "200 OK",
            r#"{"id": 31, "name": "Show", "overview": "From /find."}"#.to_owned(),
        )
    } else if !line.contains("/find/") {
        tmdb_series(line)
    } else if SERIES_FIND_ANSWERS.load(Ordering::SeqCst) {
        (
            "200 OK",
            r#"{"movie_results": [], "tv_results": [{"id": 31}]}"#.to_owned(),
        )
    } else {
        ("500 Internal Server Error", "{}".to_owned())
    }
}

/// A series whose IMDb `/find` request fails (a 500 through every retry)
/// asks neither the TVDB `/find` nor the name search, so it keeps no guessed
/// `Tmdb` id; the refresh is not stamped, and the next scan asks `/find`
/// again and stores the id it resolves. Upstream's `FindByExternalIdAsync`
/// throws out of `TmdbSeriesProvider.GetMetadata` before the TVDB `/find` and
/// `SearchSeriesAsync` (`TmdbSeriesProvider.cs:178-213`);
/// `ExecuteRemoteProviders` counts it (`MetadataService.cs:1017-1021`).
#[tokio::test]
async fn a_failed_tmdb_find_leaves_a_series_unmatched_until_the_next_scan() {
    // Every retry of the 500 is still sent; the limiter's own suite tests
    // the backoff between them.
    ferrofin_providers::rate_limit::test_support::skip_backoff();
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    let folder = "Show 2026 [imdbid-tt0000500] [tvdbid-77]";
    touch(&media, &format!("{folder}/Season 1/Show 2026 S01E01.mkv"));
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::tvshows).await;
    let (base, requests) = spawn(tmdb_series_find_recovers);
    let scanner = scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .with_metadata_options(series_without_tmdb_images);
    let path = media.join(folder);

    scanner.scan_all().await.expect("scan");
    let asked = std::mem::take(&mut *requests.lock().expect("log"));
    assert!(
        asked.iter().any(|l| l.contains("/find/tt0000500")),
        "{asked:?}"
    );
    assert!(
        asked
            .iter()
            .all(|l| !l.contains("/find/77") && !l.contains("/search/")),
        "{asked:?}"
    );
    assert_eq!(ids_of(&db, &path).await, ["Imdb=tt0000500", "Tvdb=77"]);
    assert!(
        !refreshed(&db, &path).await,
        "the failure leaves it unstamped"
    );

    SERIES_FIND_ANSWERS.store(true, Ordering::SeqCst);
    scanner.scan_all().await.expect("second scan");
    let asked = requests.lock().expect("log").clone();
    assert!(
        asked.iter().any(|l| l.contains("/find/tt0000500")),
        "{asked:?}"
    );
    assert!(asked.iter().all(|l| !l.contains("/search/")), "{asked:?}");
    assert_eq!(
        ids_of(&db, &path).await,
        ["Imdb=tt0000500", "Tmdb=31", "Tvdb=77"]
    );
    assert!(refreshed(&db, &path).await);
}

/// `/find` fails (500); anything else as [`tmdb_series`].
fn tmdb_series_find_fails(line: &str) -> (&'static str, String) {
    if line.contains("/find/") {
        ("500 Internal Server Error", "{}".to_owned())
    } else {
        tmdb_series(line)
    }
}

/// A series with only a TVDB id whose `/find` request fails is not searched
/// by name either, and is left unstamped (`FindByExternalIdAsync` throws out
/// of `TmdbSeriesProvider.GetMetadata` there too).
#[tokio::test]
async fn a_failed_tvdb_find_leaves_a_series_unsearched() {
    ferrofin_providers::rate_limit::test_support::skip_backoff();
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    let folder = "Show 2026 [tvdbid-77]";
    touch(&media, &format!("{folder}/Season 1/Show 2026 S01E01.mkv"));
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::tvshows).await;
    let (base, requests) = spawn(tmdb_series_find_fails);

    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .with_metadata_options(series_without_tmdb_images)
        .scan_all()
        .await
        .expect("scan");

    let asked = requests.lock().expect("log").clone();
    assert!(asked.iter().any(|l| l.contains("/find/77")), "{asked:?}");
    assert!(asked.iter().all(|l| !l.contains("/search/")), "{asked:?}");
    let path = media.join(folder);
    assert_eq!(ids_of(&db, &path).await, ["Tvdb=77"]);
    assert!(
        !refreshed(&db, &path).await,
        "the failure leaves it unstamped"
    );
}

/// OMDb's exact-title lookup answers only `t=Ocean's Eleven&y=2001`.
fn omdb(line: &str) -> (&'static str, String) {
    let Some(target) = line.split(' ').nth(1) else {
        return ("400 Bad Request", "{}".to_owned());
    };
    let url = reqwest::Url::parse(&format!("http://stand-in{target}")).expect("url");
    let has = |key: &str, value: &str| url.query_pairs().any(|(k, v)| k == key && v == value);
    if has("t", "Ocean's Eleven") && has("y", "2001") {
        (
            "200 OK",
            r#"{"Title": "Ocean's Eleven", "Year": "2001", "Plot": "From OMDb.",
                "imdbID": "tt0240772", "Response": "True"}"#
                .to_owned(),
        )
    } else {
        (
            "200 OK",
            r#"{"Response": "False", "Error": "Movie not found!"}"#.to_owned(),
        )
    }
}

/// OMDb's title lookup (`GetResult` → `GetImdbId`) parses the name too, and
/// sends the parsed title as it is: upstream URL-encodes it and never runs
/// TheMovieDb's `CleanName` on it, so the apostrophe stays.
#[tokio::test]
async fn a_movie_folder_searches_omdb_by_its_parsed_name() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media, "Ocean's Eleven (2001)/Ocean's Eleven (2001).mkv");
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, requests) = spawn(omdb);

    scanner
        .with_omdb(Arc::new(
            OmdbClient::new("key").with_base_url(&format!("{base}/omdb/")),
        ))
        .scan_all()
        .await
        .expect("scan");

    let lookup = query_of(&requests, "/omdb/");
    assert_eq!(param(&lookup, "t"), Some("Ocean's Eleven"), "{lookup:?}");
    assert_eq!(param(&lookup, "y"), Some("2001"), "{lookup:?}");
    assert_eq!(
        ids_of(
            &db,
            &media.join("Ocean's Eleven (2001)/Ocean's Eleven (2001).mkv")
        )
        .await,
        ["Imdb=tt0240772"]
    );
}

/// The item's own year is the search year, and only an item without one
/// searches by the year in its name (`info.Year ?? parsedName.Year`, in both
/// providers). An own-folder movie is named after its folder but dated by its
/// file (`ResolveVideos`: `ProductionYear = video.Year`), so the movie in
/// `Ocean's Eleven (2001)/Ocean's Eleven (2002).mkv` is named after the folder
/// and dated 2002. TheMovieDb gets the parsed title through `CleanName`
/// (punctuation to spaces); OMDb gets it as it is.
#[tokio::test]
async fn the_items_own_year_wins_over_the_year_in_its_name() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media, "Ocean's Eleven (2001)/Ocean's Eleven (2002).mkv");
    let (_, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, requests) = spawn(tmdb_movie);
    scanner
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.path().join("metadata"),
        )
        .scan_all()
        .await
        .expect("TMDb scan");
    let search = query_of(&requests, "/search/movie");
    assert_eq!(
        param(&search, "query"),
        Some("Ocean s Eleven"),
        "{search:?}"
    );
    assert_eq!(param(&search, "year"), Some("2002"), "{search:?}");

    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    touch(&media, "Ocean's Eleven (2001)/Ocean's Eleven (2002).mkv");
    let (_, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, requests) = spawn(omdb);
    scanner
        .with_omdb(Arc::new(
            OmdbClient::new("key").with_base_url(&format!("{base}/omdb/")),
        ))
        .scan_all()
        .await
        .expect("OMDb scan");
    let lookup = query_of(&requests, "/omdb/");
    assert_eq!(param(&lookup, "t"), Some("Ocean's Eleven"), "{lookup:?}");
    assert_eq!(param(&lookup, "y"), Some("2002"), "{lookup:?}");
}
