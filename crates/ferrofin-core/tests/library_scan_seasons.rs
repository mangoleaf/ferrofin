//! The season remote metadata providers, end to end through a scan:
//! `TmdbSeasonProvider` and the TVDB plugin's `TvdbSeasonProvider` folded as
//! `MetadataService.ExecuteRemoteProviders` folds every kind's providers
//! (`MetadataService.cs:955-1025`), under the same refresh decision.
//!
//! One series (`Show`, TMDB 1399, TheTVDB 121361) with one season folder and
//! one episode, against one stand-in serving TheMovieDb under `/tmdb` and
//! TheTVDB under `/tvdb`, which logs every request line. What the season
//! asks for is asserted on that log: the season request TheMovieDb's season
//! provider shares with the season's episodes and poster, the season record
//! TheTVDB's reads, and the series record the season's TVDB id comes from.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::people_repository::FerrofinPeopleRepository;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, TypeOptions};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, MetadataField};
use ferrofin_providers::library_options::fetcher_names::{TMDB, TVDB};
use ferrofin_providers::{FanartClient, TmdbClient, TvdbClient};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemRepository, PeopleRepository as _};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

/// TheMovieDb's season: its own id, overview, air date, one credited actor
/// and the TheTVDB id of its `external_ids`; the episode it lists is titled,
/// so no episode backfill asks again.
const TMDB_SEASON: &str = r#"{"id": 3624, "name": "Season One On TMDB",
    "overview": "TMDB season.", "air_date": "2011-04-17",
    "credits": {"cast": [{"id": 22, "name": "Sean Bean", "character": "Ned", "order": 0}],
                "crew": []},
    "external_ids": {"tvdb_id": 364731},
    "episodes": [{"id": 63056, "episode_number": 1, "name": "Winter Is Coming",
                  "overview": "Ned is summoned.", "air_date": "2011-04-17"}]}"#;

/// [`TMDB_SEASON`] with no overview.
const TMDB_SEASON_NO_OVERVIEW: &str = r#"{"id": 3624, "overview": "",
    "air_date": "2011-04-17",
    "credits": {"cast": [{"id": 22, "name": "Sean Bean", "character": "Ned", "order": 0}],
                "crew": []},
    "episodes": [{"id": 63056, "episode_number": 1, "name": "Winter Is Coming",
                  "overview": "Ned is summoned.", "air_date": "2011-04-17"}]}"#;

/// TheMovieDb's series: an overview and a trailer (so no D2 backfill asks for
/// it again) and its TheTVDB id.
const TMDB_SERIES: &str = r#"{"id": 1399, "name": "Show", "overview": "TMDB series.",
    "first_air_date": "2011-04-17", "external_ids": {"tvdb_id": 121361},
    "videos": {"results": [{"site": "YouTube", "type": "Trailer", "key": "k", "name": "T"}]}}"#;

/// TheMovieDb finds the series by name.
const TMDB_FOUND: &str = r#"{"results": [{"id": 1399, "name": "Show"}]}"#;

/// TheMovieDb has no such series.
const TMDB_NOT_FOUND: &str = r#"{"results": []}"#;

/// TheTVDB's series record, full or short: it lists the season, `official`
/// season 1 = TVDB season 77.
const TVDB_SERIES: &str = r#"{"data": {"id": 121361, "name": "Show",
    "overview": "TVDB series.",
    "seasons": [{"id": 76, "number": 1, "type": {"type": "dvd"}},
                {"id": 77, "number": 1, "type": {"type": "official"}}]}}"#;

/// TheTVDB's season record: its text is in its translations only.
const TVDB_SEASON: &str = r#"{"data": {"id": 77, "translations": {"overviewTranslations": [
    {"language": "fra", "overview": "Saison TVDB."},
    {"language": "eng", "overview": "TVDB season."}]}}}"#;

/// TheTVDB's `dvd`-ordered season 1 (id 76).
const TVDB_DVD_SEASON: &str = r#"{"data": {"id": 76, "translations": {"overviewTranslations": [
    {"language": "eng", "overview": "TVDB dvd season."}]}}}"#;

/// TheMovieDb's posters for season 1, in TheMovieDb's own order: the German
/// one first and best rated, then English, then French. Their files are
/// requested from the stand-in's `/img` root, which answers each with a 404,
/// so the scan tries every one and the request log shows its ranked order.
const TMDB_SEASON_IMAGES: &str = r#"{"posters": [
    {"file_path": "/season-de.jpg", "iso_639_1": "de", "vote_average": 9.5, "vote_count": 90},
    {"file_path": "/season-en.jpg", "iso_639_1": "en", "vote_average": 7.0, "vote_count": 40},
    {"file_path": "/season-fr.jpg", "iso_639_1": "fr", "vote_average": 5.0, "vote_count": 10}]}"#;

/// How the stand-in answers TheMovieDb's series search and season.
#[derive(Clone, Copy)]
struct Answers {
    search: &'static str,
    season: &'static str,
}

const DEFAULT_ANSWERS: Answers = Answers {
    search: TMDB_FOUND,
    season: TMDB_SEASON,
};

