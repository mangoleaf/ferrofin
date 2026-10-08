//! A library location is the `Folder` row Jellyfin keeps for it, and what it
//! holds hangs off it (owner decision D1): the shape every database gets, so
//! a native one reads, prunes and stores like an adopted one.

use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{IdDerivation, ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    AggregateFolderStore, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinVirtualFolderManager, LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use uuid::Uuid;

struct Library {
    _tmp: tempfile::TempDir,
    db: Database,
    scanner: LibraryScanner,
    vf: Arc<dyn VirtualFolderManager>,
    items: Arc<dyn ItemRepository>,
    media: std::path::PathBuf,
    cf: Uuid,
    aggregate: Uuid,
}

impl Library {
    async fn new(files: &[&str]) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let media = tmp.path().join("movies");
        for file in files {
            let path = media.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, b"").expect("write");
        }
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let roots = AggregateFolderStore::new(
            db.clone(),
            IdDerivation::Jellyfin {
                program_data_path: None,
            },
            tmp.path().join("root"),
            tmp.path().join("data"),
        )
        .ensure()
        .await
        .expect("roots");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("root/default"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let cf = vf.get_virtual_folders().await.expect("folders")[0]
            .item_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .expect("collection folder id");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner = LibraryScanner::new(
            Arc::clone(&vf),
            Arc::new(FerrofinFileSystem::new()),
            persistence,
        )
        .with_items(Arc::clone(&items))
        .with_root_folder(roots.aggregate);
        Self {
            _tmp: tmp,
            db,
            scanner,
            vf,
            items,
            media,
            cf,
            aggregate: roots.aggregate,
        }
    }

    fn location(&self) -> Uuid {
        derive_item_id(BaseItemKind::Folder, &self.media.to_string_lossy()).expect("id")
    }

    async fn row(&self, id: Uuid) -> Option<BaseItemEntity> {
        self.items.retrieve_item(id).await.expect("read")
    }

    async fn ancestors(&self, id: Uuid) -> Vec<String> {
        let mut ids: Vec<String> =
            sqlx::query_scalar(r#"SELECT "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_all(self.db.pool())
                .await
                .expect("ancestors");
        ids.sort();
        ids
    }
}

/// The location is a `Folder` under the aggregate root, its own top parent;
/// the collection folder names it in `PhysicalFolderIds` and
/// `PhysicalLocationsList`; a movie's top parent and parent are the location
/// folder, and its ancestors the location folder, the collection folder and
/// the root — the shape a Jellyfin database holds.
#[tokio::test]
async fn a_location_is_a_folder_its_items_hang_off() {
    let lib = Library::new(&["Heat (1995)/Heat (1995).mkv"]).await;
    lib.scanner.scan_all().await.expect("scan");
    let location = lib.location();

    let folder = lib.row(location).await.expect("the location's folder");
    assert!(
        folder.type_.ends_with("Entities.Folder"),
        "{}",
        folder.type_
    );
    assert_eq!(
        folder.path.as_deref(),
        Some(lib.media.to_string_lossy().as_ref())
    );
    assert_eq!(folder.parent_id, Some(guid_to_db(lib.aggregate)));
    assert_eq!(folder.top_parent_id, Some(guid_to_db(location)));
    // Stored as Jellyfin stores a folder: `MediaType` `Unknown`, no
    // `DateModified` (D2).
    assert_eq!(folder.media_type.as_deref(), Some("Unknown"));
    assert_eq!(folder.date_modified, None);

    let cf = lib.row(lib.cf).await.expect("collection folder");
    let data: serde_json::Value =
        serde_json::from_str(cf.data.as_deref().expect("data")).expect("json");
    assert_eq!(
        data["PhysicalFolderIds"],
        serde_json::json!([location.simple().to_string()])
    );
    assert_eq!(
        data["PhysicalLocationsList"][1],
        serde_json::json!(lib.media.to_string_lossy())
    );

    let movie_path = lib.media.join("Heat (1995)/Heat (1995).mkv");
    let movie = derive_item_id(BaseItemKind::Movie, &movie_path.to_string_lossy()).expect("id");
    let row = lib.row(movie).await.expect("movie");
    assert_eq!(row.top_parent_id, Some(guid_to_db(location)));
    assert_eq!(row.parent_id, Some(guid_to_db(location)));
    // `IsMovie` is a Live TV program's flag (`IHasProgramAttributes`): a
    // movie stores false, as Jellyfin's do.
    assert!(!row.is_movie);
    assert_eq!(row.media_type.as_deref(), Some("Video"));
    let mut expected = vec![
        guid_to_db(lib.cf),
        guid_to_db(location),
        guid_to_db(lib.aggregate),
    ];
    expected.sort();
    assert_eq!(lib.ancestors(movie).await, expected);
}

/// The library reads as ever through its collection folder, a rescan finds
/// nothing to change, and a deleted movie is pruned while the location
/// folder stays.
#[tokio::test]
async fn a_library_under_its_location_reads_rescans_and_prunes() {
    let lib = Library::new(&[
        "Heat (1995)/Heat (1995).mkv",
        "Ronin (1998)/Ronin (1998).mkv",
    ])
    .await;
    lib.scanner.scan_all().await.expect("scan");
    let browse = || async {
        lib.items
            .get_item_list(&InternalItemsQuery {
                parent_id: lib.cf,
                recursive: true,
                include_item_types: vec![BaseItemKind::Movie],
                ..Default::default()
            })
            .await
            .expect("browse")
            .len()
    };
    assert_eq!(browse().await, 2);
    // The library's direct children are its locations' children
    // (`CollectionFolder.GetActualChildren`), for a browse and for the
    // `/Years` and filter facets alike.
    let direct = InternalItemsQuery {
        parent_id: lib.cf,
        ..Default::default()
    };
    assert_eq!(
        lib.items
            .get_item_list(&direct)
            .await
            .expect("children")
            .len(),
        2
    );
    let mut years = lib.items.get_distinct_years(&direct).await.expect("years");
    years.sort_unstable();
    assert_eq!(years, [1995, 1998]);
    let again = lib.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        again.created + again.updated + again.removed,
        0,
        "{again:?}"
    );

    std::fs::remove_dir_all(lib.media.join("Ronin (1998)")).expect("rm");
    let pruned = lib.scanner.scan_all().await.expect("prune");
    assert_eq!(pruned.removed, 1, "{pruned:?}");
    assert_eq!(browse().await, 1);
    assert!(
        lib.row(lib.location()).await.is_some(),
        "the location folder stays"
    );
}

