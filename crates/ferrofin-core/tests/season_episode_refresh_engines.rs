//! The two refresh engines over the same season and episode (gl-105).
//!
//! A file-backed season or episode refreshes through the library scan; a
//! path-less one (a virtual season, an episode with a remote path) through
//! `LocalProviderManager::refresh_item`. Both port upstream's
//! `MetadataService` pipeline separately, so these tests run each engine on
//! the same (file-backed) rows against the same TheMovieDb answers and pin
//! what each stores. Where the engines agree the test asserts one value;
//! where they have drifted it pins each side's, so a change to either engine
//! — or the unification — shows here. Only TheMovieDb's metadata fold is
//! covered: the refreshes under test leave images alone, the stand-in lists
//! no artwork, and TheTVDB and OMDb are not wired.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_providers::{LocalProviderManager, TmdbClient};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions, ProviderManager};
use uuid::Uuid;

/// `/tv/1399/season/1`: the season's own air date is a week before its first
/// episode's.
const SEASON_JSON: &str = r#"{
    "id": 3624, "name": "Series One", "overview": "The first season.",
    "air_date": "2011-04-10",
    "episodes": [
        {"id": 63056, "episode_number": 1, "name": "Winter Is Coming",
         "overview": "Ned is summoned south.", "air_date": "2011-04-17",
         "vote_average": 8.5}
    ]
}"#;

/// `/tv/1399/season/1/episode/1?append_to_response=…`: the episode's ids and
/// one trailer listed twice.
const EPISODE_JSON: &str = r#"{
    "external_ids": {"imdb_id": "tt1480055", "tvdb_id": 3254641},
    "credits": {"cast": [], "guest_stars": [], "crew": []},
    "videos": {"results": [
        {"site": "YouTube", "type": "Trailer", "key": "k1", "name": "Trailer"},
        {"site": "YouTube", "type": "Trailer", "key": "k1", "name": "Trailer again"}
    ]}
}"#;