/// The stand-in's answer to request line `line`; `None` is a 404 (a miss,
/// not a failure).
fn answer(line: &str, answers: Answers) -> Option<&'static str> {
    let routes: [(&str, &str); 15] = [
        ("/tvdb/login", r#"{"data": {"token": "tok"}}"#),
        (
            "/tvdb/search",
            r#"{"data": [{"tvdb_id": "121361", "name": "Show", "year": "2011"}]}"#,
        ),
        (
            "/tvdb/series/121361/episodes/official",
            r#"{"data": {"episodes": [{"id": 9001, "seasonNumber": 1, "number": 1}]}}"#,
        ),
        (
            "/tvdb/series/121361/episodes/dvd",
            r#"{"data": {"episodes": [{"id": 9002, "seasonNumber": 1, "number": 1}]}}"#,
        ),
        (
            "/tvdb/episodes/9002/extended",
            r#"{"data": {"id": 9002, "name": "Dvd cut", "characters": []}}"#,
        ),
        ("/tvdb/seasons/76/extended", TVDB_DVD_SEASON),
        ("/tvdb/series/121361/extended", TVDB_SERIES),
        ("/tvdb/seasons/77/extended", TVDB_SEASON),
        (
            "/tvdb/episodes/9001/extended",
            r#"{"data": {"id": 9001, "name": "Winter Is Coming", "overview": "TVDB episode.",
                "characters": []}}"#,
        ),
        ("/tmdb/search/tv", answers.search),
        (
            "/tmdb/tv/1399/season/1/episode/1",
            r#"{"credits": {"cast": [], "crew": []}}"#,
        ),
        ("/tmdb/tv/1399/season/1/images", TMDB_SEASON_IMAGES),
        ("/tmdb/tv/1399/season/1?", answers.season),
        ("/tmdb/tv/1399?", TMDB_SERIES),
        // A biography, so the person is complete and not asked again.
        (
            "/tmdb/person/",
            r#"{"id": 22, "name": "Sean Bean", "biography": "An actor."}"#,
        ),
    ];
    routes
        .iter()
        .find(|(route, _)| line.contains(route))
        .map(|(_, body)| *body)
}

/// Starts the stand-in; returns its base URL and its request log.
fn spawn_providers(answers: Answers) -> (String, Arc<Mutex<Vec<String>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default().to_owned();
            // fanart.tv's document for the series (TVDB 121361): season 1's poster
            // and banner, season 2's poster, an all-seasons background — their
            // urls point back here so the scan's downloads are observable.
            let fanart_document = format!(
                r#"{{"seasonposter":[{{"url":"http://{addr}/img/poster-1.jpg","lang":"en","season":"1","likes":"4"}},
                    {{"url":"http://{addr}/img/poster-2.jpg","lang":"en","season":"2","likes":"9"}}],
                  "seasonbanner":[{{"url":"http://{addr}/img/banner-1.jpg","season":"1"}}],
                  "showbackground":[{{"url":"http://{addr}/img/bg-all.jpg","season":"all"}}]}}"#
            );
            let (status, payload) = if line.contains("/fanart/tv/121361") {
                ("200 OK", fanart_document.as_str())
            } else {
                answer(&line, answers).map_or(("404 Not Found", "{}"), |b| ("200 OK", b))
            };
            log.lock().expect("log").push(line);
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), requests)
}

/// A base URL nothing listens on: every request to it fails to connect, as
/// the adoption gates' unreachable proxy makes every provider request fail.
fn unreachable_base() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    format!("http://{addr}")
}

/// A one-series TV library over an in-memory database.
struct Library {
    db: Database,
    vf: Arc<dyn VirtualFolderManager>,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: Arc<dyn ItemRepository>,
    people: Arc<FerrofinPeopleRepository>,
    season_dir: PathBuf,
    season_id: Uuid,
    metadata: PathBuf,
}

