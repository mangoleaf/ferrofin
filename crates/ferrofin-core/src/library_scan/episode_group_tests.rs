//! The scan's TheMovieDb episode groups (gl-1784): each kept for the pass by
//! series, order and language, as `TmdbClientManager.GetSeriesGroupAsync`
//! keeps it (`TmdbClientManager.cs:182-226`), a miss and a failure included.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use ferrofin_db::Database;
use ferrofin_providers::rate_limit::count_request_failures;

use super::{ArtworkCache, LibraryScanner};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// A stand-in TheMovieDb listing series 1399's groups (an absolute one, and a
/// story-arc one whose body does not decode) and its aired seasons 2 and 3
/// (whose body does not decode either), logging every request path.
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
            let path = req
                .split_whitespace()
                .nth(1)
                .and_then(|target| target.split('?').next())
                .unwrap_or_default()
                .to_owned();
            let body = match path.as_str() {
                "/tv/1399/episode_groups" => {
                    r#"{"results": [{"id": "abs1", "type": 2}, {"id": "arc1", "type": 5}]}"#
                }
                "/tv/episode_group/abs1" => {
                    r#"{"groups": [{"order": 1, "episodes": [
                        {"order": 0, "season_number": 2, "episode_number": 5},
                        {"order": 1, "season_number": 3, "episode_number": 1}]}]}"#
                }
                "/tv/episode_group/arc1" => r#"{"groups": "not a list"}"#,
                "/tv/1399/season/2" => {
                    r#"{"episodes": [{"id": 205, "episode_number": 5, "name": "Two Five"}]}"#
                }
                "/tv/1399/season/3" => r#"{"episodes": "not a list"}"#,
                _ => "{}",
            };
            requests.lock().expect("log").push(path);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{addr}"), log)
}

/// A scanner whose TheMovieDb is `base`.
async fn scanner(base: &str) -> LibraryScanner {
    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrations");
    let tmp = tempfile::tempdir().expect("tmp");
    let vf = Arc::new(FerrofinVirtualFolderManager::new(tmp.path().join("views")));
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db));
    LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence).with_metadata(
        Arc::new(ferrofin_providers::TmdbClient::new().with_base_url(base)),
        tmp.path().join("metadata"),
    )
}

/// How many times `path` was asked.
fn asked(log: &Mutex<Vec<String>>, path: &str) -> usize {
    log.lock()
        .expect("log")
        .iter()
        .filter(|line| *line == path)
        .count()
}

/// A group is asked once per series, order and language, and every later
/// reader of the pass maps through the kept one; an order with no group type
/// asks nothing.
#[tokio::test]
async fn a_group_is_asked_once_per_series_order_and_language() {
    let (base, log) = spawn_tmdb();
    let scanner = scanner(&base).await;
    let mut cache = ArtworkCache::default();

    for _ in 0..2 {
        let group = scanner
            .episode_group_cached(&mut cache, "SERIES", 1399, "absolute", Some("en-US"))
            .await
            .expect("answered")
            .expect("a group");
        assert_eq!(group.aired_numbers(1, 1), Some((2, 5)));
    }
    assert_eq!(asked(&log, "/tv/1399/episode_groups"), 1);
    assert_eq!(asked(&log, "/tv/episode_group/abs1"), 1);

    scanner
        .episode_group_cached(&mut cache, "SERIES", 1399, "absolute", None)
        .await
        .expect("answered");
    assert_eq!(asked(&log, "/tv/episode_group/abs1"), 2, "another language");

    for _ in 0..2 {
        assert_eq!(
            scanner
                .episode_group_cached(&mut cache, "SERIES", 1399, "dvd", None)
                .await,
            Ok(None),
            "TMDb lists no DVD group"
        );
    }
    assert_eq!(
        asked(&log, "/tv/1399/episode_groups"),
        3,
        "the miss is kept"
    );

    assert_eq!(
        scanner
            .episode_group_cached(&mut cache, "SERIES", 1399, "aired", None)
            .await,
        Ok(None)
    );
    assert_eq!(log.lock().expect("log").len(), 5, "aired asks nothing");
}

/// A failed group request is kept for the pass, and every reader counts it
/// once, so none of the series' episodes is stamped as refreshed.
#[tokio::test]
async fn a_failed_group_is_asked_once_and_counted_by_each_reader() {
    let (base, log) = spawn_tmdb();
    let scanner = scanner(&base).await;
    let mut cache = ArtworkCache::default();

    for reader in 0..2 {
        let (group, failures) = count_request_failures(Box::pin(
            scanner.episode_group_cached(&mut cache, "SERIES", 1399, "storyArc", None),
        ))
        .await;
        assert!(group.is_err(), "reader {reader}");
        assert_eq!(failures, 1, "reader {reader}");
    }
    assert_eq!(asked(&log, "/tv/episode_group/arc1"), 1);
}

