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
        Self::of_type(files, CollectionTypeOptions::movies).await
    }

    async fn of_type(files: &[&str], collection_type: CollectionTypeOptions) -> Self {
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
            IdDerivation::LegacyLowercase,
            tmp.path().join("root"),
            tmp.path().join("data"),
        )
        .ensure()
        .await
        .expect("roots");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("root/default"))
                .with_item_store(persistence.clone())
                .with_physical_root(db.clone(), roots.aggregate),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(collection_type),
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
        let items: Arc<dyn ItemRepository> = Arc::new(
            FerrofinItemRepository::new(db.clone(), Arc::new(ItemTypeLookup::new()))
                .with_root_ids(roots),
        );
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
    let rescan = lib
        .scanner
        .scan_all()
        .await
        .expect("rescan with an empty library");
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

/// Upgrading must not require a scan (which could refresh edited metadata).
/// Model the old native hierarchy, then remove its library immediately.
#[tokio::test]
async fn removing_a_legacy_library_preserves_media_and_metadata_without_a_scan() {
    let lib = Library::new(&["Heat/Heat.mkv"]).await;
    lib.scanner.scan_all().await.unwrap();
    let movie = derive_item_id(
        BaseItemKind::Movie,
        &lib.media.join("Heat/Heat.mkv").to_string_lossy(),
    )
    .unwrap();
    sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?, "TopParentId" = ?, "Name" = 'Edited title', "Overview" = 'Keep me' WHERE "Id" = ?"#)
        .bind(guid_to_db(lib.cf)).bind(guid_to_db(lib.cf)).bind(guid_to_db(movie))
        .execute(lib.db.writer()).await.unwrap();
    sqlx::query(r#"DELETE FROM "BaseItems" WHERE "Id" = ?"#)
        .bind(guid_to_db(lib.location()))
        .execute(lib.db.writer())
        .await
        .unwrap();
    lib.vf.remove_virtual_folder("Movies").await.unwrap();
    let row = lib
        .row(movie)
        .await
        .expect("media preserved without a scan");
    assert_eq!(row.name.as_deref(), Some("Edited title"));
    assert_eq!(row.overview.as_deref(), Some("Keep me"));
    assert_eq!(row.parent_id, Some(guid_to_db(lib.location())));
    assert_eq!(row.top_parent_id, Some(guid_to_db(lib.location())));
    assert!(lib.row(lib.cf).await.is_none());
    assert!(
        lib.ancestors(movie)
            .await
            .contains(&guid_to_db(lib.aggregate))
    );
}

#[tokio::test]
async fn every_shared_library_stays_in_the_ancestor_closure() {
    let lib = Library::new(&["Heat/Heat.mkv"]).await;
    lib.vf
        .add_virtual_folder(
            "Other",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: lib.media.to_string_lossy().into_owned(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let other = lib
        .vf
        .get_virtual_folders()
        .await
        .unwrap()
        .into_iter()
        .find(|folder| folder.name.as_deref() == Some("Other"))
        .unwrap();
    let other_id = Uuid::parse_str(other.item_id.as_deref().unwrap()).unwrap();
    lib.scanner.scan_all().await.unwrap();
    let movie = derive_item_id(
        BaseItemKind::Movie,
        &lib.media.join("Heat/Heat.mkv").to_string_lossy(),
    )
    .unwrap();
    let ancestors = lib.ancestors(movie).await;
    assert!(ancestors.contains(&guid_to_db(lib.cf)));
    assert!(ancestors.contains(&guid_to_db(other_id)));
    lib.vf.remove_virtual_folder("Other").await.unwrap();
    assert!(lib.row(movie).await.is_some());
}

/// A legacy physical Folder already at the location must become a root child,
/// never its own parent, and its descendants retain their existing ids.
#[tokio::test]
async fn an_existing_location_folder_is_reparented_without_a_cycle() {
    let lib = Library::new(&["Heat/Heat.mkv"]).await;
    lib.scanner.scan_all().await.unwrap();
    sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?, "TopParentId" = ? WHERE "Id" = ?"#)
        .bind(guid_to_db(lib.cf))
        .bind(guid_to_db(lib.cf))
        .bind(guid_to_db(lib.location()))
        .execute(lib.db.writer())
        .await
        .unwrap();
    lib.vf.remove_virtual_folder("Movies").await.unwrap();
    let folder = lib.row(lib.location()).await.unwrap();
    assert_eq!(folder.parent_id, Some(guid_to_db(lib.aggregate)));
    assert_eq!(folder.top_parent_id, Some(guid_to_db(lib.location())));
    assert!(
        !lib.ancestors(lib.location())
            .await
            .contains(&guid_to_db(lib.location()))
    );
    let movie = derive_item_id(
        BaseItemKind::Movie,
        &lib.media.join("Heat/Heat.mkv").to_string_lossy(),
    )
    .unwrap();
    assert!(lib.row(movie).await.is_some());
}

/// A corrupt path fails before deleting either the shortcut directory or media.
/// Other branches converted in this transaction also roll back.
#[tokio::test]
async fn an_unresolvable_legacy_child_rolls_back_the_library_removal() {
    let lib = Library::new(&["Heat/Heat.mkv", "Ronin/Ronin.mkv"]).await;
    lib.scanner.scan_all().await.unwrap();
    let ids: Vec<_> = ["Heat/Heat.mkv", "Ronin/Ronin.mkv"]
        .iter()
        .map(|path| {
            derive_item_id(BaseItemKind::Movie, &lib.media.join(path).to_string_lossy()).unwrap()
        })
        .collect();
    for id in &ids {
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?, "TopParentId" = ? WHERE "Id" = ?"#)
            .bind(guid_to_db(lib.cf))
            .bind(guid_to_db(lib.cf))
            .bind(guid_to_db(*id))
            .execute(lib.db.writer())
            .await
            .unwrap();
    }
    sqlx::query(r#"UPDATE "BaseItems" SET "Path" = '/unrelated/file.mkv' WHERE "Id" = ?"#)
        .bind(guid_to_db(ids[1]))
        .execute(lib.db.writer())
        .await
        .unwrap();
    assert!(lib.vf.remove_virtual_folder("Movies").await.is_err());
    assert!(lib.row(lib.cf).await.is_some());
    assert_eq!(lib.vf.get_virtual_folders().await.unwrap().len(), 1);
    for id in ids {
        assert_eq!(
            lib.row(id).await.unwrap().parent_id,
            Some(guid_to_db(lib.cf))
        );
    }
}

#[tokio::test]
async fn the_first_path_refresh_provisions_its_physical_parent() {
    let lib = Library::new(&["Heat/Heat.mkv"]).await;
    let path = lib
        .media
        .join("Heat/Heat.mkv")
        .to_string_lossy()
        .into_owned();
    lib.scanner
        .scan_paths(std::slice::from_ref(&path))
        .await
        .unwrap();
    let movie = derive_item_id(BaseItemKind::Movie, &path).unwrap();
    assert_eq!(
        lib.row(movie).await.unwrap().parent_id,
        Some(guid_to_db(lib.location()))
    );
    assert_eq!(
        lib.row(lib.location()).await.unwrap().parent_id,
        Some(guid_to_db(lib.aggregate))
    );
}

#[tokio::test]
async fn removing_the_last_library_prunes_its_media_on_the_next_scan() {
    let lib = Library::new(&["Heat/Heat.mkv"]).await;
    lib.scanner.scan_all().await.unwrap();
    let movie = derive_item_id(
        BaseItemKind::Movie,
        &lib.media.join("Heat/Heat.mkv").to_string_lossy(),
    )
    .unwrap();
    lib.vf.remove_virtual_folder("Movies").await.unwrap();
    assert!(
        lib.row(movie).await.is_some(),
        "refresh false preserves the physical row"
    );
    lib.scanner.scan_all().await.unwrap();
    assert!(lib.row(movie).await.is_none());
    assert!(lib.row(lib.location()).await.is_none());
    assert!(lib.media.join("Heat/Heat.mkv").is_file());
}

/// Disabling photos invalidates the previously resolved photo rows as well as
/// stopping discovery. It never removes source files or video catalog entries.
#[tokio::test]
async fn a_photo_toggle_removes_existing_photos_and_rediscovers_them_when_enabled() {
    let lib = Library::of_type(
        &["Album/A.jpg", "B.jpg", "clip.mkv"],
        CollectionTypeOptions::homevideos,
    )
    .await;
    let photo = derive_item_id(
        BaseItemKind::Photo,
        &lib.media.join("Album/A.jpg").to_string_lossy(),
    )
    .unwrap();
    let album = derive_item_id(
        BaseItemKind::PhotoAlbum,
        &lib.media.join("Album").to_string_lossy(),
    )
    .unwrap();
    let movie = derive_item_id(
        BaseItemKind::Movie,
        &lib.media.join("clip.mkv").to_string_lossy(),
    )
    .unwrap();
    lib.scanner.scan_all().await.unwrap();
    assert!(lib.row(photo).await.is_some());
    assert!(lib.row(album).await.is_some());
    let mut options = lib
        .vf
        .get_virtual_folders()
        .await
        .unwrap()
        .remove(0)
        .library_options
        .unwrap();
    options.enable_photos = false;
    lib.vf
        .update_library_options("Movies", &options)
        .await
        .unwrap();
    std::fs::write(lib.media.join("C.jpg"), b"").unwrap();
    let extra = derive_item_id(
        BaseItemKind::Photo,
        &lib.media.join("C.jpg").to_string_lossy(),
    )
    .unwrap();
    lib.scanner.scan_all().await.unwrap();
    assert!(lib.row(photo).await.is_none());
    assert!(lib.row(album).await.is_none());
    assert!(lib.row(extra).await.is_none());
    assert!(lib.row(movie).await.is_some());
    for path in ["Album/A.jpg", "B.jpg", "C.jpg", "clip.mkv"] {
        assert!(lib.media.join(path).is_file());
    }
    options.enable_photos = true;
    lib.vf
        .update_library_options("Movies", &options)
        .await
        .unwrap();
    lib.scanner.scan_all().await.unwrap();
    assert!(lib.row(photo).await.is_some());
    assert!(lib.row(album).await.is_some());
    assert!(lib.row(extra).await.is_some());
    assert!(lib.row(movie).await.is_some());
}

#[tokio::test]
async fn disabling_photos_does_not_prune_an_unreachable_location() {
    let lib = Library::of_type(&["Album/A.jpg"], CollectionTypeOptions::homevideos).await;
    lib.scanner.scan_all().await.unwrap();
    let photo = derive_item_id(
        BaseItemKind::Photo,
        &lib.media.join("Album/A.jpg").to_string_lossy(),
    )
    .unwrap();
    let mut options = lib
        .vf
        .get_virtual_folders()
        .await
        .unwrap()
        .remove(0)
        .library_options
        .unwrap();
    options.enable_photos = false;
    lib.vf
        .update_library_options("Movies", &options)
        .await
        .unwrap();
    let offline = lib.media.with_file_name("offline");
    std::fs::rename(&lib.media, &offline).unwrap();
    lib.scanner.scan_all().await.unwrap();
    assert!(lib.row(photo).await.is_some());
    std::fs::rename(&offline, &lib.media).unwrap();
    lib.scanner.scan_all().await.unwrap();
    assert!(lib.row(photo).await.is_none());
    assert!(lib.media.join("Album/A.jpg").is_file());
}
