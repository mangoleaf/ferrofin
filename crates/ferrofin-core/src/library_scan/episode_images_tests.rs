//! The next episode's TheMovieDb `/images` request runs during the current
//! episode's still downloads (gl-1275).
//!
//! The walk refreshes one item at a time, so a new episode used to wait for
//! its own `/tv/{id}/season/{n}/episode/{m}/images` request after the
//! previous episode's downloads had finished. Upstream refreshes a folder's
//! children concurrently; here the current episode's image pass starts the
//! next one's request through the same rate-limited client, and the next
//! pass takes that answer instead of asking again.

use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;

use super::LibraryScanner;
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// How long the first still download waits for the next episode's request
/// before answering anyway.
const HOLD: Duration = Duration::from_secs(5);

/// A fake TheMovieDb (API and image host) answering each connection on its
/// own thread, so a held download does not block other requests. It logs
/// every request's path; with `hold`, the first still download is held until a second
/// episode's `/images` request arrives (or [`HOLD`] passes), and the flag
/// says whether it arrived.
fn spawn_tmdb(hold: bool) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
    // Images are served on loopback, which downloads refuse by default.
    ferrofin_providers::destination::test_support::allow_loopback_images();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let overlapped = Arc::new(AtomicBool::new(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (server_log, server_flag) = (Arc::clone(&log), Arc::clone(&overlapped));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let (log, overlapped) = (Arc::clone(&server_log), Arc::clone(&server_flag));
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                // The path alone: the query carries the API key.
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = request
                    .split_whitespace()
                    .nth(1)
                    .and_then(|target| target.split('?').next())
                    .unwrap_or_default()
                    .to_owned();
                log.lock().expect("log").push(line.clone());
                let count = |what: &dyn Fn(&str) -> bool| {
                    log.lock().expect("log").iter().filter(|l| what(l)).count()
                };
                let images_asked = |l: &str| l.contains("/episode/") && l.contains("/images");
                let next_asked = || count(&images_asked) >= 2;
                let episode = (1..=3).find(|n| line.ends_with(&format!("/episode/{n}/images")));
                let (content_type, body) = if line.contains("/img/") {
                    if hold && count(&|l: &str| l.contains("-still")) == 1 {
                        let deadline = Instant::now() + HOLD;
                        while !next_asked() && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        overlapped.store(next_asked(), Ordering::SeqCst);
                    }
                    ("image/png", "GIF89aimage".to_owned())
                } else if let Some(n) = episode {
                    (
                        "application/json",
                        format!(r#"{{"stills":[{{"file_path":"/e{n}-still.png","width":1920}}]}}"#),
                    )
                } else if line.contains("/search/tv") {
                    (
                        "application/json",
                        r#"{"results": [{"id": 1399, "name": "GoT"}]}"#.to_owned(),
                    )
                } else if line == "/tv/1399/season/1" {
                    (
                        "application/json",
                        r#"{"name": "Season 1", "episodes": [
                            {"id": 1, "episode_number": 1, "name": "One"},
                            {"id": 2, "episode_number": 2, "name": "Two"},
                            {"id": 3, "episode_number": 3, "name": "Three"}]}"#
                            .to_owned(),
                    )
                } else if line == "/tv/1399" {
                    (
                        "application/json",
                        r#"{"id": 1399, "name": "GoT"}"#.to_owned(),
                    )
                } else {
                    ("application/json", "{}".to_owned())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (format!("http://{addr}"), log, overlapped)
}

/// Runs a first scan of a library of three new episodes (episode 2 with a
/// local still when `local_still`) against [`spawn_tmdb`], and returns the
/// request log and whether the held download saw the next request.
async fn first_scan(local_still: bool) -> (Vec<String>, bool) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tv = tmp.path().join("tv");
    std::fs::create_dir_all(tv.join("GoT/Season 01")).expect("season dir");
    for n in 1..=3 {
        std::fs::write(tv.join(format!("GoT/Season 01/GoT S01E0{n}.mkv")), b"").expect("episode");
    }
    if local_still {
        std::fs::write(tv.join("GoT/Season 01/GoT S01E02-thumb.jpg"), b"").expect("thumb");
    }
    let (base, log, overlapped) = spawn_tmdb(!local_still);

    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrations");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db));
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
    .expect("library");
    let tmdb = ferrofin_providers::TmdbClient::new()
        .with_base_url(&base)
        .with_image_root(&format!("{base}/img"));
    LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_metadata(Arc::new(tmdb), tmp.path().join("metadata"))
        .scan(None)
        .await
        .expect("scan");
    let log = log.lock().expect("log").clone();
    (log, overlapped.load(Ordering::SeqCst))
}

/// A first scan of three new episodes asks each one's `/images` once, and
/// the second episode's request arrives while the first one's still is
/// downloading: the first one's image pass started it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_episodes_images_request_runs_during_this_ones_downloads() {
    let (log, overlapped) = first_scan(false).await;
    for n in 1..=3 {
        let asked = log
            .iter()
            .filter(|l| l.contains(&format!("/episode/{n}/images")))
            .count();
        assert_eq!(asked, 1, "episode {n}'s /images, once: {log:#?}");
        assert!(
            log.iter().any(|l| l.contains(&format!("e{n}-still"))),
            "episode {n}'s still was downloaded: {log:#?}"
        );
    }
    assert!(
        overlapped,
        "the next episode's /images arrived during the first still download: {log:#?}"
    );
}

/// An episode whose local still fills its Primary runs no remote image step,
/// so nothing asks for its `/images` early either: no request is wasted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_episode_with_a_local_still_is_not_asked_for_early() {
    let (log, _) = first_scan(true).await;
    for (n, expected) in [(1, 1), (2, 0), (3, 1)] {
        let asked = log
            .iter()
            .filter(|l| l.contains(&format!("/episode/{n}/images")))
            .count();
        assert_eq!(asked, expected, "episode {n}'s /images: {log:#?}");
    }
}

/// Only the pass asking exactly the request started early takes its answer,
/// and with it the failed requests it counted, which keep that item's refresh
/// from being recorded as complete; any other pass asks for itself.
#[rstest::rstest]
#[case::its_own_request(2, 0, 2)]
#[case::another_episode(3, 1, 0)]
#[tokio::test]
async fn an_early_answer_goes_only_to_its_own_request(
    #[case] episode: i32,
    #[case] asked: usize,
    #[case] failures: u32,
) {
    let (base, log, _) = spawn_tmdb(false);
    let tmdb = ferrofin_providers::TmdbClient::new().with_base_url(&base);
    let mut cache = super::ArtworkCache::default();
    let started = tokio::spawn(async { (Vec::new(), 2) });
    cache.episode_images = Some((
        (1399, 1, 2, Some("en".to_owned())),
        super::AbortOnDrop(started),
    ));

    let key = (1399, 1, episode, Some("en".to_owned()));
    let (_, counted) = ferrofin_providers::rate_limit::count_request_failures(
        super::episode_images(&tmdb, &mut cache, &key),
    )
    .await;

    assert_eq!(counted, failures);
    let log = log.lock().expect("log").clone();
    assert_eq!(
        log.iter().filter(|l| l.ends_with("/images")).count(),
        asked,
        "{log:#?}"
    );
}