/// A group the cache has no room for is not kept, but it still answers the
/// reader that asked.
#[tokio::test]
async fn a_group_the_cache_has_no_room_for_still_answers() {
    let (base, log) = spawn_tmdb();
    let scanner = scanner(&base).await;
    let mut cache = ArtworkCache {
        episode_groups: super::ProviderCache::bounded(
            super::TMDB_CACHE_DURATION,
            0,
            super::GroupLookup::estimate_size,
        ),
        ..ArtworkCache::default()
    };

    for _ in 0..2 {
        assert!(
            scanner
                .episode_group_cached(&mut cache, "SERIES", 1399, "absolute", None)
                .await
                .expect("answered")
                .is_some()
        );
    }
    assert_eq!(asked(&log, "/tv/episode_group/abs1"), 2, "not kept");
}

/// A group is kept for `TmdbClientManager`'s hour and asked again after it.
#[tokio::test]
async fn a_group_is_asked_again_once_the_hour_has_passed() {
    let (base, log) = spawn_tmdb();
    let scanner = scanner(&base).await;
    let mut cache = ArtworkCache::default();
    let read = async |cache: &mut ArtworkCache| {
        scanner
            .episode_group_cached(cache, "SERIES", 1399, "absolute", None)
            .await
            .expect("answered")
            .expect("a group");
    };

    read(&mut cache).await;
    cache
        .episode_groups
        .pass(super::TMDB_CACHE_DURATION.saturating_sub(std::time::Duration::from_secs(1)));
    read(&mut cache).await;
    assert_eq!(asked(&log, "/tv/episode_group/abs1"), 1, "within the hour");
    cache.episode_groups.pass(std::time::Duration::from_secs(1));
    read(&mut cache).await;
    assert_eq!(asked(&log, "/tv/episode_group/abs1"), 2, "after it");
}

/// `GetEpisodeAsync` throws when a request for any number of a range fails,
/// so a range in a group's order whose second episode's aired season fails
/// gets no answer, never the first episode's alone, and the failure is
/// counted once. The first episode on its own is still answered.
#[tokio::test]
async fn a_range_with_a_failed_aired_season_gets_no_answer() {
    use ferrofin_db::entities::base_items::BaseItemEntity;
    let (base, log) = spawn_tmdb();
    let scanner = scanner(&base).await;
    let mut cache = ArtworkCache::default();
    cache.series_tmdb.insert("SERIES".to_owned(), 1399);
    cache
        .display_orders
        .insert("SERIES".to_owned(), "absolute".to_owned());
    let episode = BaseItemEntity {
        type_: "MediaBrowser.Controller.Entities.TV.Episode".to_owned(),
        series_id: Some("SERIES".to_owned()),
        parent_index_number: Some(1),
        index_number: Some(1),
        ..BaseItemEntity::default()
    };
    let range = super::ResolverGuesses {
        index_number_end: Some(2),
        ..super::ResolverGuesses::default()
    };

    let (answer, failures) = count_request_failures(Box::pin(
        scanner.fetch_tmdb_episode(&episode, &mut cache, &range),
    ))
    .await;
    assert!(answer.is_none(), "{log:?}");
    assert_eq!(failures, 1);
    assert_eq!(asked(&log, "/tv/1399/season/3"), 1);

    let (answer, failures) = count_request_failures(Box::pin(scanner.fetch_tmdb_episode(
        &episode,
        &mut cache,
        &super::ResolverGuesses::default(),
    )))
    .await;
    let answer = answer.expect("the first episode alone");
    assert_eq!(answer.item.name.as_deref(), Some("Two Five"));
    assert_eq!(failures, 0);
    assert_eq!(asked(&log, "/tv/1399/season/2"), 1, "kept for the pass");
}

/// A refresh between two scan items that touched a series (an Identify that
/// swapped its TheMovieDb match, an edit of its display order) drops its
/// groups and its order with its seasons: they were asked under the old
/// ones. Another series keeps them.
#[tokio::test]
async fn a_refresh_that_touched_a_series_drops_its_groups_and_order() {
    let (base, _log) = spawn_tmdb();
    let scanner = scanner(&base).await;
    let (touched, other) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
    let key = |series: uuid::Uuid| {
        (
            ferrofin_db::store::guid_to_db(series),
            "absolute".to_owned(),
            None,
        )
    };
    let mut cache = ArtworkCache::default();
    for series in [touched, other] {
        assert!(
            cache
                .episode_groups
                .insert(key(series), super::GroupLookup::Missing)
                .is_none()
        );
        cache.display_orders.insert(
            ferrofin_db::store::guid_to_db(series),
            "absolute".to_owned(),
        );
    }

    scanner
        .reread_touched(
            &[touched],
            &mut cache,
            &mut std::collections::HashMap::new(),
        )
        .await;

    assert!(cache.episode_groups.get(&key(touched)).is_none());
    assert!(cache.episode_groups.get(&key(other)).is_some());
    assert!(
        !cache
            .display_orders
            .contains_key(&ferrofin_db::store::guid_to_db(touched))
    );
    assert!(
        cache
            .display_orders
            .contains_key(&ferrofin_db::store::guid_to_db(other))
    );
}
