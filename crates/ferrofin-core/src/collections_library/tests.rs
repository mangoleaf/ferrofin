//! The Collections library as `EnsureLibraryFolder` makes it, and the boot
//! repair that retires the bare row an older Ferrofin made instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrofin_db::Database;
use ferrofin_db::enums::PreferenceKind;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::collections::{CollectionCreationOptions, CollectionManager};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::system::ServerApplicationPaths;
use uuid::Uuid;

use crate::adoption_repairs::{MergedCollectionsLibrary, merge_legacy_collections_library};
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_type_lookup::{IdDerivation, stored_type_name};
use crate::library_structure_repository::{ancestors_of, fail_item_deletes, user_data_items};
use crate::linked_children_service::FerrofinLinkedChildrenService;
use crate::test_support::{
    fetch_item, fetch_item_opt, image_info, item_repository_over, library_manager_over, save_item,
    seed_child_item, seed_folder_with_data, seed_images, seed_user, seed_user_data, set_item_path,
    test_db,
};
use crate::user_entity_ext::{get_preference, set_preference};
use crate::virtual_paths::VirtualPathExpander;
use crate::{AggregateFolderStore, FerrofinCollectionManager, FerrofinVirtualFolderManager};

/// `GetNewItemIdInternal` for `Folder` + `data\collections`, read off a live
/// Jellyfin 12.2: the parent of every box set.
const FOLDER_ID: &str = "8679d10569ec12981200c4116da3e90b";
/// …for `CollectionFolder` + `root\default\Collections`: Jellyfin's library.
const LIBRARY_ID: &str = "9d7ad6afe9afa2dab1a2f6e00ad28fa6";
/// …for `CollectionFolder` + `data\collections`: the row Ferrofin used to make.
const LEGACY_ID: &str = "6f929a39bd27711ce6208fb0aef66e5b";

fn id(simple: &str) -> Uuid {
    Uuid::parse_str(simple).expect("a uuid")
}

struct Lab {
    tmp: tempfile::TempDir,
    db: Database,
    paths: Arc<crate::FerrofinServerApplicationPaths>,
    mode: IdDerivation,
    expander: VirtualPathExpander,
    vf: Arc<FerrofinVirtualFolderManager>,
    aggregate: Uuid,
    user_root: Uuid,
}

impl Lab {
    /// A server with its roots made and no Collections library.
    async fn new() -> Self {
        let db = test_db().await;
        db.meta_set("item_id_derivation", "jellyfin-10.11.8")
            .await
            .expect("mode");
        Self::over(db, "jellyfin-10.11.8").await
    }

