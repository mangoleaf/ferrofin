//! A provider-manager refresh (`LocalProviderManager::refresh_item`) and a
//! write made over the API while it waits for TheMovieDb (gl-1961).
//!
//! Upstream needs no lock: the editor and the refresh change one cached
//! `BaseItem`, so an edit never waits for a provider, and the refresh merges
//! its answers onto the edited item. Here each loads its own row, so the
//! refresh holds the item's metadata lock only from reading the row again
//! after its fetches to its last save: an edit made during the fetches takes
//! the lock at once, and the refresh then merges onto the edited row. A row
//! deleted meanwhile is not saved back.

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

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
use ferrofin_model::entities::{CollectionTypeOptions, MetadataField};
use ferrofin_providers::{LocalProviderManager, TmdbClient};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions, ProviderManager};
use uuid::Uuid;

/// `/tv/1399/season/1`: the season's overview and air date.
const SEASON_JSON: &str = r#"{
    "id": 3624, "name": "Series One", "overview": "The first season.",
    "air_date": "2011-04-10", "episodes": []
}"#;

/// The season's TheMovieDb lookup, held open until the test lets it answer.
struct HeldSeason {
    /// Signalled when the season's lookup reaches the stand-in.
    reached: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Answers the held lookup when it receives.
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

/// A TheMovieDb stand-in for one series (`1399`) and its season 1, which
/// holds the season's lookup until released.
fn spawn_tmdb(held: Arc<HeldSeason>) -> String {
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
            } else if line.contains("/tv/1399/season/1") {
                if let Some(reached) = held.reached.lock().expect("reached").take() {
                    let _ = reached.send(());
                    let _ = held.release.lock().expect("release").recv();
                }
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

/// A scanned `Show/Season 1/Show - S01E01.mkv` library, the provider manager
/// over its stores and the stand-in, and the season's id. The season's
/// overview and air date are cleared, so the refresh has them to fill.
struct Library {
    _tmp: tempfile::TempDir,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    providers: Arc<LocalProviderManager>,
    season: Uuid,
    /// Lets the held season lookup answer.
    release: std::sync::mpsc::Sender<()>,
    /// Signalled when the season lookup is held.
    reached: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl Library {
    async fn scanned() -> Self {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let held = Arc::new(HeldSeason {
            reached: std::sync::Mutex::new(None),
            release: std::sync::Mutex::new(release_rx),
        });
        let base = spawn_tmdb(Arc::clone(&held));
        let tmp = tempfile::tempdir().expect("tmp");
        let tv = tmp.path().join("tv");
        let season_dir = tv.join("Show").join("Season 1");
        std::fs::create_dir_all(&season_dir).expect("mkdir");
        std::fs::write(season_dir.join("Show - S01E01.mkv"), b"").expect("write");
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
        let providers = Arc::new(
            LocalProviderManager::new(Vec::new())
                .with_remote_images(tmdb, Arc::clone(&items))
                .with_image_store(persistence.clone(), metadata)
                .with_virtual_folders(vf),
        );
        let season =
            derive_item_id(BaseItemKind::Season, &season_dir.to_string_lossy()).expect("season id");
        let library = Self {
            _tmp: tmp,
            items,
            persistence,
            providers,
            season,
            release: release_tx,
            reached: Some(reached_rx),
        };
        let mut row = library.row().await.expect("season");
        row.overview = None;
        row.premiere_date = None;
        library.persistence.save_items(&[row]).await.expect("clear");
        // Held from now on: the scan above is done.
        *held.reached.lock().expect("reached") = Some(reached_tx);
        library
    }

    async fn row(&self) -> Option<BaseItemEntity> {
        self.items.retrieve_item(self.season).await.expect("read")
    }

    /// Refreshes the season ("Search for missing metadata", images left
    /// alone) in the background, waits until its TheMovieDb lookup is held,
    /// runs `change` there under the season's metadata lock, then lets the
    /// lookup answer. The flag is `false`, and `change` never ran, when the
    /// lock was not free within two seconds: the refresh held it across its
    /// fetch.
    async fn refresh_with_write_during_fetch<F, Fut>(
        &mut self,
        change: F,
    ) -> (bool, Result<(), ServiceError>)
    where
        F: FnOnce(Arc<FerrofinItemPersistenceService>, Uuid) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let options = MetadataRefreshOptions {
            metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
            image_refresh_mode: MetadataRefreshMode::None,
            ..MetadataRefreshOptions::default()
        };
        let refresh = tokio::spawn({
            let providers = Arc::clone(&self.providers);
            let season = self.season;
            async move { providers.refresh_full_item(season, &options).await }
        });
        tokio::time::timeout(
            Duration::from_secs(10),
            self.reached.take().expect("one refresh"),
        )
        .await
        .expect("the refresh asked TheMovieDb")
        .expect("reached");
        let locked = tokio::time::timeout(
            Duration::from_secs(2),
            self.providers.lock_item_metadata(self.season),
        )
        .await;
        let wrote = match locked {
            Ok(lock) => {
                change(Arc::clone(&self.persistence), self.season).await;
                drop(lock);
                true
            }
            Err(_) => false,
        };
        self.release.send(()).expect("release");
        let refreshed = tokio::time::timeout(Duration::from_secs(10), refresh)
            .await
            .expect("the refresh finished")
            .expect("join");
        (wrote, refreshed)
    }
}

/// An edit made while the refresh waits for TheMovieDb does not wait for it,
/// and the refresh merges onto the edited row: the overview the edit typed
/// stays (this mode only fills what is empty), and the air date the edit
/// left empty is filled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_during_the_fetch_is_not_held_and_the_refresh_merges_onto_it() {
    let mut library = Library::scanned().await;
    let items = Arc::clone(&library.items);
    let (wrote, refreshed) = library
        .refresh_with_write_during_fetch(|persistence, id| async move {
            let mut row = items.retrieve_item(id).await.expect("read").expect("row");
            row.overview = Some("Typed mid-fetch".to_owned());
            persistence.save_items(&[row]).await.expect("edit");
        })
        .await;
    refreshed.expect("refresh");
    assert!(wrote, "the edit waited for the refresh's TheMovieDb lookup");
    let row = library.row().await.expect("season");
    assert_eq!(
        row.overview.as_deref(),
        Some("Typed mid-fetch"),
        "the refresh saved over the edit"
    );
    assert_eq!(
        row.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2011-04-10".to_owned()),
        "the refresh's answer was not merged"
    );
}

/// A field locked while the refresh waits for TheMovieDb, with nothing else
/// of the row changed, is left as stored: the refresh reads the item's
/// `LockedFields` again and merges under them. The refresh is stamped, as no
/// provider failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_field_locked_during_the_fetch_is_left_as_stored() {
    let mut library = Library::scanned().await;
    let before = library.row().await.expect("season").date_last_refreshed;
    let (wrote, refreshed) = library
        .refresh_with_write_during_fetch(|persistence, id| async move {
            persistence
                .replace_locked_fields(id, &[MetadataField::Overview.json_value()])
                .await
                .expect("lock");
        })
        .await;
    refreshed.expect("refresh");
    assert!(wrote, "the edit waited for the refresh's TheMovieDb lookup");
    let row = library.row().await.expect("season");
    assert_eq!(row.overview, None, "the refresh filled a locked field");
    assert_eq!(
        row.premiere_date.map(|d| d.format("%Y-%m-%d").to_string()),
        Some("2011-04-10".to_owned()),
        "the refresh's answer was not merged"
    );
    assert!(
        row.date_last_refreshed > before,
        "the refresh was not stamped: {before:?} -> {:?}",
        row.date_last_refreshed
    );
}

/// A delete made while the refresh waits for TheMovieDb (the library
/// manager's delete holds the lock too) is not undone by the refresh's
/// save: the refresh finds the row gone and stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_during_the_fetch_is_not_saved_back() {
    let mut library = Library::scanned().await;
    let (wrote, refreshed) = library
        .refresh_with_write_during_fetch(|persistence, id| async move {
            persistence.delete_items(&[id]).await.expect("delete");
        })
        .await;
    assert!(
        wrote,
        "the delete waited for the refresh's TheMovieDb lookup"
    );
    assert!(
        matches!(refreshed, Err(ServiceError::NotFound(_))),
        "{refreshed:?}"
    );
    assert!(
        library.row().await.is_none(),
        "the refresh saved the deleted season back"
    );
}