impl Library {
    /// `tv/Show/Season 1/Show S01E01.mkv` in a library saved with `options`.
    async fn new(tmp: &Path, options: LibraryOptions) -> Self {
        let tv = tmp.join("tv");
        let season_dir = tv.join("Show").join("Season 1");
        std::fs::create_dir_all(&season_dir).expect("mkdir");
        std::fs::write(season_dir.join("Show S01E01.mkv"), b"0123456789").expect("write");
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "TV",
            Some(CollectionTypeOptions::tvshows),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: tv.to_string_lossy().into_owned(),
                }],
                ..options
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let people = Arc::new(FerrofinPeopleRepository::new(db.clone()));
        let season_id =
            derive_item_id(BaseItemKind::Season, &season_dir.to_string_lossy()).expect("season id");
        Self {
            db,
            vf,
            persistence,
            items,
            people,
            season_dir,
            season_id,
            metadata: tmp.join("metadata"),
        }
    }

    /// A scanner whose TheMovieDb and TheTVDB clients talk to `base`.
    fn scanner(&self, base: &str) -> LibraryScanner {
        self.scanner_with(base, base)
    }

    /// A scanner whose TheMovieDb client talks to `the_moviedb` and TheTVDB
    /// client to `the_tvdb`.
    fn scanner_with(&self, the_moviedb: &str, the_tvdb: &str) -> LibraryScanner {
        LibraryScanner::new(
            Arc::clone(&self.vf),
            Arc::new(FerrofinFileSystem::new()),
            self.persistence.clone(),
        )
        .with_items(Arc::clone(&self.items))
        .with_metadata(
            Arc::new(
                TmdbClient::new()
                    .with_base_url(&format!("{the_moviedb}/tmdb"))
                    .with_image_root(&format!("{the_moviedb}/img")),
            ),
            self.metadata.clone(),
        )
        .with_tvdb(Arc::new(
            TvdbClient::new().with_base_url(&format!("{the_tvdb}/tvdb")),
        ))
        .with_fanart(Arc::new(
            FanartClient::new(None).with_base_url(&format!("{the_moviedb}/fanart")),
        ))
        .with_people(
            Arc::clone(&self.people) as Arc<dyn ferrofin_traits::persistence::PeopleRepository>
        )
    }

    /// The season's stored row.
    async fn season(&self) -> BaseItemEntity {
        self.items
            .retrieve_item(self.season_id)
            .await
            .expect("read")
            .expect("the season")
    }

    /// The season's stored provider ids, sorted.
    async fn season_ids(&self) -> Vec<(String, String)> {
        let mut ids: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders" WHERE "ItemId" = ?1"#,
        )
        .bind(ferrofin_db::store::guid_to_db(self.season_id))
        .fetch_all(self.db.pool())
        .await
        .expect("ids");
        ids.sort();
        ids
    }

    /// The season's credited names and roles.
    async fn cast(&self) -> Vec<(String, Option<String>)> {
        self.people
            .get_people_batch(&[self.season_id])
            .await
            .expect("credits")
            .remove(&self.season_id)
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.name, p.role))
            .collect()
    }

    /// Clears the season's `DateLastRefreshed`: its `isFirstRefresh` on the
    /// next scan, which runs every provider in `Default` mode
    /// (`MetadataService.RefreshMetadata`). A changed season folder does not:
    /// a directory has no `DateModified` (D2), and `BaseItem.RequiresRefresh`
    /// is false for a `MinValue` one (`BaseItem.cs:1721-1731`).
    async fn forget_season_refresh(&self) {
        self.set_season(r#""DateLastRefreshed" = NULL"#).await;
    }

    /// Sets one column of the season's row.
    async fn set_season(&self, assignment: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"UPDATE "BaseItems" SET {assignment} WHERE "Id" = ?1"#
        )))
        .bind(ferrofin_db::store::guid_to_db(self.season_id))
        .execute(self.db.writer())
        .await
        .expect("update the season");
    }
}

/// The request lines of `log` containing `needle`.
fn asked(log: &Mutex<Vec<String>>, needle: &str) -> usize {
    log.lock()
        .expect("log")
        .iter()
        .filter(|line| line.contains(needle))
        .count()
}

/// Empties `log`, returning what it held.
fn drain(log: &Mutex<Vec<String>>) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

/// A library whose admin saved `order` as the Season fetchers and their
/// order (every other kind as with no saved options).
fn season_order(order: &[&str]) -> LibraryOptions {
    let names: Vec<String> = order.iter().map(|n| (*n).to_owned()).collect();
    LibraryOptions {
        type_options: vec![TypeOptions {
            type_: Some("Season".to_owned()),
            metadata_fetchers: names.clone(),
            metadata_fetcher_order: names,
            ..TypeOptions::default()
        }],
        ..LibraryOptions::default()
    }
}

/// The dashboard choices as the item-refresh route builds them.
fn dashboard(choice: &str) -> MetadataRefreshOptions {
    use MetadataRefreshMode::{Default, FullRefresh};
    match choice {
        "scan" => MetadataRefreshOptions::for_item_refresh(Default, Default, false, false, false),
        "missing" => {
            MetadataRefreshOptions::for_item_refresh(FullRefresh, FullRefresh, false, false, false)
        }
        "replace" => {
            MetadataRefreshOptions::for_item_refresh(FullRefresh, FullRefresh, true, false, false)
        }
        other => panic!("no choice {other}"),
    }
}

