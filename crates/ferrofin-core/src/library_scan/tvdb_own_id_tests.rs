//! A season or an episode with a TheTVDB id of its own is looked up by it
//! (jellyfin-plugin-tvdb `v24`, `228f7b3`), its metadata and its artwork,
//! with no request through its series; one with no id of its own still
//! resolves through the series by its number; and an id `Convert.ToInt32`
//! cannot read is a failure with no request.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_providers::rate_limit::count_request_failures;

use super::{ArtworkCache, LibraryScanner, ResolverGuesses};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// TheTVDB's recorded answers: series 5 lists `official` season 1 = 51,
/// whose episode 1 is 91; season 77 and episode 88 are what a season and an
/// episode of their own ids point at.
const ROUTES: &[(&str, &str)] = &[
    ("/login", r#"{"data":{"token":"tok"}}"#),
    (
        "/artwork/types",
        r#"{"data":[{"id":7,"name":"Poster","recordType":"season"}]}"#,
    ),
    (
        "/series/5/extended",
        r#"{"data":{"seasons":[{"id":51,"number":1,"type":{"type":"official"}}]}}"#,
    ),
    (
        "/seasons/51/extended",
        r#"{"data":{"id":51,"translations":{"overviewTranslations":[
            {"language":"eng","overview":"By the series."}]}}}"#,
    ),
    (
        "/seasons/77/extended",
        r#"{"data":{"id":77,"artwork":[{"type":7,"image":"https://img.invalid/own-season.jpg"}],
            "translations":{"overviewTranslations":[
            {"language":"eng","overview":"The season's own."}]}}}"#,
    ),
    (
        "/series/5/episodes/official",
        r#"{"data":{"episodes":[{"id":91,"seasonNumber":1,"number":1}]}}"#,
    ),
    (
        "/episodes/91/extended",
        r#"{"data":{"id":91,"name":"By number"}}"#,
    ),
    (
        "/episodes/88/extended",
        r#"{"data":{"id":88,"name":"Own","airsBeforeSeason":2,
            "image":"https://img.invalid/own-episode.jpg"}}"#,
    ),
];

/// A TheTVDB stand-in serving [`ROUTES`] (404 for anything else), with the
/// request line of each request it had.
fn tvdb_stand_in() -> (String, Arc<Mutex<Vec<String>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
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
            let (status, body) = ROUTES
                .iter()
                .find(|(route, _)| {
                    line.contains(&format!("{route}?")) || line.contains(&format!("{route} "))
                })
                .map_or(("404 Not Found", "{}"), |(_, body)| ("200 OK", *body));
            log.lock().unwrap().push(line);
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{addr}"), requests)
}

/// A scanner with TheTVDB at `base` and no item repository (every series'
/// display order reads as none), and a cache recording series 5 as the
/// series `SERIES`'s TheTVDB id.
async fn scanner(base: &str) -> (LibraryScanner, ArtworkCache) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let vf = Arc::new(FerrofinVirtualFolderManager::new(tmp.path().join("views")));
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db));
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_tvdb(Arc::new(
            ferrofin_providers::TvdbClient::new().with_base_url(base),
        ));
    let mut cache = ArtworkCache::default();
    cache.item_provider_ids.insert(
        "SERIES".to_owned(),
        vec![("Tvdb".to_owned(), "5".to_owned())],
    );
    (scanner, cache)
}

/// Season or episode 1 of season 1 of the series `SERIES`.
fn child(kind: &str) -> BaseItemEntity {
    BaseItemEntity {
        id: kind.to_uppercase(),
        type_: format!("MediaBrowser.Controller.Entities.TV.{kind}"),
        series_id: Some("SERIES".to_owned()),
        parent_index_number: Some(1),
        index_number: Some(1),
        ..Default::default()
    }
}

fn tvdb_id(id: &str) -> Vec<(String, String)> {
    vec![("Tvdb".to_owned(), id.to_owned())]
}

#[rstest::rstest]
#[case::own_id(tvdb_id("77"), "The season's own.", false)]
#[case::padded_own_id(tvdb_id(" 77 "), "The season's own.", false)]
#[case::through_the_series(Vec::new(), "By the series.", true)]
#[tokio::test]
async fn a_season_is_looked_up_by_its_own_tvdb_id_first(
    #[case] known: Vec<(String, String)>,
    #[case] overview: &str,
    #[case] asks_the_series: bool,
) {
    let (base, requests) = tvdb_stand_in();
    let (scanner, cache) = scanner(&base).await;
    let answer = scanner
        .fetch_tvdb_season(
            &child("Season"),
            &cache,
            &ResolverGuesses::default(),
            &known,
            "en",
        )
        .await
        .expect("an answer");
    assert_eq!(answer.item.overview.as_deref(), Some(overview));
    let asked = requests.lock().unwrap().clone();
    assert_eq!(
        asked.iter().any(|l| l.contains("/series/")),
        asks_the_series,
        "{asked:?}"
    );
}

