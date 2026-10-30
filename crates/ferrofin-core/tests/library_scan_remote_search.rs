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
