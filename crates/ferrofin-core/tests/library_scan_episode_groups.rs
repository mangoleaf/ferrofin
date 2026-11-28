//! TheMovieDb's episode lookups in the series' display order, end to end
//! through a scan (gl-1784).
//!
//! `TmdbClientManager.GetEpisodeAsync` maps an episode's season and episode
//! numbers through the series' episode group of the type its `DisplayOrder`
//! names (`TmdbClientManager.cs:182-226`, `:286-297`), for the metadata
//! provider (`TmdbEpisodeProvider.cs:115,160`) and the image provider
//! (`TmdbEpisodeImageProvider.cs:76`). One series (`Show`, TMDB 1399) with
//! two episodes, S01E01 and S01E02, whose `tvshow.nfo` names the order,
//! against a stand-in for TheMovieDb that logs every request path.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::ItemRepository;

/// The stand-in's answer to request path `path`; `None` is a 404.
fn answer(path: &str) -> Option<&'static str> {
    Some(match path {
        "/tv/1399" => r#"{"id": 1399, "name": "Show", "overview": "A series."}"#,
        "/search/tv" => r#"{"results": [{"id": 1399, "name": "Show"}]}"#,
        // Two absolute groups (the first is the one asked), a story-arc one
        // and a TV one; no DVD group.
        "/tv/1399/episode_groups" => {
            r#"{"results": [{"id": "abs1", "type": 2}, {"id": "abs2", "type": 2},
                {"id": "arc1", "type": 5}, {"id": "tv1", "type": 7}]}"#
        }
        "/tv/episode_group/abs1" => {
            r#"{"id": "abs1", "type": 2, "groups": [{"order": 1, "episodes": [
                {"order": 0, "season_number": 2, "episode_number": 1},
                {"order": 1, "season_number": 2, "episode_number": 2}]}]}"#
        }
        "/tv/episode_group/abs2" => {
            r#"{"id": "abs2", "type": 2, "groups": [{"order": 1, "episodes": [
                {"order": 0, "season_number": 3, "episode_number": 1}]}]}"#
        }
        // Its first season 1 lists the first episode only, as aired S02E02;
        // a later season 1 is never read.
        "/tv/episode_group/arc1" => {
            r#"{"id": "arc1", "type": 5, "groups": [
                {"order": 2, "episodes": [{"order": 0, "season_number": 3, "episode_number": 1}]},
                {"order": 1, "episodes": [{"order": 0, "season_number": 2, "episode_number": 2}]},
                {"order": 1, "episodes": [{"order": 1, "season_number": 3, "episode_number": 1}]}]}"#
        }
        // A body that does not decode: a failed request, which leaves the
        // limiter's state alone (a failing status would cool it down and hide
        // what the scan asks after it).
        "/tv/episode_group/tv1" => r#"{"groups": "not a list"}"#,
        "/tv/1399/season/1" => {
            r#"{"id": 31, "name": "Season 1", "episodes": [
                {"id": 101, "episode_number": 1, "name": "One One"},
                {"id": 102, "episode_number": 2, "name": "One Two"}]}"#
        }
        "/tv/1399/season/2" => {
            r#"{"id": 32, "name": "Season 2", "episodes": [
                {"id": 201, "episode_number": 1, "name": "Two One"},
                {"id": 202, "episode_number": 2, "name": "Two Two"}]}"#
        }
        _ if path.ends_with("/images") => r#"{"stills": []}"#,
        _ if path.contains("/episode/") => r#"{"external_ids": {}}"#,
        _ => return None,
    })
}

/// Starts the stand-in; returns its base URL and its log of request paths
/// (no query: it carries the API key).
fn spawn_tmdb() -> (String, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::clone(&log);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req
                .split_whitespace()
                .nth(1)
                .and_then(|target| target.split('?').next())
                .unwrap_or_default()
                .to_owned();
            let (status, body) = answer(&path).map_or(("404 Not Found", "{}"), |b| ("200 OK", b));
            requests.lock().expect("log").push(path);
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{addr}"), log)
}

