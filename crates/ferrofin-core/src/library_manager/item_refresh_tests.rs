//! What an item's refresh covers (`ProviderManager.RefreshItem`,
//! `RefreshArtist`, `RefreshFullItem`), over real repositories: the scan
//! each kind of item refreshes through, the provider manager for an item no
//! scan covers, and Identify's refusals.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::enums::ItemValueType;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{LibraryManager as _, ScanTarget};
use ferrofin_traits::persistence::ItemPersistenceService as _;
use ferrofin_traits::providers::{
    ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use uuid::Uuid;

use super::{FerrofinLibraryManager, ScanRunner};
use crate::item_count_service::FerrofinItemCountService;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
use crate::library_scan::{ScanOutcome, ScanRun};
use crate::people_repository::FerrofinPeopleRepository;
use crate::test_support::test_db;

const LIBRARY: Uuid = Uuid::from_u128(0x11B);

fn manager(db: &Database) -> FerrofinLibraryManager {
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ItemTypeLookup::new());
    FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(db.clone(), lookup)),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        Arc::new(FerrofinItemPersistenceService::new(db.clone())),
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    )
}

/// A row of `kind`: a folder or not, with its parent, library and path.
fn row(
    id: u128,
    kind: BaseItemKind,
    folder: bool,
    parent: Option<u128>,
    path: Option<&str>,
) -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(Uuid::from_u128(id)),
        type_: stored_type_name(kind).expect("stored type").to_owned(),
        name: Some(format!("{kind:?}")),
        is_folder: folder,
        parent_id: parent.map(|id| guid_to_db(Uuid::from_u128(id))),
        top_parent_id: Some(guid_to_db(LIBRARY)),
        path: path.map(str::to_owned),
        ..BaseItemEntity::default()
    }
}

/// [`row`] outside every library (no `TopParentId`).
fn outside(mut row: BaseItemEntity) -> BaseItemEntity {
    row.top_parent_id = None;
    row
}

async fn save(db: &Database, rows: &[BaseItemEntity]) {
    FerrofinItemPersistenceService::new(db.clone())
        .save_items(rows)
        .await
        .expect("save rows");
}

/// The scan each item's refresh is: a library scans itself, the root and
/// the aggregate folder every library, another folder its subtree, a file
/// its own path; a box set, a playlist, a view, a virtual season, a person
/// and a file outside every library have no scan (the provider manager
/// refreshes them).
#[tokio::test]
async fn each_kind_of_item_refreshes_through_its_own_scan() {
    let db = test_db().await;
    let mgr = manager(&db);
    let target = |row: BaseItemEntity| {
        let mgr = &mgr;
        async move { mgr.refresh_target(&row).await.expect("target") }
    };
    let library = row(
        0x11B,
        BaseItemKind::CollectionFolder,
        true,
        None,
        Some("/media/tv"),
    );
    assert_eq!(target(library).await, Some(ScanTarget::Library(LIBRARY)));
    for kind in [BaseItemKind::UserRootFolder, BaseItemKind::AggregateFolder] {
        assert_eq!(
            target(row(2, kind, true, None, None)).await,
            Some(ScanTarget::All)
        );
    }
    let series = row(
        3,
        BaseItemKind::Series,
        true,
        Some(0x11B),
        Some("/media/tv/Firefly"),
    );
    assert_eq!(
        target(series).await,
        Some(ScanTarget::Paths(vec!["/media/tv/Firefly".to_owned()]))
    );
    let episode = row(
        4,
        BaseItemKind::Episode,
        false,
        Some(3),
        Some("/media/tv/Firefly/e1.mkv"),
    );
    assert_eq!(
        target(episode.clone()).await,
        Some(ScanTarget::Items(vec![
            "/media/tv/Firefly/e1.mkv".to_owned()
        ]))
    );
    for kind in [
        BaseItemKind::BoxSet,
        BaseItemKind::Playlist,
        BaseItemKind::PlaylistsFolder,
        BaseItemKind::UserView,
    ] {
        assert_eq!(
            target(row(5, kind, true, None, Some("/data/x"))).await,
            None,
            "{kind:?}"
        );
    }
    let virtual_season = row(6, BaseItemKind::Season, true, Some(3), None);
    assert_eq!(target(virtual_season).await, None);
    assert_eq!(
        target(row(7, BaseItemKind::Person, false, None, None)).await,
        None
    );
    assert_eq!(target(outside(episode)).await, None, "a file in no library");
    let streamed = row(
        8,
        BaseItemKind::Video,
        false,
        None,
        Some("http://host/v.ts"),
    );
    assert_eq!(target(streamed).await, None);
}

