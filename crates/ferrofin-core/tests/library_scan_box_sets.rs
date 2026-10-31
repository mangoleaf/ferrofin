//! A scan of the Collections (`boxsets`) library refreshes its box sets, as
//! upstream's `Folder.ValidateChildrenInternal2` → `RefreshMetadataRecursive`
//! does for every library: each box set in the library's locations goes
//! through the single-item refresh with the scan's refresh options (gl-324).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner, ScanCancel, ScanRun,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use ferrofin_traits::providers::{
    ItemUpdateType, MetadataRefreshMode, MetadataRefreshOptions, ProviderManager, RefreshPriority,
};
use uuid::Uuid;

const BOX_SET: &str = "MediaBrowser.Controller.Entities.Movies.BoxSet";

/// A provider manager that records each single-item refresh and its modes,
/// and fails the refresh of `fail`.
#[derive(Default)]
struct RecordingRefreshes {
    refreshed: Mutex<Vec<(Uuid, MetadataRefreshMode, MetadataRefreshMode)>>,
    fail: Mutex<Option<Uuid>>,
}

#[async_trait]
impl ProviderManager for RecordingRefreshes {
    async fn queue_refresh(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
        _priority: RefreshPriority,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn refresh_full_item(
        &self,
        _item_id: Uuid,
        _options: &MetadataRefreshOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn refresh_single_item(
        &self,
        item_id: Uuid,
        options: &MetadataRefreshOptions,
    ) -> Result<ItemUpdateType, ServiceError> {
        self.refreshed.lock().expect("log").push((
            item_id,
            options.metadata_refresh_mode,
            options.image_refresh_mode,
        ));
        if *self.fail.lock().expect("fail") == Some(item_id) {
            return Err(ServiceError::backend("provider down"));
        }
        Ok(ItemUpdateType::None)
    }
    async fn save_image_from_url(
        &self,
        _item_id: Uuid,
        _url: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn save_image(
        &self,
        _item_id: Uuid,
        _content: &[u8],
        _mime_type: &str,
        _image_type: ImageType,
        _image_index: Option<i32>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_available_remote_images(
        &self,
        _item_id: Uuid,
        _query: &ferrofin_model::providers::RemoteImageQuery,
    ) -> Result<Vec<ferrofin_model::providers::RemoteImageInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_remote_image_provider_info(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ImageProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn save_metadata(
        &self,
        _item_id: Uuid,
        _update_type: ItemUpdateType,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_external_id_infos(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<ferrofin_model::providers::ExternalIdInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_all_metadata_plugins(
        &self,
    ) -> Result<Vec<ferrofin_model::configuration::MetadataPluginSummary>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_metadata_options(
        &self,
        _item_id: Uuid,
    ) -> Result<ferrofin_model::configuration::MetadataOptions, ServiceError> {
        Ok(ferrofin_model::configuration::MetadataOptions::default())
    }
    async fn get_refresh_queue(&self) -> Result<Vec<Uuid>, ServiceError> {
        Ok(Vec::new())
    }
}

/// A Collections library over `{data}/collections` holding two box sets, one
/// stored with an adopted row's `%AppDataPath%` token, plus a movies library
/// and box sets outside the Collections library's location: one in the movies
/// library, one in a sibling `collections2` folder.
struct Lab {
    tmp: tempfile::TempDir,
    scanner: LibraryScanner,
    refreshes: Arc<RecordingRefreshes>,
    vf: Arc<dyn VirtualFolderManager>,
    collections: std::path::PathBuf,
    inside: Uuid,
    adopted: Uuid,
}

#[allow(clippy::too_many_lines)] // one fixture: two libraries and four box sets
async fn lab() -> Lab {
    let tmp = tempfile::tempdir().expect("tempdir");
    let collections = tmp.path().join("data/collections");
    let movies = tmp.path().join("movies");
    std::fs::create_dir_all(collections.join("Heist [boxset]")).expect("box set folder");
    std::fs::create_dir_all(movies.join("Heat (1995)")).expect("movie folder");
    std::fs::write(movies.join("Heat (1995)/Heat (1995).mkv"), b"").expect("movie");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("root/default"))
            .with_item_store(persistence.clone()),
    );
    for (name, kind, path) in [
        ("Collections", CollectionTypeOptions::boxsets, &collections),
        ("Movies", CollectionTypeOptions::movies, &movies),
    ] {
        vf.add_virtual_folder(
            name,
            Some(kind),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: path.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
    }
    let inside = Uuid::new_v4();
    let adopted = Uuid::new_v4();
    let outside = Uuid::new_v4();
    let sibling = Uuid::new_v4();
    let box_set = |id: Uuid, name: &str, path: String| BaseItemEntity {
        id: guid_to_db(id),
        type_: BOX_SET.to_owned(),
        name: Some(name.to_owned()),
        path: Some(path),
        is_folder: true,
        ..BaseItemEntity::default()
    };
    persistence
        .save_items(&[
            box_set(
                adopted,
                "Adopted",
                "%AppDataPath%/collections/Adopted [boxset]".to_owned(),
            ),
            box_set(
                sibling,
                "Sibling",
                tmp.path()
                    .join("data/collections2/Sibling [boxset]")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ])
        .await
        .expect("more box sets");
    persistence
        .save_items(&[
            BaseItemEntity {
                id: guid_to_db(inside),
                type_: BOX_SET.to_owned(),
                name: Some("Heist".to_owned()),
                path: Some(
                    collections
                        .join("Heist [boxset]")
                        .to_string_lossy()
                        .into_owned(),
                ),
                is_folder: true,
                ..BaseItemEntity::default()
            },
            BaseItemEntity {
                id: guid_to_db(outside),
                type_: BOX_SET.to_owned(),
                name: Some("Elsewhere".to_owned()),
                path: Some(
                    movies
                        .join("Elsewhere [boxset]")
                        .to_string_lossy()
                        .into_owned(),
                ),
                is_folder: true,
                ..BaseItemEntity::default()
            },
        ])
        .await
        .expect("box sets");
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let refreshes = Arc::new(RecordingRefreshes::default());
    let scanner = LibraryScanner::new(
        Arc::clone(&vf),
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(items)
    .with_metadata_savers(refreshes.clone())
    .with_virtual_paths(
        ferrofin_core::virtual_paths::VirtualPathExpander::from_paths(Arc::new(
            ferrofin_core::FerrofinServerApplicationPaths::new(
                tmp.path(),
                tmp.path().join("log"),
                tmp.path().join("config"),
                tmp.path().join("cache"),
                tmp.path().join("web"),
            ),
        )),
    );
    Lab {
        tmp,
        scanner,
        refreshes,
        vf,
        collections,
        inside,
        adopted,
    }
}

/// `ids`, sorted.
fn sorted<const N: usize>(mut ids: [Uuid; N]) -> [Uuid; N] {
    ids.sort();
    ids
}

/// The ids refreshed so far, sorted (the pass refreshes in listing order).
fn refreshed_ids(lab: &Lab) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = lab
        .refreshes
        .refreshed
        .lock()
        .expect("log")
        .iter()
        .map(|(id, ..)| *id)
        .collect();
    ids.sort();
    ids
}

/// `ValidateChildrenInternal2` with `refreshChildMetadata`: a scan of every
/// library refreshes the Collections library's box set with the scan's
/// options — the defaults for a plain scan, the requested modes for a
/// dashboard "Refresh metadata" — and only the box sets in its locations.
#[rstest::rstest]
#[case::plain_scan(MetadataRefreshOptions::default())]
#[case::replace_all(MetadataRefreshOptions {
    metadata_refresh_mode: MetadataRefreshMode::FullRefresh,
    image_refresh_mode: MetadataRefreshMode::FullRefresh,
    replace_all_metadata: true,
    ..MetadataRefreshOptions::default()
})]
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_refreshes_the_collections_librarys_box_sets(
    #[case] options: MetadataRefreshOptions,
) {
    let lab = lab().await;
    lab.scanner.scan_with(None, &options).await.expect("scan");
    assert_eq!(refreshed_ids(&lab), sorted([lab.inside, lab.adopted]));
    assert!(
        lab.refreshes
            .refreshed
            .lock()
            .expect("log")
            .iter()
            .all(|(_, metadata, images)| (*metadata, *images)
                == (options.metadata_refresh_mode, options.image_refresh_mode)),
        "each box set refreshes with the scan's options"
    );
}

/// One box set whose refresh fails does not stop the others'.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_box_set_refresh_does_not_stop_the_others() {
    let lab = lab().await;
    *lab.refreshes.fail.lock().expect("fail") = Some(lab.inside);
    lab.scanner
        .scan_with(None, &MetadataRefreshOptions::default())
        .await
        .expect("scan");
    assert_eq!(refreshed_ids(&lab), sorted([lab.inside, lab.adopted]));
}

/// A scan of another library alone refreshes no box set.
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_of_another_library_refreshes_no_box_set() {
    let lab = lab().await;
    let movies = lab
        .vf
        .get_virtual_folders()
        .await
        .expect("libraries")
        .into_iter()
        .find(|folder| folder.name.as_deref() == Some("Movies"))
        .and_then(|folder| {
            folder
                .item_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
        })
        .expect("the movies library");
    lab.scanner
        .scan_with(Some(movies), &MetadataRefreshOptions::default())
        .await
        .expect("scan");
    assert!(lab.refreshes.refreshed.lock().expect("log").is_empty());
}

/// gl-796: a path-scoped scan under the Collections library's location — a
/// folder refresh there, or a change the library monitor reports in it —
/// refreshes the box sets in its scope, as upstream's
/// `ValidateChildrenInternal2` does for whichever folder it validates. A box
/// set is in scope when it sits under a scanned path or holds one.
#[rstest::rstest]
#[case::the_library_folder("", true)]
#[case::one_box_set("Heist [boxset]", false)]
#[case::a_file_in_a_box_set("Heist [boxset]/collection.xml", false)]
#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_scan_refreshes_the_box_sets_in_its_scope(
    #[case] below: &str,
    #[case] all: bool,
    #[values(false, true)] reported: bool,
) {
    let lab = lab().await;
    let path = lab.collections.join(below).to_string_lossy().into_owned();
    let options = MetadataRefreshOptions::default();
    let target = if reported {
        ScanTarget::Changed(vec![path])
    } else {
        ScanTarget::Paths(vec![path])
    };
    lab.scanner
        .scan_target(
            &target,
            ScanRun::new(&options, &options, &ScanCancel::new()),
        )
        .await
        .expect("scan");
    let expected = if all {
        sorted([lab.inside, lab.adopted]).to_vec()
    } else {
        vec![lab.inside]
    };
    assert_eq!(refreshed_ids(&lab), expected);
}

/// A scoped scan of another library refreshes no box set, not even one stored
/// under the scanned folder, and a cancelled scoped scan runs no pass.
#[rstest::rstest]
#[case::another_library(false)]
#[case::cancelled(true)]
#[tokio::test(flavor = "multi_thread")]
async fn a_scoped_scan_elsewhere_or_cancelled_refreshes_no_box_set(#[case] cancelled: bool) {
    let lab = lab().await;
    let options = MetadataRefreshOptions::default();
    let cancel = ScanCancel::new();
    let path = if cancelled {
        cancel.cancel();
        lab.collections.clone()
    } else {
        // "Elsewhere [boxset]" sits in the movies folder, outside every
        // boxsets library.
        lab.tmp.path().join("movies")
    };
    lab.scanner
        .scan_target(
            &ScanTarget::Paths(vec![path.to_string_lossy().into_owned()]),
            ScanRun::new(&options, &options, &cancel),
        )
        .await
        .expect("scan");
    assert!(lab.refreshes.refreshed.lock().expect("log").is_empty());
}

/// A single item's refresh (`ScanTarget::Items`) is the box set's own refresh
/// already: it runs no box-set pass of its own on top.
#[tokio::test(flavor = "multi_thread")]
async fn an_item_refresh_runs_no_box_set_pass() {
    let lab = lab().await;
    let options = MetadataRefreshOptions::default();
    lab.scanner
        .scan_target(
            &ScanTarget::Items(vec![
                lab.collections
                    .join("Heist [boxset]")
                    .to_string_lossy()
                    .into_owned(),
            ]),
            ScanRun::new(&options, &options, &ScanCancel::new()),
        )
        .await
        .expect("scan");
    assert!(lab.refreshes.refreshed.lock().expect("log").is_empty());
}

/// The shape `EnsureLibraryFolder` leaves survives what a library scan does to
/// it: the library's `Folder` stays a child of the root, and the box sets
/// under it stay, an empty directory walk not being a deletion. A box set with
/// no folder of its own, as an older Ferrofin made them, stays too.
#[allow(clippy::too_many_lines)] // one fixture: the real library, scanner and manager
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_keeps_the_library_and_its_box_sets() {
    use ferrofin_core::item_type_lookup::IdDerivation;
    use ferrofin_core::{AggregateFolderStore, FerrofinCollectionManager};
    use ferrofin_traits::collections::{CollectionCreationOptions, CollectionManager as _};

    let tmp = tempfile::tempdir().expect("tempdir");
    let program = tmp.path().to_string_lossy().into_owned();
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    db.meta_set("item_id_derivation", "jellyfin-10.11.8")
        .await
        .expect("mode");
    let mode = IdDerivation::from_meta(Some("jellyfin-10.11.8"), Some(program));
    let paths = Arc::new(ferrofin_core::FerrofinServerApplicationPaths::new(
        tmp.path(),
        tmp.path().join("log"),
        tmp.path().join("config"),
        tmp.path().join("cache"),
        tmp.path().join("web"),
    ));
    let expander =
        ferrofin_core::virtual_paths::VirtualPathExpander::from_paths(Arc::clone(&paths));
    let ids = AggregateFolderStore::new(
        db.clone(),
        mode.clone(),
        tmp.path().join("root"),
        tmp.path().join("data"),
    )
    .ensure()
    .await
    .expect("roots");
    let persistence = Arc::new(
        FerrofinItemPersistenceService::new(db.clone()).with_virtual_paths(expander.clone()),
    );
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("root/default"))
            .with_item_store(persistence.clone())
            .with_id_derivation(mode.clone())
            .with_virtual_paths(expander.clone()),
    );
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let scanner = LibraryScanner::new(
        Arc::clone(&vf),
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence.clone(),
    )
    .with_items(Arc::clone(&items))
    .with_id_derivation(mode)
    .with_root_folder(ids.aggregate)
    .with_virtual_paths(expander.clone());
    let library = Arc::new(ferrofin_core::FerrofinLibraryManager::new(
        Arc::clone(&items),
        Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
        persistence.clone(),
        Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
    ));
    let manager = FerrofinCollectionManager::new(
        db.clone(),
        library,
        Arc::new(ferrofin_core::FerrofinLinkedChildrenService::new(
            db.clone(),
        )),
        paths,
    )
    .with_virtual_folders(Arc::clone(&vf))
    .with_virtual_paths(expander);
    let box_set = manager
        .create_collection(&CollectionCreationOptions {
            name: "Heist".to_owned(),
            ..CollectionCreationOptions::default()
        })
        .await
        .expect("create");
    let folderless = Uuid::new_v4();
    persistence
        .save_items(&[BaseItemEntity {
            id: guid_to_db(folderless),
            type_: BOX_SET.to_owned(),
            name: Some("Old".to_owned()),
            parent_id: box_set.parent_id.clone(),
            top_parent_id: box_set.parent_id.clone(),
            is_folder: true,
            ..BaseItemEntity::default()
        }])
        .await
        .expect("a box set with no folder");

    for _ in 0..2 {
        scanner
            .scan_with(None, &MetadataRefreshOptions::default())
            .await
            .expect("scan");
    }

    for id in [Uuid::parse_str(&box_set.id).expect("id"), folderless] {
        let row = items.retrieve_item(id).await.expect("read").expect("kept");
        assert_eq!(row.parent_id, box_set.parent_id);
    }
    let folder = box_set.parent_id.as_deref().expect("parent");
    let folder = items
        .retrieve_item(Uuid::parse_str(folder).expect("id"))
        .await
        .expect("read")
        .expect("the folder stays");
    assert_eq!(folder.parent_id, Some(guid_to_db(ids.aggregate)));
}