/// A location removed from its library takes its folder, and what hangs off
/// it, on the next scan (`AggregateFolder.ValidateChildren`), counted as
/// removed; the library's other location stays. Never while a library lists
/// no location: a shortcut that did not resolve reads that way, and must not
/// read as a removed location.
#[tokio::test]
async fn a_removed_location_takes_its_folder_and_items() {
    let lib = Library::new(&["Heat (1995)/Heat (1995).mkv"]).await;
    let more = lib.media.with_file_name("more");
    std::fs::create_dir_all(more.join("Ronin (1998)")).expect("mkdir");
    std::fs::write(more.join("Ronin (1998)/Ronin (1998).mkv"), b"").expect("write");
    let more_path = more.to_string_lossy().into_owned();
    lib.vf
        .add_media_path(
            "Movies",
            &MediaPathInfo {
                path: more_path.clone(),
            },
        )
        .await
        .expect("add location");
    lib.scanner.scan_all().await.expect("scan");
    let more_folder = derive_item_id(BaseItemKind::Folder, &more_path).expect("id");
    let ronin = derive_item_id(
        BaseItemKind::Movie,
        &more.join("Ronin (1998)/Ronin (1998).mkv").to_string_lossy(),
    )
    .expect("id");
    assert!(lib.row(more_folder).await.is_some());
    assert!(lib.row(ronin).await.is_some());

    lib.vf
        .remove_media_path("Movies", &more_path)
        .await
        .expect("remove location");
    lib.vf
        .add_virtual_folder("Empty", None, &LibraryOptions::default())
        .await
        .expect("add an empty library");
    lib.scanner.scan_all().await.expect("held");
    assert!(
        lib.row(ronin).await.is_some(),
        "held while a library lists no location"
    );
    lib.vf
        .remove_virtual_folder("Empty")
        .await
        .expect("remove the empty library");
    let rescan = lib.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        rescan.removed, 2,
        "the location's folder and its movie: {rescan:?}"
    );
    assert!(
        lib.row(more_folder).await.is_none(),
        "the location's folder goes"
    );
    assert!(lib.row(ronin).await.is_none(), "and what hung off it");
    assert!(
        lib.row(lib.location()).await.is_some(),
        "the other location stays"
    );
}

/// A location inside another location is no child of the root
/// (`NormalizeRootPathList`): it gets no folder of its own.
#[tokio::test]
async fn a_nested_location_has_no_folder_of_its_own() {
    let lib = Library::new(&[
        "Heat (1995)/Heat (1995).mkv",
        "Kids/Up (2009)/Up (2009).mkv",
    ])
    .await;
    let kids = lib.media.join("Kids").to_string_lossy().into_owned();
    lib.vf
        .add_virtual_folder(
            "Kids",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo { path: kids.clone() }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add the nested library");
    lib.scanner.scan_all().await.expect("scan");
    let folder = lib
        .row(derive_item_id(BaseItemKind::Folder, &kids).expect("id"))
        .await;
    assert!(folder.is_none(), "{folder:?}");
    assert!(lib.row(lib.location()).await.is_some());
}