/// [`row`] named `name`, with the clean name the artist credits match on.
fn named(row: BaseItemEntity, name: &str) -> BaseItemEntity {
    BaseItemEntity {
        name: Some(name.to_owned()),
        clean_name: Some(crate::text_util::get_clean_value(name)),
        ..row
    }
}

/// A music library with two artist folders, Artist and Various (the latter
/// holding a disc folder), and four albums: two under Artist, one on the
/// disc, one straight in the library. Every album credits Artist; the
/// compilation artist, known only by name and pathed in the metadata folder,
/// is credited on one album under Artist, the disc's and the library's.
/// Returns the folder-backed and the by-name artist.
async fn seed_music(db: &Database) -> (BaseItemEntity, BaseItemEntity) {
    let folder = |id, kind, parent, path| row(id, kind, true, Some(parent), Some(path));
    let artist = named(
        folder(0xA, BaseItemKind::MusicArtist, 0x11B, "/media/music/Artist"),
        "Artist",
    );
    let by_name = outside(named(
        row(
            0xC,
            BaseItemKind::MusicArtist,
            false,
            None,
            Some("/config/metadata/artists/C"),
        ),
        "Compilation",
    ));
    let album = |id: u128, parent| {
        named(
            folder(id, BaseItemKind::MusicAlbum, parent, "/media/music/x"),
            &format!("Album {id}"),
        )
    };
    save(
        db,
        &[
            row(
                0x11B,
                BaseItemKind::CollectionFolder,
                true,
                None,
                Some("/media/music"),
            ),
            artist.clone(),
            by_name.clone(),
            named(
                folder(
                    0xB,
                    BaseItemKind::MusicArtist,
                    0x11B,
                    "/media/music/Various",
                ),
                "Various",
            ),
            folder(0xD, BaseItemKind::Folder, 0xB, "/media/music/Various/D"),
            album(0x21, 0xA),
            album(0x22, 0xA),
            album(0x23, 0xD),
            album(0x24, 0x11B),
        ],
    )
    .await;
    let credits = FerrofinItemPersistenceService::new(db.clone());
    for (id, compilation) in [(0x21, true), (0x22, false), (0x23, true), (0x24, true)] {
        let mut values = vec![(i32::from(ItemValueType::Artist), "Artist".to_owned())];
        if compilation {
            values.push((
                i32::from(ItemValueType::AlbumArtist),
                "Compilation".to_owned(),
            ));
        }
        credits
            .save_item_values(Uuid::from_u128(id), &values)
            .await
            .expect("credit the album");
    }
    (artist, by_name)
}

/// `refresh_target`'s [`ScanTarget::Artist`] for `artist`, folders sorted.
async fn artist_target(
    mgr: &FerrofinLibraryManager,
    artist: &BaseItemEntity,
) -> (Uuid, Option<String>, Vec<String>) {
    let Some(ScanTarget::Artist {
        id,
        path,
        mut folders,
    }) = mgr.refresh_target(artist).await.expect("target")
    else {
        panic!("an artist refreshes as one");
    };
    folders.sort();
    (id, path, folders)
}

/// `RefreshArtist`: the artist folders above the credited albums validate
/// their children — each once, an album with no artist folder above it
/// adding none — and the artist refreshes itself, folder-backed or known
/// only by name (which has no folder of its own).
#[tokio::test]
async fn an_artist_refreshes_the_artist_folders_above_its_albums() {
    let db = test_db().await;
    let mgr = manager(&db);
    let (artist, by_name) = seed_music(&db).await;
    let folders = vec![
        "/media/music/Artist".to_owned(),
        "/media/music/Various".to_owned(),
    ];
    assert_eq!(
        artist_target(&mgr, &artist).await,
        (
            Uuid::from_u128(0xA),
            Some("/media/music/Artist".to_owned()),
            folders.clone()
        )
    );
    assert_eq!(
        artist_target(&mgr, &by_name).await,
        (Uuid::from_u128(0xC), None, folders)
    );

    // An artist whose only credited album is filed under another artist's
    // folder validates that folder, never its own; one credited on no album
    // validates nothing.
    let solo = named(
        row(
            0xE,
            BaseItemKind::MusicArtist,
            true,
            Some(0x11B),
            Some("/media/music/Solo"),
        ),
        "Solo",
    );
    let duets = named(
        row(
            0x25,
            BaseItemKind::MusicAlbum,
            true,
            Some(0xB),
            Some("/media/music/Various/Duets"),
        ),
        "Duets",
    );
    let nobody = outside(named(
        row(0xF, BaseItemKind::MusicArtist, false, None, None),
        "Nobody",
    ));
    save(&db, &[solo.clone(), duets, nobody.clone()]).await;
    FerrofinItemPersistenceService::new(db.clone())
        .save_item_values(
            Uuid::from_u128(0x25),
            &[(i32::from(ItemValueType::Artist), "Solo".to_owned())],
        )
        .await
        .expect("credit the album");
    assert_eq!(
        artist_target(&mgr, &solo).await,
        (
            Uuid::from_u128(0xE),
            Some("/media/music/Solo".to_owned()),
            vec!["/media/music/Various".to_owned()]
        )
    );
    assert_eq!(
        artist_target(&mgr, &nobody).await,
        (Uuid::from_u128(0xF), None, Vec::new())
    );
}