/// A first scan of `tv/Show/Season 1/Show S01E0{1,2}.mkv` whose
/// `tvshow.nfo` names `display_order` (none when empty): the two episodes'
/// stored names, and every path TheMovieDb was asked.
async fn first_scan(tmp: &Path, display_order: &str) -> ([Option<String>; 2], Vec<String>) {
    let tv = tmp.join("tv");
    let season_dir = tv.join("Show").join("Season 1");
    std::fs::create_dir_all(&season_dir).expect("mkdir");
    let order = if display_order.is_empty() {
        String::new()
    } else {
        format!("<displayorder>{display_order}</displayorder>")
    };
    std::fs::write(
        tv.join("Show/tvshow.nfo"),
        format!("<tvshow><title>Show</title>{order}</tvshow>"),
    )
    .expect("tvshow.nfo");
    let files = ["Show S01E01.mkv", "Show S01E02.mkv"].map(|name| season_dir.join(name));
    for file in &files {
        std::fs::write(file, b"0123456789").expect("episode");
    }
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("views")).with_item_store(persistence.clone()),
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
    let (base, log) = spawn_tmdb();
    LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::clone(&items))
        .with_metadata(
            Arc::new(TmdbClient::new().with_base_url(&base)),
            tmp.join("metadata"),
        )
        .scan_all()
        .await
        .expect("scan");
    let mut names = [None, None];
    for (name, file) in names.iter_mut().zip(&files) {
        let id = derive_item_id(BaseItemKind::Episode, &file.to_string_lossy()).expect("id");
        *name = items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("the episode")
            .name;
    }
    let log = log.lock().expect("log").clone();
    (names, log)
}

/// How many times `path` was asked.
fn asked(log: &[String], path: &str) -> usize {
    log.iter().filter(|line| *line == path).count()
}

/// An absolute-ordered series' episodes are TheMovieDb's aired S02E01 and
/// S02E02, which the first absolute group it lists maps them to: their
/// names, their own requests and their stills. The groups are asked once for
/// the whole series per language (the metadata provider's, and the image
/// provider's none), never once per episode.
#[tokio::test(flavor = "multi_thread")]
async fn an_absolute_ordered_series_is_looked_up_through_its_episode_group() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (names, log) = first_scan(tmp.path(), "absolute").await;

    assert_eq!(
        names,
        [Some("Two One".to_owned()), Some("Two Two".to_owned())],
        "{log:#?}"
    );
    for episode in [1, 2] {
        assert_eq!(
            asked(&log, &format!("/tv/1399/season/2/episode/{episode}")),
            1,
            "{log:#?}"
        );
        assert_eq!(
            asked(&log, &format!("/tv/1399/season/2/episode/{episode}/images")),
            1,
            "{log:#?}"
        );
    }
    assert!(
        !log.iter()
            .any(|line| line.starts_with("/tv/1399/season/1/episode/")),
        "nothing in aired order: {log:#?}"
    );
    assert_eq!(asked(&log, "/tv/1399/episode_groups"), 2, "{log:#?}");
    assert_eq!(asked(&log, "/tv/episode_group/abs1"), 2, "{log:#?}");
    assert_eq!(asked(&log, "/tv/episode_group/abs2"), 0, "{log:#?}");
}

/// The fallbacks: an episode its group does not list keeps its own numbers
/// (story arc: S01E01 is listed as S02E02, S01E02 is not), and so does every
/// episode of an order TheMovieDb lists no group of (DVD). An order TMDb has
/// no group type for (aired, or none) asks for no group at all.
#[rstest::rstest]
#[case::unlisted_episode("storyArc", ["Two Two", "One Two"], 2)]
#[case::no_group_of_the_type("dvd", ["One One", "One Two"], 2)]
#[case::aired("aired", ["One One", "One Two"], 0)]
#[case::none("", ["One One", "One Two"], 0)]
#[tokio::test(flavor = "multi_thread")]
async fn an_episode_no_group_maps_keeps_its_own_numbers(
    #[case] display_order: &str,
    #[case] expected: [&str; 2],
    #[case] group_lists: usize,
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let (names, log) = first_scan(tmp.path(), display_order).await;

    assert_eq!(
        names,
        expected.map(|name| Some(name.to_owned())),
        "{log:#?}"
    );
    assert_eq!(
        asked(&log, "/tv/1399/episode_groups"),
        group_lists,
        "{log:#?}"
    );
}

/// A group request that fails (here its body does not decode; not a 404) is
/// TMDbLib's exception out of `GetEpisodeAsync`: the episode gets no
/// TheMovieDb answer and no stills, never the aired-order episode's.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_group_request_answers_nothing_in_aired_order() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (names, log) = first_scan(tmp.path(), "tv").await;

    assert!(
        !log.iter().any(|line| line.contains("/episode/")),
        "no episode asked: {log:#?}"
    );
    for name in names {
        assert_ne!(name.as_deref(), Some("One One"), "{log:#?}");
        assert_ne!(name.as_deref(), Some("One Two"), "{log:#?}");
    }
}
