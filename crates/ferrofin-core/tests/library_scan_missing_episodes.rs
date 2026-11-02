//! The TMDb missing/unaired episode provider (`TmdbMissingEpisodeProvider`,
//! Jellyfin v12.2), end to end through a scan: with the TMDb settings
//! page's options on and the library opted in, a series refresh creates a
//! virtual episode for each aired episode it lacks and each unaired one TMDb
//! lists, and a virtual season for a season it has nothing of; turned off,
//! the next refresh removes them again.
//!
//! One series (`Show`, TMDb 1399) with one season folder holding episode 1,
//! against a stand-in serving TheMovieDb under `/tmdb`.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
use ferrofin_providers::TmdbClient;
use ferrofin_traits::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
use uuid::Uuid;

/// TheMovieDb's series, listing seasons 1 and 2.
const TMDB_SERIES: &str = r#"{"id": 1399, "name": "Show", "overview": "A show.",
    "first_air_date": "2011-04-17", "status": "Returning Series",
    "seasons": [{"season_number": 1, "episode_count": 3},
                {"season_number": 2, "episode_count": 1}]}"#;

/// TheMovieDb finds the series by name.
const TMDB_FOUND: &str = r#"{"results": [{"id": 1399, "name": "Show"}]}"#;

/// Season 1: episode 1 (the file the library has), episode 2 aired long
/// ago, episode 3 far in the future.
const TMDB_SEASON_1: &str = r#"{"id": 3624, "name": "Season 1", "air_date": "2011-04-17",
    "episodes": [
      {"id": 63056, "episode_number": 1, "name": "One", "air_date": "2011-04-17"},
      {"id": 63057, "episode_number": 2, "name": "Two", "overview": "Second.",
       "air_date": "2011-04-24"},
      {"id": 63058, "episode_number": 3, "name": "Three", "air_date": "2099-01-01"}]}"#;

/// Season 2: one episode, far in the future.
const TMDB_SEASON_2: &str = r#"{"id": 3625, "name": "Season 2", "air_date": "2099-02-01",
    "episodes": [
      {"id": 63060, "episode_number": 1, "name": "Next", "air_date": "2099-02-01"}]}"#;

/// The stand-in's answer to request line `line`; `None` is a 404.
fn answer(line: &str) -> Option<&'static str> {
    [
        ("/tmdb/search/tv", TMDB_FOUND),
        ("/tmdb/tv/1399/season/1?", TMDB_SEASON_1),
        ("/tmdb/tv/1399/season/2?", TMDB_SEASON_2),
        ("/tmdb/tv/1399?", TMDB_SERIES),
    ]
    .iter()
    .find(|(needle, _)| line.contains(needle))
    .map(|(_, body)| *body)
}

/// Starts the stand-in; returns its base URL.
fn spawn_tmdb() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default().to_owned();
            let (status, payload) =
                answer(&line).map_or(("404 Not Found", "{}"), |b| ("200 OK", b));
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    format!("http://{addr}")
}

/// A plugin manager holding only the TMDb plugin's configuration, which a
/// test rewrites between scans.
struct TmdbSettings(Mutex<String>);