/// A library that runs TheMovieDb before TheTVDB for seasons: TheMovieDb's
/// season provider runs first and TheTVDB's fills only what it left: TMDB's
/// overview, air date and cast stand, and the season's Tvdb id is the one
/// TMDB's answer carried (the first answer wins a key). The season's TMDB
/// request is the one its episode shares, and its TVDB id comes from the
/// series record the series provider read: a first scan asks TheMovieDb for
/// the season once and TheTVDB for the season's record once. An unchanged
/// rescan asks nothing — a changed season folder neither (D2); a season to
/// refresh again asks both season providers again, and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_tmdb_first_season_folds_tvdb_after_it_and_shares_its_season_request() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), season_order(&[TMDB, TVDB])).await;
    let (base, log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);

    scanner.scan_all().await.expect("first scan");
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("TMDB season."));
    assert_eq!(
        season.premiere_date.map(|d| d.date_naive().to_string()),
        Some("2011-04-17".to_owned())
    );
    assert_eq!(season.production_year, Some(2011));
    assert_eq!(
        season.name.as_deref(),
        Some("Season 1"),
        "TMDb's ImportSeasonName is off by default"
    );
    assert!(season.date_last_refreshed.is_some());
    assert_eq!(
        library.season_ids().await,
        [
            ("Tmdb".to_owned(), "3624".to_owned()),
            ("Tvdb".to_owned(), "364731".to_owned())
        ]
    );
    assert_eq!(
        library.cast().await,
        [("Sean Bean".to_owned(), Some("Ned".to_owned()))]
    );
    assert_eq!(
        asked(&log, "/tmdb/tv/1399/season/1?"),
        1,
        "{:?}",
        drain(&log)
    );
    assert_eq!(asked(&log, "/tvdb/seasons/77/extended"), 1);
    assert_eq!(
        asked(&log, "short=true"),
        0,
        "the season's id came from the series record: {:?}",
        drain(&log)
    );
    drain(&log);

    scanner.scan_all().await.expect("unchanged rescan");
    assert_eq!(
        drain(&log),
        Vec::<String>::new(),
        "an unchanged season asks nothing"
    );

    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
    std::fs::File::open(&library.season_dir)
        .expect("open season dir")
        .set_modified(later)
        .expect("season mtime");
    scanner.scan_all().await.expect("folder-change rescan");
    assert_eq!(
        drain(&log),
        Vec::<String>::new(),
        "a changed season folder asks nothing"
    );

    library.forget_season_refresh().await;
    scanner.scan_all().await.expect("refreshing rescan");
    let mut asked_now: Vec<String> = drain(&log)
        .into_iter()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .map(|target| target.split('?').next().unwrap_or_default().to_owned())
        .collect();
    asked_now.sort();
    assert_eq!(
        asked_now,
        ["/tmdb/tv/1399/season/1", "/tvdb/seasons/77/extended"],
        "the season's two providers, once each"
    );
}

/// A library that runs TheTVDB before TheMovieDb for seasons — by its saved
/// order, or with no saved order at all: both season providers declare no
/// `IHasOrder` (50), so registration decides, and TheTVDB registers first
/// (Jellyfin registers the TVDB plugin's assembly before the server's,
/// `ApplicationHost.GetComposablePartAssemblies:881-886`). TheTVDB's season
/// answer (its translated overview and its own Tvdb id) wins, and
/// TheMovieDb's fills what it lacks — the air date, the year, the Tmdb id
/// and the cast (TVDB's season answer carries no `People`). The requests
/// are the same whichever leads: one season request to each.
#[rstest::rstest]
#[case::saved_order(season_order(&[TVDB, TMDB]))]
#[case::no_saved_order(LibraryOptions::default())]
#[tokio::test(flavor = "multi_thread")]
async fn a_tvdb_first_season_takes_tvdbs_overview_and_tmdbs_dates_and_cast(
    #[case] options: LibraryOptions,
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), options).await;
    let (base, log) = spawn_providers(DEFAULT_ANSWERS);

    library.scanner(&base).scan_all().await.expect("scan");
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("TVDB season."));
    assert_eq!(season.production_year, Some(2011), "TMDB's air date");
    assert!(season.premiere_date.is_some());
    assert_eq!(
        library.season_ids().await,
        [
            ("Tmdb".to_owned(), "3624".to_owned()),
            ("Tvdb".to_owned(), "77".to_owned())
        ],
        "TVDB's own id, answered first"
    );
    assert_eq!(
        library.cast().await,
        [("Sean Bean".to_owned(), Some("Ned".to_owned()))]
    );
    assert_eq!(
        asked(&log, "/tmdb/tv/1399/season/1?"),
        1,
        "{:?}",
        drain(&log)
    );
    assert_eq!(asked(&log, "/tvdb/seasons/77/extended"), 1);
}

/// `SeasonInfo`/`EpisodeInfo.SeriesDisplayOrder`: a series whose `tvshow.nfo`
/// says `dvd` has its season read from the `dvd` season TheTVDB lists (76),
/// never the official one (77), and its episode looked up under
/// `/episodes/dvd` — with no request for the official ordering.
#[tokio::test(flavor = "multi_thread")]
async fn a_dvd_ordered_series_is_looked_up_in_dvd_order() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), season_order(&[TVDB, TMDB])).await;
    std::fs::write(
        tmp.path().join("tv/Show/tvshow.nfo"),
        "<tvshow><title>Show</title><displayorder>dvd</displayorder></tvshow>",
    )
    .expect("tvshow.nfo");
    let (base, log) = spawn_providers(DEFAULT_ANSWERS);

    library.scanner(&base).scan_all().await.expect("scan");
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("TVDB dvd season."));
    assert_eq!(
        asked(&log, "/tvdb/seasons/76/extended"),
        1,
        "{:?}",
        drain(&log)
    );
    assert_eq!(asked(&log, "/tvdb/seasons/77/extended"), 0);
    assert_eq!(asked(&log, "/tvdb/series/121361/episodes/dvd"), 1);
    assert_eq!(asked(&log, "/tvdb/series/121361/episodes/official"), 0);
}