    async fn over(db: Database, stored_mode: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
            dir,
            dir.join("log"),
            dir.join("config"),
            dir.join("cache"),
            dir.join("web"),
        ));
        let mode = IdDerivation::from_meta(Some(stored_mode), Some(paths.program_data_path()));
        let ids = AggregateFolderStore::new(
            db.clone(),
            mode.clone(),
            paths.root_folder_path(),
            paths.data_path(),
        )
        .ensure()
        .await
        .expect("roots");
        let expander = VirtualPathExpander::from_paths(Arc::clone(&paths));
        let persistence = Arc::new(
            FerrofinItemPersistenceService::new(db.clone()).with_virtual_paths(expander.clone()),
        );
        crate::UserRootFolderStore::new(
            persistence.clone(),
            mode.clone(),
            paths.default_user_views_path(),
        )
        .ensure()
        .await
        .expect("user root");
        let vf = Arc::new(
            FerrofinVirtualFolderManager::new(paths.default_user_views_path())
                .with_item_store(persistence)
                .with_items(item_repository_over(db.clone()))
                .with_physical_root(db.clone(), ids.aggregate)
                .with_user_preferences(db.clone(), Arc::default())
                .with_id_derivation(mode.clone())
                .with_virtual_paths(expander.clone()),
        );
        Self {
            tmp,
            db,
            paths,
            mode,
            expander,
            vf,
            aggregate: ids.aggregate,
            user_root: ids.user_root,
        }
    }

    fn manager(&self) -> FerrofinCollectionManager {
        FerrofinCollectionManager::new(
            self.db.clone(),
            library_manager_over(self.db.clone()),
            Arc::new(FerrofinLinkedChildrenService::new(self.db.clone())),
            self.paths.clone(),
        )
        .with_virtual_folders(self.vf.clone())
        .with_virtual_paths(self.expander.clone())
    }

    fn collections(&self) -> PathBuf {
        PathBuf::from(self.paths.data_path()).join("collections")
    }

    fn collections_str(&self) -> String {
        self.collections().to_string_lossy().into_owned()
    }

    fn library_dir(&self, name: &str) -> PathBuf {
        PathBuf::from(self.paths.default_user_views_path()).join(name)
    }

    /// The row and shape `ensure_container` left, under `legacy`.
    async fn seed_legacy(&self, legacy: Uuid) {
        std::fs::create_dir_all(self.collections()).expect("directory");
        seed_folder_with_data(
            &self.db,
            legacy,
            BaseItemKind::CollectionFolder,
            "Collections",
            r#"{"CollectionType":"boxsets"}"#,
            Some(self.user_root),
        )
        .await;
        set_item_path(&self.db, legacy, &self.collections_str()).await;
    }

    /// A box set as an older Ferrofin created it under `parent`: parent and
    /// top parent, a path, and no ancestor rows.
    async fn seed_box_set(&self, name: &str, parent: Uuid) -> Uuid {
        let box_set = Uuid::new_v4();
        seed_child_item(&self.db, box_set, BaseItemKind::BoxSet, name, parent).await;
        let mut row = fetch_item(&self.db, box_set).await;
        row.top_parent_id = Some(guid_to_db(parent));
        row.path = Some(format!("{}/{name} [boxset]", self.collections_str()));
        save_item(&self.db, &row).await;
        box_set
    }

    /// Jellyfin's Collections library on disk, named `name`, its shortcut in
    /// the token form, and its `Folder` row as Jellyfin stores it.
    async fn seed_jellyfin_library(&self, name: &str) {
        let dir = self.library_dir(name);
        std::fs::create_dir_all(&dir).expect("library directory");
        std::fs::create_dir_all(self.collections()).expect("directory");
        std::fs::write(dir.join("boxsets.collection"), b"").expect("marker");
        std::fs::write(dir.join("collections.mblink"), "%AppDataPath%/collections")
            .expect("shortcut");
        seed_folder_with_data(
            &self.db,
            id(FOLDER_ID),
            BaseItemKind::Folder,
            "collections",
            "{}",
            Some(self.aggregate),
        )
        .await;
        set_item_path(&self.db, id(FOLDER_ID), "%AppDataPath%/collections").await;
    }

    async fn merge(
        &self,
    ) -> Result<Option<MergedCollectionsLibrary>, ferrofin_traits::error::ServiceError> {
        merge_legacy_collections_library(
            &self.db,
            &self.vf,
            &self.mode,
            (&self.paths.data_path(), &self.paths.root_folder_path()),
            &self.expander,
        )
        .await
    }

    async fn create(&self, name: &str) -> ferrofin_db::entities::base_items::BaseItemEntity {
        self.manager()
            .create_collection(&CollectionCreationOptions {
                name: name.to_owned(),
                ..CollectionCreationOptions::default()
            })
            .await
            .expect("create")
    }

    async fn library_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .vf
            .get_virtual_folders()
            .await
            .expect("libraries")
            .into_iter()
            .filter_map(|folder| folder.name)
            .collect();
        names.sort();
        names
    }

    /// `box_set` hangs off `folder` with the ancestors an item of that library
    /// has, and off nothing older.
    async fn assert_under(&self, box_set: Uuid, folder: Uuid, library: Uuid) {
        let row = fetch_item(&self.db, box_set).await;
        assert_eq!(row.parent_id, Some(guid_to_db(folder)), "parent");
        assert_eq!(row.top_parent_id, Some(guid_to_db(folder)), "top parent");
        let mut expected = vec![
            guid_to_db(folder),
            guid_to_db(self.aggregate),
            guid_to_db(library),
        ];
        expected.sort();
        assert_eq!(ancestors_of(&self.db, box_set).await, expected, "ancestors");
    }
}