#[async_trait]
impl PluginManager for TmdbSettings {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_plugin(&self, _id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok(None)
    }
    async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError> {
        if id == ferrofin_providers::builtin_plugins::TMDB.id {
            Ok(self.0.lock().expect("settings").clone().into_bytes())
        } else {
            Err(ServiceError::not_found(format!("plugin {id}")))
        }
    }
    async fn set_plugin_configuration(
        &self,
        _id: Uuid,
        _config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok(None)
    }
    async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn set_repositories(
        &self,
        _repositories: Vec<RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

/// The TMDb settings with the missing/unaired options as given, `libraries`
/// opted in.
fn settings(unaired: bool, missing: bool, libraries: &[&str]) -> String {
    serde_json::json!({
        "ImportUnairedEpisodes": unaired,
        "ImportMissingEpisodes": missing,
        "ImportSpecials": false,
        "EnabledMissingEpisodeLibraries": libraries,
        "MissingEpisodeRefreshIntervalDays": 7,
        "UpcomingEpisodeGracePeriodDays": 7,
    })
    .to_string()
}

/// `tv/Show/Season 1/Show S01E01.mkv` in a TV library.
struct Library {
    vf: Arc<dyn VirtualFolderManager>,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: Arc<dyn ItemRepository>,
    settings: Arc<TmdbSettings>,
    folder_id: String,
    tmp: std::path::PathBuf,
}

impl Library {
    async fn new(tmp: &Path) -> Self {
        let tv = tmp.join("tv");
        let season_dir = tv.join("Show").join("Season 1");
        std::fs::create_dir_all(&season_dir).expect("mkdir");
        std::fs::write(season_dir.join("Show S01E01.mkv"), b"0123456789").expect("write");
        let db = ferrofin_db::Database::connect_in_memory()
            .await
            .expect("connect");
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
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let folder_id = vf
            .get_virtual_folders()
            .await
            .expect("folders")
            .into_iter()
            .find_map(|f| f.item_id)
            .expect("library item id");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        Self {
            vf,
            persistence,
            items,
            settings: Arc::new(TmdbSettings(Mutex::new(settings(false, false, &[])))),
            folder_id,
            tmp: tmp.to_path_buf(),
        }
    }

    fn set(&self, json: String) {
        *self.settings.0.lock().expect("settings") = json;
    }

    fn scanner(&self, base: &str) -> LibraryScanner {
        let tmdb = TmdbClient::new()
            .with_base_url(&format!("{base}/tmdb"))
            .with_image_root(&format!("{base}/img"));
        tmdb.attach_plugin_manager(Arc::clone(&self.settings) as Arc<dyn PluginManager>);
        LibraryScanner::new(
            Arc::clone(&self.vf),
            Arc::new(FerrofinFileSystem::new()),
            self.persistence.clone(),
        )
        .with_items(Arc::clone(&self.items))
        .with_metadata(Arc::new(tmdb), self.tmp.join("metadata"))
    }

    fn series_id(&self) -> Uuid {
        derive_item_id(
            BaseItemKind::Series,
            &self.tmp.join("tv").join("Show").to_string_lossy(),
        )
        .expect("series id")
    }

    fn season_1_id(&self) -> Uuid {
        derive_item_id(
            BaseItemKind::Season,
            &self
                .tmp
                .join("tv")
                .join("Show")
                .join("Season 1")
                .to_string_lossy(),
        )
        .expect("season id")
    }

    /// The upstream provider's id for its virtual season `season` or
    /// episode `episode` of it: `GetNewItemId(series.Id.ToString("N") +
    /// "Season" + n [+ "Episode" + m], type)`.
    fn provider_id(&self, season: i32, episode: Option<i32>) -> Uuid {
        let series = self.series_id().simple().to_string();
        match episode {
            None => derive_item_id(BaseItemKind::Season, &format!("{series}Season{season}")),
            Some(m) => derive_item_id(
                BaseItemKind::Episode,
                &format!("{series}Season{season}Episode{m}"),
            ),
        }
        .expect("id")
    }

    async fn row(&self, id: Uuid) -> Option<BaseItemEntity> {
        self.items.retrieve_item(id).await.expect("read")
    }

    /// The series' virtual episodes as `(season, episode)`, sorted.
    async fn virtual_episodes(&self) -> Vec<(i64, i64)> {
        let mut found: Vec<(i64, i64)> = self
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Episode],
                is_virtual_item: Some(true),
                ..Default::default()
            })
            .await
            .expect("query")
            .iter()
            .filter_map(|r| Some((r.parent_index_number?, r.index_number?)))
            .collect();
        found.sort_unstable();
        found
    }
}

/// `row`'s parent, parsed.
fn parent(row: &BaseItemEntity) -> Option<Uuid> {
    row.parent_id
        .as_deref()
        .and_then(|p| Uuid::parse_str(p).ok())
}

