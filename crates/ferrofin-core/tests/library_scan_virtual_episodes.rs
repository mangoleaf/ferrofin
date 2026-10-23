//! Virtual (missing) episodes, the path-less rows Jellyfin's missing-episode
//! providers store and an adopted database keeps, as a series refresh
//! removes them once they stand for nothing missing.
//!
//! Upstream: `SeriesMetadataService.AfterMetadataRefresh` →
//! `RemoveObsoleteEpisodes` (v12.2), on every refresh of the series: a
//! virtual episode with no number, one whose number a physical episode of
//! its season covers, and one carrying a provider id a single physical
//! episode of the series holds (aired, DVD and absolute orders number the
//! same episode differently) are deleted. A refresh of anything below the
//! series (a library monitor's new file) does not refresh the series.

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::store::guid_to_db;
use ferrofin_db::{Database, entities::base_items::BaseItemEntity};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use uuid::Uuid;

/// A TV library holding `Show/Season 01/Show S01E01.mkv`, whose NFO gives
/// it TheTVDB id `111`, scanned once.
struct Fixture {
    _tmp: tempfile::TempDir,
    episode: PathBuf,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tv = tmp.path().join("tv");
        let season = tv.join("Show/Season 01");
        std::fs::create_dir_all(&season).expect("mkdir");
        let episode = season.join("Show S01E01.mkv");
        std::fs::write(&episode, b"0123").expect("episode");
        std::fs::write(
            season.join("Show S01E01.nfo"),
            "<episodedetails><title>Pilot</title><season>1</season><episode>1</episode>\
             <uniqueid type=\"tvdb\">111</uniqueid></episodedetails>",
        )
        .expect("nfo");
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
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
            db,
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_items(Arc::clone(&items));
        scanner.scan_all().await.expect("scan");
        Self {
            _tmp: tmp,
            episode,
            items,
            persistence,
            scanner,
        }
    }

    /// Stores a virtual episode of the series, numbered `season`/`number`,
    /// carrying `ids`, as an adopted Jellyfin database holds one.
    async fn add_virtual(
        &self,
        season: Option<i64>,
        number: Option<i64>,
        ids: &[(&str, &str)],
    ) -> Uuid {
        let physical = derive_item_id(BaseItemKind::Episode, &self.episode.to_string_lossy())
            .expect("episode id");
        let template = self
            .items
            .retrieve_item(physical)
            .await
            .expect("read")
            .expect("the physical episode");
        let id = Uuid::new_v4();
        let row = BaseItemEntity {
            id: guid_to_db(id),
            name: Some(format!("Missing {season:?}x{number:?}")),
            path: None,
            is_virtual_item: true,
            parent_index_number: season,
            index_number: number,
            presentation_unique_key: None,
            data: None,
            ..template
        };
        self.persistence.save_items(&[row]).await.expect("save");
        let ids: Vec<(String, String)> = ids
            .iter()
            .map(|&(provider, id)| (provider.to_owned(), id.to_owned()))
            .collect();
        self.persistence
            .replace_provider_ids(id, &ids)
            .await
            .expect("ids");
        id
    }

    async fn exists(&self, id: Uuid) -> bool {
        self.items.retrieve_item(id).await.expect("read").is_some()
    }
}

#[tokio::test]
async fn a_series_refresh_removes_its_obsolete_virtual_episodes() {
    let f = Fixture::new().await;
    let covered = f.add_virtual(Some(1), Some(1), &[]).await;
    let missing = f.add_virtual(Some(1), Some(2), &[]).await;
    let numberless = f.add_virtual(Some(1), None, &[]).await;
    let renumbered = f.add_virtual(Some(2), Some(7), &[("Tvdb", "111")]).await;

    // A library monitor's event for the episode file refreshes that file,
    // not its series.
    f.scanner
        .scan_paths(&[f.episode.to_string_lossy().into_owned()])
        .await
        .expect("monitor scan");
    for id in [covered, missing, numberless, renumbered] {
        assert!(f.exists(id).await, "kept by a refresh below the series");
    }

    f.scanner.scan_all().await.expect("rescan");
    assert!(!f.exists(covered).await, "S01E01 is on disk");
    assert!(!f.exists(numberless).await, "no episode number");
    assert!(
        !f.exists(renumbered).await,
        "TheTVDB's 111 is the physical S01E01"
    );
    assert!(f.exists(missing).await, "S01E02 is still missing");
    let physical =
        derive_item_id(BaseItemKind::Episode, &f.episode.to_string_lossy()).expect("episode id");
    assert!(f.exists(physical).await, "the physical episode stays");
}