/// fanart.tv's `SeasonProvider` in a scan: with FanArt enabled for seasons the
/// series' fanart document is read once more, for the season, by the series'
/// TVDB id; with it unticked for the Season type that request is not made (the
/// series' own request is unaffected). Which images the season keeps, and the
/// `https` upgrade of their urls, are covered by the provider's unit tests.
#[tokio::test(flavor = "multi_thread")]
async fn a_season_asks_fanart_for_its_artwork_only_when_fanart_is_enabled() {
    let documents = |options: LibraryOptions| async move {
        let tmp = tempfile::tempdir().expect("tmp");
        let library = Library::new(tmp.path(), options).await;
        let (base, log) = spawn_providers(DEFAULT_ANSWERS);
        library.scanner(&base).scan_all().await.expect("scan");
        asked(&log, "/fanart/tv/121361")
    };
    let with_fanart = documents(LibraryOptions::default()).await;
    let without = documents(LibraryOptions {
        type_options: vec![TypeOptions {
            type_: Some("Season".to_owned()),
            image_fetchers: vec![TMDB.to_owned(), TVDB.to_owned()],
            ..TypeOptions::default()
        }],
        ..LibraryOptions::default()
    })
    .await;
    assert!(without >= 1, "the series still asks for its own artwork");
    assert_eq!(with_fanart, without + 1, "one more request, for the season");
}

/// A season's TheMovieDb posters are ranked like every other provider's
/// images (`GetImages` → `OrderByLanguageDescending(preferredLanguage)`), not
/// taken in TheMovieDb's order: a French library asks for the French poster
/// before the better-rated German and English ones.
#[tokio::test(flavor = "multi_thread")]
async fn a_season_takes_its_tmdb_poster_in_the_preferred_language_first() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(
        tmp.path(),
        LibraryOptions {
            preferred_metadata_language: Some("fr".to_owned()),
            ..LibraryOptions::default()
        },
    )
    .await;
    let (base, log) = spawn_providers(DEFAULT_ANSWERS);
    library.scanner(&base).scan_all().await.expect("scan");
    let posters: Vec<&str> = log
        .lock()
        .expect("log")
        .iter()
        .filter_map(|line| {
            ["season-fr", "season-en", "season-de"]
                .into_iter()
                .find(|name| line.contains("/img/") && line.contains(name))
        })
        .collect();
    assert_eq!(
        posters,
        ["season-fr", "season-en", "season-de"],
        "preferred language, then English, then the rest — not TMDB's order"
    );
}

/// TheMovieDb first, with no overview for the season: TheTVDB's translated
/// overview fills it; TheMovieDb's air date stands.
#[tokio::test(flavor = "multi_thread")]
async fn a_season_tmdb_has_no_overview_for_takes_tvdbs() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), season_order(&[TMDB, TVDB])).await;
    let (base, _log) = spawn_providers(Answers {
        season: TMDB_SEASON_NO_OVERVIEW,
        ..DEFAULT_ANSWERS
    });

    library.scanner(&base).scan_all().await.expect("scan");
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("TVDB season."));
    assert_eq!(season.production_year, Some(2011));
}

/// A series TheMovieDb never matched has no Tmdb id: TheMovieDb's season
/// provider asks nothing (`TmdbSeasonProvider.cs:53-56`: no series id, no
/// request — it never searches), and TheTVDB, which found the series by
/// name, supplies the season.
#[tokio::test(flavor = "multi_thread")]
async fn a_season_under_a_series_tmdb_never_matched_asks_tmdb_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    let (base, log) = spawn_providers(Answers {
        search: TMDB_NOT_FOUND,
        ..DEFAULT_ANSWERS
    });

    library.scanner(&base).scan_all().await.expect("scan");
    assert_eq!(asked(&log, "/tmdb/tv/"), 0, "{:?}", drain(&log));
    assert_eq!(asked(&log, "/tvdb/seasons/77/extended"), 1);
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("TVDB season."));
    assert_eq!(season.premiere_date, None, "TheTVDB supplies no date");
    assert_eq!(
        library.season_ids().await,
        [("Tvdb".to_owned(), "77".to_owned())]
    );
}

/// A locked season runs no season provider, even on what would be its first
/// refresh (`CanRefreshMetadata`: a locked item runs no remote provider), so
/// nothing is asked and nothing is written over.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_season_asks_its_providers_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    let (base, log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    library
        .set_season(r#""IsLocked" = 1, "Overview" = 'Mine.'"#)
        .await;
    library.forget_season_refresh().await;
    drain(&log);

    scanner.scan_all().await.expect("rescan");
    assert_eq!(drain(&log), Vec::<String>::new());
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("Mine."));
    assert!(season.is_locked);
}