/// Identify refreshes a file, a folder's subtree or a library through the
/// scan, a by-name artist alone through the scanner, and refuses an item the
/// scan cannot reach: a path under no library location, or a file in no
/// library. An item with no file of its own has no scan.
#[tokio::test]
async fn identify_targets_and_refusals() {
    use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
    use ferrofin_traits::library::VirtualFolderManager as _;
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("media");
    std::fs::create_dir_all(&media).expect("media");
    let vf = Arc::new(crate::FerrofinVirtualFolderManager::new(
        tmp.path().join("views"),
    ));
    vf.add_virtual_folder(
        "Movies",
        Some(ferrofin_model::entities::CollectionTypeOptions::movies),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("library");
    let db = test_db().await;
    let mgr = manager(&db).with_virtual_folders(vf);

    let inside = format!("{}/Movie/movie.mkv", media.display());
    let movie = row(1, BaseItemKind::Movie, false, None, Some(&inside));
    assert_eq!(
        mgr.identify_target(&movie).await.expect("target"),
        Some(ScanTarget::Items(vec![inside]))
    );
    let elsewhere = row(
        2,
        BaseItemKind::Movie,
        false,
        None,
        Some("/elsewhere/m.mkv"),
    );
    assert!(matches!(
        mgr.identify_target(&elsewhere).await,
        Err(ServiceError::Conflict(_))
    ));
    let unfiled = outside(row(3, BaseItemKind::Movie, false, None, Some("/x/m.mkv")));
    assert!(matches!(
        mgr.identify_target(&unfiled).await,
        Err(ServiceError::Conflict(_))
    ));
    let by_name = outside(row(4, BaseItemKind::MusicArtist, false, None, None));
    assert_eq!(
        mgr.identify_target(&by_name).await.expect("target"),
        Some(ScanTarget::Artist {
            id: Uuid::from_u128(4),
            path: None,
            folders: Vec::new(),
        })
    );
    let person = outside(row(5, BaseItemKind::Person, false, None, None));
    assert_eq!(mgr.identify_target(&person).await.expect("target"), None);
    // A folder inside a library location identifies with its subtree; a
    // library always reaches.
    let folder = format!("{}/Show", media.display());
    let series = row(6, BaseItemKind::Series, true, None, Some(&folder));
    assert_eq!(
        mgr.identify_target(&series).await.expect("target"),
        Some(ScanTarget::Paths(vec![folder]))
    );
    let library = row(
        0x11B,
        BaseItemKind::CollectionFolder,
        true,
        None,
        Some("/x"),
    );
    assert_eq!(
        mgr.identify_target(&library).await.expect("target"),
        Some(ScanTarget::Library(LIBRARY))
    );
}

/// A file's refresh runs through the scan of its path; an item no scan
/// covers through the provider manager: queued at high priority, or, for
/// Identify, `RefreshFullItem` awaited.
#[tokio::test]
async fn a_refresh_runs_through_the_scan_or_the_provider_manager() {
    let db = test_db().await;
    let runner = Arc::new(RecordingRunner::default());
    let providers = Arc::new(RecordingProviders::default());
    let mgr = manager(&db)
        .with_scan_runner(runner.clone())
        .with_providers(providers.clone());
    let options = MetadataRefreshOptions {
        metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
        ..MetadataRefreshOptions::default()
    };
    let file = row(
        1,
        BaseItemKind::Audio,
        false,
        None,
        Some("/media/music/a.flac"),
    );
    let box_set = row(2, BaseItemKind::BoxSet, true, None, None);

    assert!(
        mgr.refresh_item_metadata(&file, &options)
            .await
            .expect("ran")
    );
    assert_eq!(
        runner.take(),
        [(
            ScanTarget::Items(vec!["/media/music/a.flac".to_owned()]),
            options.clone()
        )]
    );
    mgr.queue_item_refresh(&file, &options)
        .await
        .expect("queued");
    for _ in 0..500 {
        if !runner.scopes.lock().expect("scopes").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        runner.take(),
        [(
            ScanTarget::Items(vec!["/media/music/a.flac".to_owned()]),
            options.clone()
        )]
    );

    mgr.queue_item_refresh(&box_set, &options)
        .await
        .expect("queued");
    assert!(
        mgr.refresh_item_metadata(&box_set, &options)
            .await
            .expect("queued")
    );
    assert!(
        mgr.refresh_full_item(&box_set, &options)
            .await
            .expect("ran")
    );
    let id = Uuid::from_u128(2);
    assert_eq!(
        providers.take(),
        [
            ("queue", id, options.clone()),
            ("queue", id, options.clone()),
            ("full", id, options),
        ]
    );
    assert!(runner.take().is_empty(), "no scan covers a box set");
}

/// Without a provider manager, an item no scan covers is refused rather
/// than silently left unrefreshed; without the library locations, so is an
/// Identify that has to check them.
#[tokio::test]
async fn a_provider_refresh_without_a_provider_manager_is_refused() {
    let db = test_db().await;
    let mgr = manager(&db);
    let box_set = row(2, BaseItemKind::BoxSet, true, None, None);
    assert!(matches!(
        mgr.queue_item_refresh(&box_set, &MetadataRefreshOptions::default())
            .await,
        Err(ServiceError::Backend(_))
    ));
    // Nor is an Identify that cannot read the library locations.
    let movie = row(3, BaseItemKind::Movie, false, None, Some("/media/m.mkv"));
    assert!(matches!(
        mgr.identify_target(&movie).await,
        Err(ServiceError::Backend(_))
    ));
}

/// Records each scan it is asked to run, which then finishes at once.
#[derive(Default)]
struct RecordingRunner {
    scopes: Mutex<Vec<(ScanTarget, MetadataRefreshOptions)>>,
}

impl RecordingRunner {
    fn take(&self) -> Vec<(ScanTarget, MetadataRefreshOptions)> {
        std::mem::take(&mut *self.scopes.lock().expect("scopes"))
    }
}

#[async_trait]
impl ScanRunner for RecordingRunner {
    async fn run(&self, scope: &ScanTarget, run: ScanRun<'_>) -> Result<ScanOutcome, ServiceError> {
        self.scopes
            .lock()
            .expect("scopes")
            .push((scope.clone(), run.options.clone()));
        Ok(ScanOutcome::default())
    }
}

/// Records the refreshes asked of it: queued (`"queue"`, at high priority)
/// or awaited (`"full"`).
#[derive(Default)]
struct RecordingProviders {
    refreshes: Mutex<Vec<(&'static str, Uuid, MetadataRefreshOptions)>>,
}

impl RecordingProviders {
    fn take(&self) -> Vec<(&'static str, Uuid, MetadataRefreshOptions)> {
        std::mem::take(&mut *self.refreshes.lock().expect("refreshes"))
    }
}

#[async_trait]
impl ProviderManager for RecordingProviders {
    async fn queue_refresh(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
        priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        assert_eq!(priority, RefreshPriority::High);
        self.refreshes
            .lock()
            .expect("refreshes")
            .push(("queue", item_id, options.clone()));
        Ok(())
    }
    async fn refresh_full_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        self.refreshes
            .lock()
            .expect("refreshes")
            .push(("full", item_id, options.clone()));
        Ok(())
    }
    async fn refresh_single_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        unreachable!("not on these paths")
    }
    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: ferrofin_model::entities::ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unreachable!("not on these paths")
    }
    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: ferrofin_model::entities::ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        unreachable!("not on these paths")
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &ferrofin_model::providers::RemoteImageQuery,
    ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
        unreachable!("not on these paths")
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
        unreachable!("not on these paths")
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        unreachable!("not on these paths")
    }
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        unreachable!("not on these paths")
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError> {
        unreachable!("not on these paths")
    }
    async fn get_metadata_options(
        &self,
        _item_id: Uuid,
    ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
        unreachable!("not on these paths")
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        unreachable!("not on these paths")
    }
}