/// `CreateCollectionAsync` on a server with no library: `AddVirtualFolder`
/// with upstream's arguments, and the box set under the location's `Folder`,
/// with the ids a live Jellyfin gives the same directories.
#[tokio::test]
async fn the_first_box_set_makes_a_real_library() {
    let lab = Lab::new().await;
    let box_set = lab.create("Heist").await;

    let dir = lab.library_dir("Collections");
    assert!(dir.join("boxsets.collection").is_file());
    assert_eq!(
        std::fs::read_to_string(dir.join("collections.mblink")).expect("shortcut"),
        "%AppDataPath%/collections"
    );
    let folders = lab.vf.get_virtual_folders().await.expect("libraries");
    assert_eq!(folders.len(), 1, "{folders:?}");
    let library = &folders[0];
    assert_eq!(library.name.as_deref(), Some("Collections"));
    assert_eq!(library.item_id.as_deref(), Some(LIBRARY_ID));
    assert_eq!(
        library.collection_type,
        Some(CollectionTypeOptions::boxsets)
    );
    assert_eq!(library.locations, [lab.collections_str()]);
    let options = library.library_options.as_ref().expect("options");
    assert!(!options.enable_realtime_monitor);
    assert!(options.save_local_metadata);

    assert_eq!(box_set.parent_id, Some(guid_to_db(id(FOLDER_ID))));
    let folder = fetch_item(&lab.db, id(FOLDER_ID)).await;
    assert_eq!(
        folder.type_,
        stored_type_name(BaseItemKind::Folder).unwrap_or_default()
    );
    assert_eq!(folder.parent_id, Some(guid_to_db(lab.aggregate)));
    assert_eq!(folder.top_parent_id, Some(guid_to_db(id(FOLDER_ID))));
    assert_eq!(folder.path.as_deref(), Some("%AppDataPath%/collections"));
    assert_eq!(folder.name.as_deref(), Some("collections"));
    assert!(folder.date_created.is_some() && folder.date_modified.is_some());
    assert!(fetch_item_opt(&lab.db, id(LEGACY_ID)).await.is_none());
    let box_set = Uuid::parse_str(&box_set.id).expect("id");
    lab.assert_under(box_set, id(FOLDER_ID), id(LIBRARY_ID))
        .await;

    let library_row = fetch_item(&lab.db, id(LIBRARY_ID)).await;
    let data = library_row.data.as_deref().expect("data");
    assert!(data.contains(r#""CollectionType":"boxsets""#), "{data}");
    assert!(data.contains(FOLDER_ID), "PhysicalFolderIds: {data}");
    assert_eq!(library_row.parent_id, Some(guid_to_db(lab.user_root)));
}

/// `FindFolders` finds the library's folder, so later box sets add no library,
/// and two first creates at once make one.
#[tokio::test]
async fn the_library_is_made_once() {
    let lab = Lab::new().await;
    let manager = lab.manager();
    let create = |name: &'static str| {
        let manager = &manager;
        async move {
            manager
                .create_collection(&CollectionCreationOptions {
                    name: name.to_owned(),
                    ..CollectionCreationOptions::default()
                })
                .await
        }
    };
    let (first, second) = tokio::join!(create("One"), create("Two"));
    assert_eq!(
        first.expect("one").parent_id,
        second.expect("two").parent_id
    );
    lab.create("Three").await;
    assert_eq!(lab.library_names().await, ["Collections"]);
}

/// A lookup makes nothing: `GetCollectionsFolder(false)` on a server with no
/// library is `null`, and neither folder nor library appears.
#[tokio::test]
async fn a_lookup_makes_nothing() {
    let lab = Lab::new().await;
    let found = lab.manager().get_collections_folder(false).await;
    assert!(found.expect("lookup").is_none());
    assert_eq!(lab.library_names().await, Vec::<String>::new());
    assert!(!lab.collections().exists());
}

/// An adopted Jellyfin database keeps its library, whatever its language calls
/// it, and its `Folder` row in the token form; nothing is provisioned.
#[tokio::test]
async fn an_adopted_library_is_reused() {
    let lab = Lab::new().await;
    lab.seed_jellyfin_library("Sammlungen").await;
    let box_set = lab.create("Heist").await;
    assert_eq!(box_set.parent_id, Some(guid_to_db(id(FOLDER_ID))));
    assert_eq!(
        box_set.path,
        Some(format!("{}/Heist [boxset]", lab.collections_str()))
    );
    assert_eq!(lab.library_names().await, ["Sammlungen"]);
    let folder = fetch_item(&lab.db, id(FOLDER_ID)).await;
    assert_eq!(folder.path.as_deref(), Some("%AppDataPath%/collections"));
}

/// `FindFolders`' `ContainsSubPath(i.Path, path)`: a library location above
/// `{data}/collections` is the root's only child for that tree, so it is the
/// parent, the box set goes in it, and no library is added.
#[tokio::test]
async fn a_location_above_the_directory_is_the_folder() {
    let lab = Lab::new().await;
    let above = Uuid::new_v4();
    let data = lab.paths.data_path();
    seed_folder_with_data(
        &lab.db,
        above,
        BaseItemKind::Folder,
        "data",
        "{}",
        Some(lab.aggregate),
    )
    .await;
    set_item_path(&lab.db, above, &data).await;

    let box_set = lab.create("Heist").await;
    assert_eq!(box_set.parent_id, Some(guid_to_db(above)));
    assert_eq!(box_set.path, Some(format!("{data}/Heist [boxset]")));
    assert_eq!(lab.library_names().await, Vec::<String>::new());
}

/// The boot repair on what every older Ferrofin database holds: the box sets
/// land on the `Folder` row, not on the library; users, their user data and the
/// row's image follow to the library; and a second boot finds nothing to do.
#[tokio::test]
async fn the_repair_retires_the_bare_row() {
    let lab = Lab::new().await;
    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    let (first, second) = (
        lab.seed_box_set("Heist", legacy).await,
        lab.seed_box_set("Alien", legacy).await,
    );
    let user = seed_user(&lab.db, Uuid::from_u128(0x66)).await;
    let other = Uuid::from_u128(0xABC);
    let d = |id: Uuid| id.hyphenated().to_string();
    for kind in [
        PreferenceKind::EnabledFolders,
        PreferenceKind::BlockedMediaFolders,
        PreferenceKind::LatestItemExcludes,
    ] {
        set_preference(&lab.db, &user.id, kind, &[d(other), d(legacy)])
            .await
            .expect("preference");
    }
    let user_id = Uuid::parse_str(&user.id).expect("id");
    seed_user_data(&lab.db, user_id, legacy, true, None).await;
    seed_images(
        &lab.db,
        legacy,
        &[image_info(ImageType::Primary, "/art/collections.jpg", None)],
    )
    .await;

    let merged = lab.merge().await.expect("repair").expect("work to do");

    assert_eq!(merged.retired, legacy);
    assert_eq!(merged.library, id(LIBRARY_ID));
    assert_eq!(merged.folder, id(FOLDER_ID));
    assert_eq!(merged.moved, 2);
    assert!(fetch_item_opt(&lab.db, legacy).await.is_none());
    assert_eq!(lab.library_names().await, ["Collections"]);
    for box_set in [first, second] {
        lab.assert_under(box_set, id(FOLDER_ID), id(LIBRARY_ID))
            .await;
    }
    for kind in [
        PreferenceKind::EnabledFolders,
        PreferenceKind::BlockedMediaFolders,
        PreferenceKind::LatestItemExcludes,
    ] {
        assert_eq!(
            get_preference(lab.db.pool(), &user.id, kind)
                .await
                .expect("preference"),
            [d(other), d(id(LIBRARY_ID))],
            "{kind:?}"
        );
    }
    assert_eq!(
        user_data_items(&lab.db, user_id).await,
        [guid_to_db(id(LIBRARY_ID))]
    );
    let images = item_repository_over(lab.db.clone())
        .get_image_infos(id(LIBRARY_ID))
        .await
        .expect("images");
    assert_eq!(images.len(), 1, "{images:?}");

    assert_eq!(lab.merge().await.expect("second boot"), None);
    assert_eq!(lab.library_names().await, ["Collections"]);
    assert_eq!(
        lab.vf
            .prune_orphan_collection_folders()
            .await
            .expect("prune"),
        0,
        "the library's directory exists"
    );
    // A create finds what the repair made.
    let made = lab.create("Later").await;
    assert_eq!(made.parent_id, Some(guid_to_db(id(FOLDER_ID))));
}

/// A database that holds Jellyfin's library and the row an older Ferrofin put
/// beside it: one library stays, in its own language, and the box sets split
/// between the two end up together.
#[tokio::test]
async fn the_repair_merges_a_duplicate_into_the_adopted_library() {
    let lab = Lab::new().await;
    lab.seed_jellyfin_library("Sammlungen").await;
    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    let theirs = lab.seed_box_set("Theirs", id(FOLDER_ID)).await;
    let ours = lab.seed_box_set("Ours", legacy).await;
    let user = seed_user(&lab.db, Uuid::from_u128(0x67)).await;
    let library = lab
        .vf
        .get_virtual_folders()
        .await
        .expect("libraries")
        .first()
        .and_then(|folder| folder.item_id.clone())
        .expect("the library's id");
    let library = id(&library);
    let values = [legacy.simple().to_string(), library.simple().to_string()];
    set_preference(&lab.db, &user.id, PreferenceKind::EnabledFolders, &values)
        .await
        .expect("preference");

    let merged = lab.merge().await.expect("repair").expect("work to do");

    assert_eq!(merged.library, library);
    assert_eq!(merged.moved, 1);
    assert_eq!(lab.library_names().await, ["Sammlungen"]);
    lab.assert_under(ours, id(FOLDER_ID), library).await;
    assert_eq!(
        fetch_item(&lab.db, theirs).await.parent_id,
        Some(guid_to_db(id(FOLDER_ID))),
        "the adopted box set stays"
    );
    assert_eq!(
        get_preference(lab.db.pool(), &user.id, PreferenceKind::EnabledFolders)
            .await
            .expect("preference"),
        [library.simple().to_string()],
        "the two ids become one"
    );
    let folder = fetch_item(&lab.db, id(FOLDER_ID)).await;
    assert_eq!(folder.path.as_deref(), Some("%AppDataPath%/collections"));
}

/// A user's own library called Collections is another library: it is left
/// alone, `AddVirtualFolder` numbers the new one, and users move to that id.
#[tokio::test]
async fn a_library_that_took_the_name_is_left_alone() {
    let lab = Lab::new().await;
    let movies = lab.tmp.path().join("movies");
    std::fs::create_dir_all(&movies).expect("movies");
    lab.vf
        .add_virtual_folder(
            "Collections",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: movies.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add");
    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    let box_set = lab.seed_box_set("Heist", legacy).await;

    let merged = lab.merge().await.expect("repair").expect("work to do");

    assert_eq!(lab.library_names().await, ["Collections", "Collections2"]);
    assert_ne!(merged.library, id(LIBRARY_ID));
    lab.assert_under(box_set, id(FOLDER_ID), merged.library)
        .await;
}

/// A pre-parity database keys everything by the legacy derivation, and the
/// repair follows it: the row is found by its path, the new ids are that
/// derivation's.
#[tokio::test]
async fn the_repair_follows_the_databases_derivation() {
    let lab = Lab::over(test_db().await, "legacy-lowercase").await;
    let legacy = Uuid::new_v4();
    lab.seed_legacy(legacy).await;
    let box_set = lab.seed_box_set("Heist", legacy).await;

    let merged = lab.merge().await.expect("repair").expect("work to do");

    assert_eq!(merged.retired, legacy);
    assert_ne!(merged.folder, id(FOLDER_ID));
    assert_eq!(
        Some(merged.folder),
        crate::item_type_lookup::derive_item_id_with(
            &lab.mode,
            BaseItemKind::Folder,
            &lab.collections_str()
        )
    );
    lab.assert_under(box_set, merged.folder, merged.library)
        .await;
}

/// A data directory that moved leaves the row at its old path, under the id
/// the relative key derives.
#[tokio::test]
async fn the_repair_finds_a_row_left_at_an_old_data_directory() {
    let lab = Lab::new().await;
    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    set_item_path(&lab.db, legacy, "/old/data/collections").await;
    let box_set = lab.seed_box_set("Heist", legacy).await;

    assert!(lab.merge().await.expect("repair").is_some());

    assert!(fetch_item_opt(&lab.db, legacy).await.is_none());
    lab.assert_under(box_set, id(FOLDER_ID), id(LIBRARY_ID))
        .await;
}

/// The library cannot be made (here the libraries root is a file): the boot
/// goes on, the old row and everything on it stay, a box set can still be
/// created, and a later boot that can write finishes the job.
#[tokio::test]
async fn a_repair_that_cannot_make_the_library_changes_nothing() {
    let lab = Lab::new().await;
    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    let box_set = lab.seed_box_set("Heist", legacy).await;
    let user = seed_user(&lab.db, Uuid::from_u128(0x68)).await;
    let enabled = [legacy.hyphenated().to_string()];
    set_preference(&lab.db, &user.id, PreferenceKind::EnabledFolders, &enabled)
        .await
        .expect("preference");
    let root = PathBuf::from(lab.paths.default_user_views_path());
    make_a_file_of(&root);

    assert!(lab.merge().await.is_err(), "the library cannot be made");

    assert_eq!(
        fetch_item(&lab.db, box_set).await.parent_id,
        Some(guid_to_db(legacy))
    );
    assert_eq!(
        get_preference(lab.db.pool(), &user.id, PreferenceKind::EnabledFolders)
            .await
            .expect("preference"),
        enabled
    );
    let made = lab.create("Still works").await;
    assert_eq!(made.parent_id, Some(guid_to_db(legacy)));
    assert_eq!(
        ancestors_of(&lab.db, Uuid::parse_str(&made.id).expect("id")).await,
        Vec::<String>::new()
    );

    std::fs::remove_file(&root).expect("the file");
    std::fs::create_dir_all(&root).expect("the directory");
    let merged = lab.merge().await.expect("repair").expect("work to do");
    assert_eq!(merged.moved, 2, "the box set made meanwhile moves too");
    lab.assert_under(box_set, id(FOLDER_ID), id(LIBRARY_ID))
        .await;
}

fn make_a_file_of(path: &Path) {
    if path.exists() {
        std::fs::remove_dir_all(path).expect("remove");
    }
    std::fs::write(path, b"").expect("write");
}

/// The move is one transaction: when its last statement fails, no box set has
/// left the old row, and the next boot does it all. Its first steps stand:
/// the library exists, and users already name it, which the retry leaves alone.
#[tokio::test]
async fn a_failed_move_leaves_the_box_sets_where_they_were() {
    let lab = Lab::new().await;
    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    let box_set = lab.seed_box_set("Heist", legacy).await;
    let user = seed_user(&lab.db, Uuid::from_u128(0x69)).await;
    let enabled = [legacy.hyphenated().to_string()];
    set_preference(&lab.db, &user.id, PreferenceKind::EnabledFolders, &enabled)
        .await
        .expect("preference");
    fail_item_deletes(&lab.db, true).await;

    assert!(lab.merge().await.is_err(), "the delete fails");

    let row = fetch_item(&lab.db, box_set).await;
    assert_eq!(row.parent_id, Some(guid_to_db(legacy)));
    assert_eq!(row.top_parent_id, Some(guid_to_db(legacy)));
    assert_eq!(ancestors_of(&lab.db, box_set).await, Vec::<String>::new());
    assert!(fetch_item_opt(&lab.db, legacy).await.is_some());
    assert_eq!(lab.library_names().await, ["Collections"]);

    fail_item_deletes(&lab.db, false).await;
    let merged = lab.merge().await.expect("repair").expect("work to do");
    assert_eq!(merged.library, id(LIBRARY_ID));
    lab.assert_under(box_set, id(FOLDER_ID), id(LIBRARY_ID))
        .await;
    assert_eq!(lab.library_names().await, ["Collections"]);
    assert_eq!(
        get_preference(lab.db.pool(), &user.id, PreferenceKind::EnabledFolders)
            .await
            .expect("preference"),
        [id(LIBRARY_ID).hyphenated().to_string()]
    );
}

/// The database file goes aside before anything moves, and only when there is
/// something to move.
#[tokio::test]
async fn the_database_is_copied_only_when_there_is_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("jellyfin.db");
    let db = Database::connect(&format!("sqlite://{}?mode=rwc", file.display()))
        .await
        .expect("connect");
    db.run_migrations().await.expect("migrate");
    let lab = Lab::over(db, "jellyfin-10.11.8").await;
    let snapshot = dir.path().join("jellyfin.db.pre-collections-library");

    assert_eq!(lab.merge().await.expect("nothing to do"), None);
    assert!(!snapshot.exists());

    let legacy = id(LEGACY_ID);
    lab.seed_legacy(legacy).await;
    lab.seed_box_set("Heist", legacy).await;
    assert!(lab.merge().await.expect("repair").is_some());
    assert!(snapshot.is_file());
}