/// An `Overview` field lock under "Replace all metadata": the season's
/// overview stands, while TheMovieDb's air date replaces an edited one
/// (`MergeData(temp, item, item.LockedFields, …)`).
#[tokio::test(flavor = "multi_thread")]
async fn a_seasons_locked_overview_stands_under_replace_all() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    library
        .set_season(r#""Overview" = 'Mine.', "ProductionYear" = 1999"#)
        .await;
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (?1, ?2)"#)
        .bind(ferrofin_db::enums::metadata_field::to_i32(
            MetadataField::Overview,
        ))
        .bind(ferrofin_db::store::guid_to_db(library.season_id))
        .execute(library.db.writer())
        .await
        .expect("lock the overview");

    scanner
        .scan_with(None, &dashboard("replace"))
        .await
        .expect("replace all");
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("Mine."), "locked");
    assert_eq!(season.production_year, Some(2011), "replaced");
}

/// The adoption gates' provider outage: every season provider request
/// fails (nothing listens), in each refresh that runs the season's
/// providers — a first refresh under "Scan for new and updated files",
/// "Search for missing metadata" and "Replace all metadata" (which skips
/// re-adding the stored values, `RemoveOldMetadata`). A season provider
/// that fails never clears what the season stores: its overview, dates,
/// provider ids and cast survive, and the refresh is not stamped, so the
/// next one asks again (`Failures > 0 && !hasRemoteMetadata`,
/// `MetadataService.cs:897-906`; D1).
#[rstest::rstest]
#[case::scan("scan")]
#[case::missing("missing")]
#[case::replace_all("replace")]
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_season_provider_never_clears_the_stored_season(#[case] choice: &str) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    library.scanner(&base).scan_all().await.expect("first scan");
    if choice == "scan" {
        library.forget_season_refresh().await;
    }
    let before = library.season().await;
    let ids = library.season_ids().await;
    let cast = library.cast().await;
    assert!(before.overview.is_some() && !ids.is_empty() && !cast.is_empty());

    library
        .scanner(&unreachable_base())
        .scan_with(None, &dashboard(choice))
        .await
        .expect("scan during the outage");

    let after = library.season().await;
    assert_eq!(after.overview, before.overview, "{choice}");
    assert_eq!(after.premiere_date, before.premiere_date, "{choice}");
    assert_eq!(after.production_year, before.production_year, "{choice}");
    assert_eq!(after.name, before.name, "{choice}");
    assert_eq!(library.season_ids().await, ids, "{choice}");
    assert_eq!(library.cast().await, cast, "{choice}");
    assert_eq!(
        after.date_last_refreshed, before.date_last_refreshed,
        "{choice}: a failed refresh is not stamped"
    );
}

/// TheTVDB unreachable while TheMovieDb answers: the season takes TMDB's
/// answer, and TheTVDB's failure counts as a provider failure, so the
/// season's refresh is not stamped and the next scan asks again (D1). For a
/// season this is the plugin's own behaviour: `TvdbSeasonProvider.
/// GetMetadata` has no try/catch, so its client's exception reaches
/// `ExecuteRemoteProviders`, which counts it (`MetadataService.cs:1017-1022`)
/// — unlike the series and episode providers, whose swallowed failures are
/// Ferrofin's accepted divergence.
#[tokio::test(flavor = "multi_thread")]
async fn a_season_tvdb_fails_for_takes_tmdbs_answer_unstamped() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);

    library
        .scanner_with(&base, &unreachable_base())
        .scan_all()
        .await
        .expect("scan");
    let season = library.season().await;
    assert_eq!(season.overview.as_deref(), Some("TMDB season."));
    assert_eq!(season.date_last_refreshed, None, "TheTVDB failed");
}