#[tokio::test]
async fn opted_in_series_gains_missing_and_unaired_episodes_and_loses_them_when_turned_off() {
    let tmp = tempfile::tempdir().expect("tmp");
    let lib = Library::new(tmp.path()).await;
    let base = spawn_tmdb();
    lib.set(settings(true, true, &[lib.folder_id.as_str()]));
    lib.scanner(&base).scan_all().await.expect("scan");

    // Episode 1 is on disk; 2 aired and is missing; 3 and season 2's
    // episode 1 have not aired.
    assert_eq!(lib.virtual_episodes().await, vec![(1, 2), (1, 3), (2, 1)]);

    let missing = lib.row(lib.provider_id(1, Some(2))).await.expect("S01E02");
    assert!(missing.is_virtual_item && missing.path.is_none());
    assert_eq!(missing.name.as_deref(), Some("Two"));
    assert_eq!(missing.overview.as_deref(), Some("Second."));
    assert_eq!(parent(&missing), Some(lib.season_1_id()));
    let ids = lib
        .persistence
        .provider_ids_for_items(&[lib.provider_id(1, Some(2))])
        .await
        .expect("ids");
    assert!(
        ids.values()
            .flatten()
            .any(|(p, v)| p.eq_ignore_ascii_case("Tmdb") && v == "63057"),
        "{ids:?}"
    );

    // Season 2 has nothing on disk: a virtual season holds its episode.
    let season_2 = lib.row(lib.provider_id(2, None)).await.expect("season 2");
    assert!(season_2.is_virtual_item);
    assert_eq!(season_2.index_number, Some(2));
    let next = lib.row(lib.provider_id(2, Some(1))).await.expect("S02E01");
    assert_eq!(parent(&next), Some(lib.provider_id(2, None)));

    // Both options off: the next refresh removes what it created.
    lib.set(settings(false, false, &[lib.folder_id.as_str()]));
    lib.scanner(&base).scan_all().await.expect("rescan");
    assert_eq!(lib.virtual_episodes().await, Vec::<(i64, i64)>::new());
    assert!(
        lib.row(lib.provider_id(2, None)).await.is_none(),
        "empty virtual season goes"
    );
    assert!(lib.row(lib.series_id()).await.is_some());
}

#[tokio::test]
async fn a_library_not_opted_in_gets_no_virtual_episodes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let lib = Library::new(tmp.path()).await;
    let base = spawn_tmdb();
    lib.set(settings(true, true, &[]));
    lib.scanner(&base).scan_all().await.expect("scan");
    assert_eq!(lib.virtual_episodes().await, Vec::<(i64, i64)>::new());
}

#[tokio::test]
async fn only_unaired_episodes_when_missing_import_is_off() {
    let tmp = tempfile::tempdir().expect("tmp");
    let lib = Library::new(tmp.path()).await;
    let base = spawn_tmdb();
    lib.set(settings(true, false, &[lib.folder_id.as_str()]));
    lib.scanner(&base).scan_all().await.expect("scan");
    assert_eq!(lib.virtual_episodes().await, vec![(1, 3), (2, 1)]);
}

#[tokio::test]
async fn a_series_refresh_runs_the_provider_too() {
    let tmp = tempfile::tempdir().expect("tmp");
    let lib = Library::new(tmp.path()).await;
    let base = spawn_tmdb();
    lib.scanner(&base).scan_all().await.expect("scan");
    assert_eq!(lib.virtual_episodes().await, Vec::<(i64, i64)>::new());
    // The series' own refresh is a path-scoped scan (its closing passes are
    // the touched ones).
    lib.set(settings(true, true, &[lib.folder_id.as_str()]));
    lib.scanner(&base)
        .scan_paths(&[tmp
            .path()
            .join("tv")
            .join("Show")
            .to_string_lossy()
            .into_owned()])
        .await
        .expect("series refresh");
    assert_eq!(lib.virtual_episodes().await, vec![(1, 2), (1, 3), (2, 1)]);
}

/// A file arriving for a placeholder supersedes it: the next refresh removes
/// the virtual episode (`GetExistingEpisodes`, "a physical episode now
/// exists for this slot") and creates no other.
#[tokio::test]
async fn a_file_arriving_for_a_placeholder_replaces_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let lib = Library::new(tmp.path()).await;
    let base = spawn_tmdb();
    lib.set(settings(true, true, &[lib.folder_id.as_str()]));
    lib.scanner(&base).scan_all().await.expect("scan");
    assert!(lib.virtual_episodes().await.contains(&(1, 2)));
    std::fs::write(
        tmp.path()
            .join("tv")
            .join("Show")
            .join("Season 1")
            .join("Show S01E02.mkv"),
        b"0123456789",
    )
    .expect("write");
    lib.scanner(&base).scan_all().await.expect("rescan");
    assert_eq!(lib.virtual_episodes().await, vec![(1, 3), (2, 1)]);
    assert!(lib.row(lib.provider_id(1, Some(2))).await.is_none());
}