/// By its own id a season needs neither a series nor a number.
#[tokio::test]
async fn a_season_with_its_own_tvdb_id_needs_no_series_or_number() {
    let (base, _requests) = tvdb_stand_in();
    let (scanner, cache) = scanner(&base).await;
    let season = BaseItemEntity {
        series_id: None,
        index_number: None,
        ..child("Season")
    };
    let answer = scanner
        .fetch_tvdb_season(
            &season,
            &cache,
            &ResolverGuesses::default(),
            &tvdb_id("77"),
            "en",
        )
        .await
        .expect("an answer");
    assert_eq!(answer.item.index_number, None);
    assert_eq!(answer.provider_ids, tvdb_id("77"));
}

#[rstest::rstest]
#[case::own_id(tvdb_id("88"), "88", false)]
#[case::through_the_series(Vec::new(), "91", true)]
#[tokio::test]
async fn an_episode_is_looked_up_by_its_own_tvdb_id_first(
    #[case] known: Vec<(String, String)>,
    #[case] fetched: &str,
    #[case] asks_the_series: bool,
) {
    let (base, requests) = tvdb_stand_in();
    let (scanner, mut cache) = scanner(&base).await;
    let answer = scanner
        .fetch_tvdb_metadata(
            &child("Episode"),
            "Episode",
            &mut cache,
            &known,
            &ResolverGuesses::default(),
        )
        .await
        .expect("an answer");
    assert!(
        answer
            .provider_ids
            .contains(&("Tvdb".to_owned(), fetched.to_owned())),
        "{:?}",
        answer.provider_ids
    );
    let asked = requests.lock().unwrap().clone();
    assert_eq!(
        asked.iter().any(|l| l.contains("/series/")),
        asks_the_series,
        "{asked:?}"
    );
}

/// A series whose row cannot be read is no series to the plugin: an episode
/// with its own id is still fetched by it, its specials' placement taken as
/// in aired order, and one without is skipped with nothing asked.
#[tokio::test]
async fn an_unread_series_skips_only_the_episodes_without_their_own_tvdb_id() {
    let (base, requests) = tvdb_stand_in();
    let (scanner, mut cache) = scanner(&base).await;
    cache.unread_parents.insert("SERIES".to_owned());
    let lookup = ResolverGuesses::default();
    let skipped = scanner
        .fetch_tvdb_metadata(&child("Episode"), "Episode", &mut cache, &[], &lookup)
        .await;
    assert!(skipped.is_none());
    let asked = requests.lock().unwrap().clone();
    assert!(asked.is_empty(), "{asked:?}");
    let answer = scanner
        .fetch_tvdb_metadata(
            &child("Episode"),
            "Episode",
            &mut cache,
            &tvdb_id("88"),
            &lookup,
        )
        .await
        .expect("an answer by its own id");
    assert!(
        answer
            .item
            .data
            .as_deref()
            .is_some_and(|data| data.contains("AirsBeforeSeasonNumber")),
        "{:?}",
        answer.item.data
    );
}

/// The image pass reads the item's ids as the pass settled them: by its
/// own id a season's art and an episode's still come with no request
/// through the series.
#[tokio::test]
async fn season_and_episode_art_is_looked_up_by_the_items_own_tvdb_id() {
    let (base, requests) = tvdb_stand_in();
    let (scanner, mut cache) = scanner(&base).await;
    for (kind, id, url) in [
        ("Season", "77", "https://img.invalid/own-season.jpg"),
        ("Episode", "88", "https://img.invalid/own-episode.jpg"),
    ] {
        let item = child(kind);
        cache.item_provider_ids.insert(item.id.clone(), tvdb_id(id));
        let images = scanner.tvdb_child_images(&item, &cache).await;
        let urls: Vec<&str> = images.iter().map(|image| image.url.as_str()).collect();
        assert_eq!(urls, [url], "{kind}");
    }
    let asked = requests.lock().unwrap().clone();
    assert!(!asked.iter().any(|l| l.contains("/series/")), "{asked:?}");
}

/// An own id `Convert.ToInt32` cannot read throws in each of the plugin's
/// providers before anything is asked: no request, one failure.
#[tokio::test]
async fn an_unreadable_own_tvdb_id_asks_nothing_and_fails() {
    let (base, requests) = tvdb_stand_in();
    let (scanner, mut cache) = scanner(&base).await;
    let lookup = ResolverGuesses::default();
    let unreadable = tvdb_id("x77");
    let (season, failures) = count_request_failures(scanner.fetch_tvdb_season(
        &child("Season"),
        &cache,
        &lookup,
        &unreadable,
        "en",
    ))
    .await;
    assert!(season.is_none());
    assert_eq!(failures, 1, "the season");
    let (episode, failures) = count_request_failures(scanner.fetch_tvdb_metadata(
        &child("Episode"),
        "Episode",
        &mut cache,
        &unreadable,
        &lookup,
    ))
    .await;
    assert!(episode.is_none());
    assert_eq!(failures, 1, "the episode");
    let asked = requests.lock().unwrap().clone();
    assert!(asked.is_empty(), "{asked:?}");
}