/// Both physical and virtual season zero names follow saved options on a quiet
/// scan. Name locks still preserve the user's edit on a later scan.
#[rstest::rstest]
#[case::physical(false)]
#[case::virtual_season(true)]
#[tokio::test]
async fn season_zero_uses_saved_name_and_keeps_name_locks(#[case] flat: bool) {
    let tmp = tempfile::tempdir().expect("tmp");
    let mut library = Library::new(
        tmp.path(),
        LibraryOptions {
            season_zero_display_name: "Bonus".to_owned(),
            type_options: ["Series", "Season", "Episode"]
                .into_iter()
                .map(|kind| TypeOptions {
                    type_: Some(kind.to_owned()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
    )
    .await;
    let show = library.season_dir.parent().unwrap().to_owned();
    std::fs::remove_dir_all(&library.season_dir).expect("remove fixture season");
    library.season_dir = if flat {
        show.clone()
    } else {
        show.join("Season 0")
    };
    std::fs::create_dir_all(&library.season_dir).expect("specials folder");
    std::fs::write(library.season_dir.join("Show S00E01.mkv"), b"fixture").expect("episode");
    let scanner = library.scanner("http://127.0.0.1:9");
    scanner.scan_all().await.expect("initial scan");
    // A season with no folder is keyed by its series, number and name
    // (`CreateSeasonAsync`), and keeps that id through a rename.
    library.season_id = if flat {
        let id: String = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%.Season' AND "IndexNumber" = 0"#,
        )
        .fetch_one(library.db.pool())
        .await
        .expect("the virtual season");
        uuid::Uuid::parse_str(&id).expect("id")
    } else {
        derive_item_id(BaseItemKind::Season, &library.season_dir.to_string_lossy()).unwrap()
    };
    assert_eq!(library.season().await.name.as_deref(), Some("Bonus"));

    let folder = library.vf.get_virtual_folders().await.unwrap().remove(0);
    let mut options = folder.library_options.unwrap();
    options.season_zero_display_name = "Extras".to_owned();
    library
        .vf
        .update_library_options("TV", &options)
        .await
        .unwrap();
    scanner.scan_all().await.expect("quiet rescan");
    assert_eq!(library.season().await.name.as_deref(), Some("Extras"));
    options.season_zero_display_name = "EXTRAS".to_owned();
    library
        .vf
        .update_library_options("TV", &options)
        .await
        .unwrap();
    scanner.scan_all().await.expect("case-only rescan");
    assert_eq!(library.season().await.name.as_deref(), Some("Extras"));

    library.set_season(r#""Name" = 'Mine'"#).await;
    sqlx::query(r#"INSERT INTO "BaseItemMetadataFields" ("Id", "ItemId") VALUES (?1, ?2)"#)
        .bind(ferrofin_db::enums::metadata_field::to_i32(
            MetadataField::Name,
        ))
        .bind(ferrofin_db::store::guid_to_db(library.season_id))
        .execute(library.db.writer())
        .await
        .unwrap();
    scanner.scan_all().await.expect("locked rescan");
    assert_eq!(library.season().await.name.as_deref(), Some("Mine"));
    let episode = derive_item_id(
        BaseItemKind::Episode,
        &library.season_dir.join("Show S00E01.mkv").to_string_lossy(),
    )
    .unwrap();
    let episode = library.items.retrieve_item(episode).await.unwrap().unwrap();
    assert_eq!(
        episode.season_name.as_deref(),
        Some("Mine"),
        "children use the locked season name"
    );
}

/// The episode's NFO: aired before TheMovieDb's season air date, in another
/// year. A local reader runs before the remote providers, so the episode
/// keeps it whatever TheMovieDb's episode says.
const EARLY_EPISODE_NFO: &str = "<episodedetails><aired>2010-12-31</aired></episodedetails>";

impl Library {
    /// Gives the season's episode [`EARLY_EPISODE_NFO`].
    fn date_the_episode_early(&self) {
        std::fs::write(self.season_dir.join("Show S01E01.nfo"), EARLY_EPISODE_NFO)
            .expect("episode nfo");
    }

    /// Stamps the season as refreshed now: no longer its first refresh.
    async fn stamp_season_refresh(&self) {
        sqlx::query(r#"UPDATE "BaseItems" SET "DateLastRefreshed" = ?1 WHERE "Id" = ?2"#)
            .bind(ferrofin_db::store::datetime_to_db(chrono::Utc::now()))
            .bind(ferrofin_db::store::guid_to_db(self.season_id))
            .execute(self.db.writer())
            .await
            .expect("stamp the season");
    }

    /// The season's stored premiere day and production year.
    async fn season_date(&self) -> (Option<String>, Option<i64>) {
        let season = self.season().await;
        (
            season.premiere_date.map(|d| d.date_naive().to_string()),
            season.production_year,
        )
    }
}

/// TheMovieDb's season air date, as a stored premiere day and year.
fn tmdbs_date() -> (Option<String>, Option<i64>) {
    (Some("2011-04-17".to_owned()), Some(2011))
}

/// The early episode's date, as a stored premiere day and year.
fn episodes_date() -> (Option<String>, Option<i64>) {
    (Some("2010-12-31".to_owned()), Some(2011 - 1))
}

/// `SeasonMetadataService.EnableUpdatingPremiereDateFromChildren`
/// (`SeasonMetadataService.cs:46`): a full refresh of a season takes its
/// premiere date and year from its earliest episode in `BeforeSave`, before
/// the providers run (`MetadataService.cs:150,462-464`). "Search for missing
/// metadata" only fills, so the episode's date stands over TheMovieDb's;
/// "Replace all metadata" lets TheMovieDb's air date replace it; "Scan for new
/// and updated files" is no full refresh, so the season keeps what it has.
/// The first scan refreshes the season before its episode exists
/// (`Series.RefreshAllMetadataInternal` refreshes the seasons first), so the
/// season starts with TheMovieDb's date; it is stamped as refreshed, as a
/// clean first scan leaves it (the stand-in's image 404s would not).
#[rstest::rstest]
#[case::missing("missing", episodes_date())]
#[case::replace_all("replace", tmdbs_date())]
#[case::scan("scan", tmdbs_date())]
#[tokio::test(flavor = "multi_thread")]
async fn a_full_refresh_dates_a_season_from_its_earliest_episode(
    #[case] choice: &str,
    #[case] expected: (Option<String>, Option<i64>),
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    library.date_the_episode_early();
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    assert_eq!(library.season_date().await, tmdbs_date(), "first scan");
    library.stamp_season_refresh().await;

    scanner
        .scan_with(None, &dashboard(choice))
        .await
        .expect("refresh");
    assert_eq!(library.season_date().await, expected, "{choice}");
}

/// A library whose seasons run TheTVDB alone, which answers with no season
/// date: the season has none after its first scan, and "Search for missing
/// metadata" dates it from its episode.
#[tokio::test(flavor = "multi_thread")]
async fn a_tvdb_only_season_takes_its_episodes_date() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), season_order(&[TVDB])).await;
    library.date_the_episode_early();
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    assert_eq!(library.season_date().await, (None, None), "first scan");

    scanner
        .scan_with(None, &dashboard("missing"))
        .await
        .expect("search for missing metadata");
    assert_eq!(library.season_date().await, episodes_date());
}

/// `UpdateMetadataFromChildren` returns on `item.IsLocked`
/// (`MetadataService.cs:457-460`): a locked season keeps its date.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_season_keeps_its_date_on_a_full_refresh() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    library.date_the_episode_early();
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    library.set_season(r#""IsLocked" = 1"#).await;

    scanner
        .scan_with(None, &dashboard("missing"))
        .await
        .expect("search for missing metadata");
    assert_eq!(library.season_date().await, tmdbs_date());
}

/// The other half of the gate, `updateType > None`: a new local season
/// poster is an image update before `BeforeSave` (`ValidateImages`,
/// `MetadataService.cs:123-143`), so even a "Scan for new and updated
/// files" of a season refreshed before dates it from its episode.
///
/// The stand-in's image files all 404, which leaves the season's first
/// refresh unstamped (so every scan would be another first refresh); the
/// test stamps it, as a clean first scan would.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_season_poster_dates_the_season_from_its_episode() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    library.date_the_episode_early();
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    library.stamp_season_refresh().await;
    scanner.scan_all().await.expect("quiet rescan");
    assert_eq!(library.season_date().await, tmdbs_date(), "no image change");

    std::fs::write(library.season_dir.join("poster.jpg"), b"jpeg").expect("poster");
    scanner.scan_all().await.expect("rescan");
    assert_eq!(library.season_date().await, episodes_date());
}

/// A season NFO that changed in the same scan as the new poster: upstream
/// derives the date onto the season before its providers run, and the NFO's
/// `<premiered>` then replaces it (`MergeBaseItemData`), so the NFO's date
/// stands.
#[tokio::test(flavor = "multi_thread")]
async fn a_season_nfos_date_stands_over_its_episodes_on_an_image_update() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    library.date_the_episode_early();
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library.scanner(&base);
    scanner.scan_all().await.expect("first scan");
    library.stamp_season_refresh().await;
    // Saved well before the NFO is written, so its change monitor fires
    // (`BaseNfoProvider.HasChanged` allows a few seconds of save tolerance).
    library
        .set_season(r#""DateLastSaved" = '2020-01-01 00:00:00.0000000'"#)
        .await;

    std::fs::write(
        library.season_dir.join("season.nfo"),
        "<season><premiered>2008-03-02</premiered></season>",
    )
    .expect("season nfo");
    std::fs::write(library.season_dir.join("poster.jpg"), b"jpeg").expect("poster");
    scanner.scan_all().await.expect("rescan");
    assert_eq!(
        library.season_date().await,
        (Some("2008-03-02".to_owned()), Some(2008))
    );
}

/// `Season.GetEpisodes` under "Display specials within seasons": a special
/// that airs before season 1 is one of its episodes, so with the setting on
/// its earlier date dates season 1; with it off, season 1's own episode does
/// (`Series.FilterEpisodesBySeason`).
#[rstest::rstest]
#[case::specials_within(true, (Some("2009-06-01".to_owned()), Some(2009)))]
#[case::specials_apart(false, episodes_date())]
#[tokio::test(flavor = "multi_thread")]
async fn a_special_airing_within_a_season_dates_it_when_specials_show_there(
    #[case] within: bool,
    #[case] expected: (Option<String>, Option<i64>),
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path(), LibraryOptions::default()).await;
    library.date_the_episode_early();
    let specials = library.season_dir.parent().expect("show").join("Season 0");
    std::fs::create_dir_all(&specials).expect("specials folder");
    std::fs::write(specials.join("Show S00E01.mkv"), b"special").expect("special");
    std::fs::write(
        specials.join("Show S00E01.nfo"),
        "<episodedetails><aired>2009-06-01</aired>\
         <airsbefore_season>1</airsbefore_season></episodedetails>",
    )
    .expect("special nfo");
    let (base, _log) = spawn_providers(DEFAULT_ANSWERS);
    let scanner = library
        .scanner(&base)
        .with_display_specials_within_seasons(move || within);
    scanner.scan_all().await.expect("first scan");

    scanner
        .scan_with(None, &dashboard("missing"))
        .await
        .expect("search for missing metadata");
    assert_eq!(library.season_date().await, expected);
}