/// A TheMovieDb stand-in for one series (`1399`), its season 1 and that
/// season's episode 1, with no artwork.
fn spawn_tmdb() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default();
            let (status, payload) = if line.contains("/images") {
                (
                    "200 OK",
                    r#"{"posters":[],"backdrops":[],"stills":[],"logos":[]}"#,
                )
            } else if line.contains("/tv/1399/season/1/episode/1") {
                ("200 OK", EPISODE_JSON)
            } else if line.contains("/tv/1399/season/1") {
                ("200 OK", SEASON_JSON)
            } else if line.contains("/search/tv") {
                ("200 OK", r#"{"results": [{"id": 1399, "name": "Show"}]}"#)
            } else if line.contains("/tv/1399") {
                ("200 OK", r#"{"id": 1399, "name": "Show", "genres": []}"#)
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
    format!("http://{addr}")
}

/// What each engine stored for one item: the columns a TheMovieDb answer
/// sets, the provider ids and the remote trailers' URLs.
#[derive(Debug, PartialEq)]
struct Stored {
    name: Option<String>,
    overview: Option<String>,
    premiere_date: Option<String>,
    production_year: Option<i64>,
    community_rating: Option<f64>,
    provider_ids: Vec<(String, String)>,
    trailers: Vec<String>,
}

/// A scanned TV library — `Show/Season 1/Show - S01E01.mkv` — and the
/// provider manager over the same stores and the same TheMovieDb.
struct Engines {
    _tmp: tempfile::TempDir,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    providers: LocalProviderManager,
    season: Uuid,
    episode: Uuid,
}

impl Engines {
    /// The library scanned, then refreshed whole with `options`.
    async fn scanned(options: &MetadataRefreshOptions) -> Self {
        let base = spawn_tmdb();
        let tmp = tempfile::tempdir().expect("tmp");
        let tv = tmp.path().join("tv");
        let season_dir = tv.join("Show").join("Season 1");
        std::fs::create_dir_all(&season_dir).expect("mkdir");
        let file = season_dir.join("Show - S01E01.mkv");
        std::fs::write(&file, b"").expect("write");
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
        let tmdb = Arc::new(TmdbClient::new().with_base_url(&base));
        let metadata = tmp.path().join("metadata");
        let scanner = LibraryScanner::new(
            Arc::clone(&vf),
            Arc::new(FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_metadata(Arc::clone(&tmdb), metadata.clone())
        .with_items(Arc::clone(&items));
        assert_eq!(scanner.scan_all().await.expect("scan").created, 3);
        // The refresh over the library: the season refreshes, before its
        // episodes, with the first scan's episode dates already stored.
        scanner
            .scan_with(None, options)
            .await
            .expect("full refresh");
        let providers = LocalProviderManager::new(Vec::new())
            .with_remote_images(tmdb, Arc::clone(&items))
            .with_image_store(persistence.clone(), metadata)
            .with_virtual_folders(vf);
        let id = |kind, path: &Path| derive_item_id(kind, &path.to_string_lossy()).expect("id");
        Self {
            season: id(BaseItemKind::Season, &season_dir),
            episode: id(BaseItemKind::Episode, &file),
            _tmp: tmp,
            items,
            persistence,
            providers,
        }
    }

    async fn row(&self, id: Uuid) -> BaseItemEntity {
        self.items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row")
    }

    async fn stored(&self, id: Uuid) -> Stored {
        let row = self.row(id).await;
        let mut provider_ids = self
            .persistence
            .provider_ids_for_items(&[id])
            .await
            .expect("ids")
            .remove(&id)
            .unwrap_or_default();
        provider_ids.sort();
        let trailers = row
            .data
            .as_deref()
            .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .and_then(|data| data.get("RemoteTrailers").cloned())
            .and_then(|trailers| trailers.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|trailer| trailer.get("Url")?.as_str().map(str::to_owned))
            .collect();
        Stored {
            name: row.name,
            overview: row.overview,
            premiere_date: row.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
            production_year: row.production_year,
            community_rating: row.community_rating,
            provider_ids,
            trailers,
        }
    }

    /// Clears what TheMovieDb set on `id`, then has the provider manager
    /// refresh it with `options`, so everything it stores is its own answer.
    /// An episode's title is cleared too; a season keeps its own name, which
    /// neither engine takes from TheMovieDb by default (`ImportSeasonName`).
    async fn refresh_through_the_provider_manager(
        &self,
        id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Stored {
        let mut row = self.row(id).await;
        if id == self.episode {
            row.name = None;
        }
        row.overview = None;
        row.premiere_date = None;
        row.production_year = None;
        row.community_rating = None;
        row.data = None;
        self.persistence.save_items(&[row]).await.expect("save");
        self.persistence
            .replace_provider_ids(id, &[])
            .await
            .expect("clear ids");
        self.providers
            .refresh_full_item(id, options)
            .await
            .expect("refresh");
        self.stored(id).await
    }
}

/// A metadata full refresh, images left alone: "Replace all metadata" when
/// `replace`, else "Search for missing metadata".
fn full_refresh(replace: bool) -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        image_refresh_mode: MetadataRefreshMode::None,
        replace_all_metadata: replace,
        ..MetadataRefreshOptions::default()
    }
}

fn ids(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut ids: Vec<_> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    ids.sort();
    ids
}

/// An episode: both engines store the same title, synopsis, air date, year,
/// rating, ids and trailers from TheMovieDb's answer (a trailer it lists
/// twice is stored once, `AddTrailerUrl`), whichever full refresh runs.
#[rstest::rstest]
#[case::replace_all_metadata(true)]
#[case::search_for_missing_metadata(false)]
#[tokio::test]
async fn an_episode_maps_tmdbs_answer_the_same_in_both_engines(#[case] replace: bool) {
    let options = full_refresh(replace);
    let engines = Engines::scanned(&options).await;
    let scanned = engines.stored(engines.episode).await;
    let refreshed = engines
        .refresh_through_the_provider_manager(engines.episode, &options)
        .await;
    let expected = Stored {
        name: Some("Winter Is Coming".to_owned()),
        overview: Some("Ned is summoned south.".to_owned()),
        premiere_date: Some("2011-04-17".to_owned()),
        production_year: Some(2011),
        community_rating: Some(8.5),
        provider_ids: ids(&[
            ("Imdb", "tt1480055"),
            ("Tmdb", "63056"),
            ("Tvdb", "3254641"),
        ]),
        trailers: vec!["https://www.youtube.com/watch?v=k1".to_owned()],
    };
    assert_eq!(scanned, expected);
    assert_eq!(refreshed, expected);
}

/// A season: both engines keep the season's own name and take TheMovieDb's
/// synopsis and id. Its air date differs on "Search for missing metadata".
/// Upstream's `SeasonMetadataService.EnableUpdatingPremiereDateFromChildren`
/// sets it to the episodes' earliest before the providers run (`BeforeSave`,
/// `MetadataService.cs:150,341-363,543-575`), so a provider's date only
/// replaces it when replacing everything. The scan ports that
/// (`update_season_premiere_date`); the provider manager's refresh reads the
/// episodes only for `SaveIsVirtualItem` and takes no date from them, so it
/// fills the cleared date with TheMovieDb's season date. gl-1838 tracks the
/// port; its fix gives both cases one date.
#[rstest::rstest]
#[case::replace_all_metadata(true, "2011-04-10", "2011-04-10")]
#[case::search_for_missing_metadata(false, "2011-04-17", "2011-04-10")]
#[tokio::test]
async fn a_season_maps_tmdbs_answer_the_same_in_both_engines_but_its_air_date(
    #[case] replace: bool,
    #[case] scan_date: &str,
    #[case] provider_manager_date: &str,
) {
    let options = full_refresh(replace);
    let engines = Engines::scanned(&options).await;
    let scanned = engines.stored(engines.season).await;
    let refreshed = engines
        .refresh_through_the_provider_manager(engines.season, &options)
        .await;
    let expected = |premiere: &str| Stored {
        name: Some("Season 1".to_owned()),
        overview: Some("The first season.".to_owned()),
        premiere_date: Some(premiere.to_owned()),
        production_year: Some(2011),
        community_rating: None,
        provider_ids: ids(&[("Tmdb", "3624")]),
        trailers: Vec::new(),
    };
    assert_eq!(scanned, expected(scan_date));
    assert_eq!(refreshed, expected(provider_manager_date));
}
