//! A library adopted from Jellyfin, scanned by Ferrofin
//! (`PLAN_SCAN_CHANGE_DETECTION` 6A).
//!
//! Jellyfin stores a library item's `TopParentId` (and a top-level item's
//! `ParentId`) as the library's *physical* folder — the `Folder` row at the
//! library location, a child of the `AggregateFolder` that the collection
//! folder names in its `PhysicalFolderIds` (`BaseItem.GetTopParent`). A
//! Ferrofin scan saves the rows it plans under the collection folder itself.
//! So an adopted library's rows carry either, and both are the library:
//!
//! - every row is browsed, the ones a scan saved and the ones it did not;
//! - media deleted from disk is pruned whichever it carries — by a library
//!   scan and by a path-scoped (watcher/webhook) one;
//! - the library's own folders are never pruned: the collection folder, the
//!   physical folders it names, a row at one of its locations;
//! - no row whose path is still on disk is pruned, whatever its kind (an
//!   adopted plain-subfolder `Folder` row, which the scan never plans);
//! - an empty or missing location prunes nothing;
//! - a native library (no `PhysicalFolderIds`) behaves as it always did.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id, stored_type_name};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

/// The movie files of the fixture library, relative to its location: three
/// per-title folders and one title under a plain subfolder, which Jellyfin
/// stores as a `Folder` row and Ferrofin never plans.
const MOVIES: [&str; 4] = [
    "A (2001)/A (2001).mkv",
    "B (2002)/B (2002).mkv",
    "C (2003)/C (2003).mkv",
    "Collection/D (2004)/D (2004).mkv",
];

/// A movie library, scanned once natively and then — when `adopted` — put
/// in the shape a Jellyfin database carries it.
struct Library {
    _tmp: tempfile::TempDir,
    db: Database,
    scanner: LibraryScanner,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: Arc<dyn ItemRepository>,
    /// The library location.
    media: PathBuf,
    /// The collection folder.
    cf: Uuid,
    /// The physical folder at the location (adopted only).
    physical: Uuid,
    /// The plain subfolder's `Folder` row (adopted only).
    subfolder: Uuid,
}

impl Library {
    async fn new(adopted: bool) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let media = tmp.path().join("movies");
        for movie in MOVIES {
            let file = media.join(movie);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
            std::fs::write(&file, b"").expect("write");
        }
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("default"))
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
        let cf = vf
            .get_virtual_folders()
            .await
            .expect("folders")
            .first()
            .and_then(|f| f.item_id.as_deref())
            .and_then(|id| Uuid::parse_str(id).ok())
            .expect("collection folder id");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner =
            LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence.clone())
                .with_items(Arc::clone(&items));
        let first = scanner.scan_all().await.expect("first scan");
        assert_eq!(first.created, MOVIES.len(), "{first:?}");
        let folder_id = |path: &Path| {
            derive_item_id(BaseItemKind::Folder, &path.to_string_lossy()).expect("folder id")
        };
        let mut library = Self {
            physical: folder_id(&media),
            subfolder: folder_id(&media.join("Collection")),
            _tmp: tmp,
            db,
            scanner,
            persistence,
            items,
            media,
            cf,
        };
        if adopted {
            library.adopt().await;
        } else {
            library.physical = Uuid::nil();
            library.subfolder = Uuid::nil();
        }
        library
    }

    /// Rewrites the natively scanned library into Jellyfin's shape: a
    /// physical `Folder` row at the location (its own top parent, as a child
    /// of the aggregate folder is) that the collection folder names in its
    /// `PhysicalFolderIds`, every item carrying it as `TopParentId` and the
    /// top-level ones as `ParentId`, and the plain subfolder a `Folder` row
    /// that parents the title under it.
    async fn adopt(&self) {
        let (physical, subfolder) = (guid_to_db(self.physical), guid_to_db(self.subfolder));
        insert_folder(
            &self.db,
            &physical,
            None,
            &physical,
            &self.media.to_string_lossy(),
        )
        .await;
        insert_folder(
            &self.db,
            &subfolder,
            Some(&physical),
            &physical,
            &self.media.join("Collection").to_string_lossy(),
        )
        .await;
        self.set_physical_folder_ids(&[self.physical]).await;
        let cf = guid_to_db(self.cf);
        for sql in [
            r#"UPDATE "BaseItems" SET "TopParentId" = ?2 WHERE "TopParentId" = ?1"#,
            r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "ParentId" = ?1"#,
        ] {
            sqlx::query(sql)
                .bind(&cf)
                .bind(&physical)
                .execute(self.db.writer())
                .await
                .expect("adopt");
        }
        sqlx::query(r#"UPDATE "BaseItems" SET "ParentId" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(self.movie(3)))
            .bind(&subfolder)
            .execute(self.db.writer())
            .await
            .expect("parent under the subfolder");
    }

    /// Stamps the collection folder's `Data` as Jellyfin serializes it.
    async fn set_physical_folder_ids(&self, folders: &[Uuid]) {
        let ids: Vec<String> = folders
            .iter()
            .map(|f| format!(r#""{}""#, f.simple()))
            .collect();
        sqlx::query(r#"UPDATE "BaseItems" SET "Data" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(self.cf))
            .bind(format!(
                r#"{{"CollectionType":"movies","PhysicalFolderIds":[{}]}}"#,
                ids.join(",")
            ))
            .execute(self.db.writer())
            .await
            .expect("physical folder ids");
    }

    /// The `n`th movie of [`MOVIES`]'s id.
    fn movie(&self, n: usize) -> Uuid {
        derive_item_id(
            BaseItemKind::Movie,
            &self.media.join(MOVIES[n]).to_string_lossy(),
        )
        .expect("movie id")
    }

    /// Deletes the `n`th movie's file.
    fn delete(&self, n: usize) -> String {
        let file = self.media.join(MOVIES[n]);
        std::fs::remove_file(&file).expect("delete");
        file.to_string_lossy().into_owned()
    }

    /// Every stored row: id → (`TopParentId`, `ParentId`).
    async fn rows(&self) -> HashMap<Uuid, (Option<Uuid>, Option<Uuid>)> {
        let rows: Vec<(String, Option<String>, Option<String>)> =
            sqlx::query_as(r#"SELECT "Id", "TopParentId", "ParentId" FROM "BaseItems""#)
                .fetch_all(self.db.pool())
                .await
                .expect("rows");
        let id = |v: Option<String>| v.and_then(|v| Uuid::parse_str(&v).ok());
        rows.into_iter()
            .filter_map(|(row, top, parent)| {
                Some((Uuid::parse_str(&row).ok()?, (id(top), id(parent))))
            })
            .collect()
    }

    /// The movies a recursive browse of the library returns, by title (the
    /// name without its year).
    async fn browse(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .items
            .get_item_list(&InternalItemsQuery {
                parent_id: self.cf,
                recursive: true,
                include_item_types: vec![BaseItemKind::Movie],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("browse")
            .into_iter()
            .filter_map(|row| row.name)
            .map(|name| name.split(" (").next().unwrap_or_default().to_owned())
            .collect();
        names.sort();
        names
    }
}

/// Inserts a Jellyfin `Folder` row.
async fn insert_folder(db: &Database, id: &str, parent: Option<&str>, top: &str, path: &str) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "ParentId", "TopParentId", "Path", "Name")
           VALUES (?1, ?2, 1, 0, 0, 0, 0, 0, 0, ?3, ?4, ?5, 'folder')"#,
    )
    .bind(id)
    .bind(stored_type_name(BaseItemKind::Folder).expect("folder type"))
    .bind(parent)
    .bind(top)
    .bind(path)
    .execute(db.writer())
    .await
    .expect("insert folder");
}

/// Inserts a Jellyfin movie-library row of `kind` at `path` carrying `top`,
/// owned by `owner` (with no extra type: a part or a version stored under
/// its owner) or a version of `primary`.
async fn insert_version(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    path: &Path,
    top: Uuid,
    owner: Option<Uuid>,
    primary: Option<Uuid>,
) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "TopParentId", "OwnerId",
            "PrimaryVersionId", "Path", "Name")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, 0, ?3, ?4, ?5, ?6, 'version')"#,
    )
    .bind(guid_to_db(id))
    .bind(stored_type_name(kind).expect("type"))
    .bind(guid_to_db(top))
    .bind(owner.map(guid_to_db))
    .bind(primary.map(guid_to_db))
    .bind(path.to_string_lossy().into_owned())
    .execute(db.writer())
    .await
    .expect("insert version");
}

/// Both prunes weigh exactly the rows a library read lists: an owned
/// non-extra (Jellyfin's part or version stored under its owner) and an
/// alternate version whose primary is in the library are never weighed —
/// not even when the version's own file is gone — while a version whose
/// primary is in no library is listed, and pruned once its file is gone. An
/// owned `Video` at a path the scan plans as a `Movie` is not pruned either:
/// it moves to the `Movie` id (owner decision D9b). The same for a
/// path-scoped scan and a library scan.
#[tokio::test]
async fn a_version_a_library_read_does_not_list_is_never_pruned() {
    let lib = Library::new(true).await;
    // A version Ferrofin plans as a movie of its own: it moves to that id.
    let part_path = lib.media.join("A (2001)/A (2001) - 1080p.mkv");
    std::fs::write(&part_path, b"").expect("write");
    let part = derive_item_id(BaseItemKind::Video, &part_path.to_string_lossy()).expect("id");
    let moved = derive_item_id(BaseItemKind::Movie, &part_path.to_string_lossy()).expect("id");
    insert_version(
        &lib.db,
        part,
        BaseItemKind::Video,
        &part_path,
        lib.physical,
        Some(lib.movie(0)),
        None,
    )
    .await;
    // A version of B whose file is gone.
    let version = Uuid::from_u128(0x0A_0002);
    let version_path = lib.media.join("B (2002)/B (2002) - 2160p.mkv");
    insert_version(
        &lib.db,
        version,
        BaseItemKind::Movie,
        &version_path,
        lib.physical,
        None,
        Some(lib.movie(1)),
    )
    .await;
    // A version whose primary is in no library, its file gone.
    let orphan = Uuid::from_u128(0x0A_0003);
    let orphan_path = lib.media.join("C (2003)/C (2003) - 720p.mkv");
    insert_version(
        &lib.db,
        orphan,
        BaseItemKind::Movie,
        &orphan_path,
        lib.physical,
        None,
        Some(Uuid::from_u128(0x0A_0004)),
    )
    .await;

    let scoped = lib
        .scanner
        .scan_paths(&[
            lib.media.join("A (2001)").to_string_lossy().into_owned(),
            version_path.to_string_lossy().into_owned(),
            orphan_path.to_string_lossy().into_owned(),
        ])
        .await
        .expect("scan");
    assert_eq!(scoped.removed, 1, "{scoped:?}");
    let rows = lib.rows().await;
    assert!(
        rows.contains_key(&moved) && !rows.contains_key(&part),
        "an owned Video planned as a Movie moves"
    );
    assert!(rows.contains_key(&version), "a version of B is not weighed");
    assert!(
        !rows.contains_key(&orphan),
        "a listed row whose file is gone"
    );

    let full = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(full.removed, 0, "{full:?}");
    let rows = lib.rows().await;
    assert!(rows.contains_key(&moved) && rows.contains_key(&version));
}

/// The first scan of an adopted library saves what it plans under the
/// collection folder and prunes what is gone — an adopted row that still
/// carries the physical folder — and the library still browses whole; the
/// next scan prunes a row that the first one saved, which is the case a
/// prune reading only the physical folders never saw; the scan after that
/// is quiet.
#[tokio::test]
async fn an_adopted_library_is_browsed_and_pruned_by_both_top_parents() {
    let lib = Library::new(true).await;
    assert_eq!(
        lib.browse().await,
        ["A", "B", "C", "D"],
        "adopted, unscanned"
    );

    lib.delete(2);
    let first = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(first.removed, 1, "{first:?}");
    let rows = lib.rows().await;
    assert!(!rows.contains_key(&lib.movie(2)), "C was deleted from disk");
    for n in [0, 1, 3] {
        assert_eq!(
            rows[&lib.movie(n)],
            (Some(lib.cf), Some(lib.cf)),
            "{} saved under the collection folder",
            MOVIES[n]
        );
    }
    assert!(
        rows.contains_key(&lib.physical),
        "the physical folder is the library's own"
    );
    assert!(
        rows.contains_key(&lib.subfolder),
        "a Folder row whose directory is still on disk is not gone"
    );
    assert_eq!(lib.browse().await, ["A", "B", "D"], "scanned");

    lib.delete(1);
    let second = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(second.removed, 1, "{second:?}");
    assert!(!lib.rows().await.contains_key(&lib.movie(1)));
    assert_eq!(lib.browse().await, ["A", "D"]);

    let quiet = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(
        (quiet.created, quiet.updated, quiet.removed),
        (0, 0, 0),
        "{quiet:?}"
    );
}

/// A watcher or webhook event for a deleted file of an adopted library
/// that no scan has saved yet prunes it: the path-scoped prune reads the
/// library's physical folder too.
#[tokio::test]
async fn a_path_scoped_scan_prunes_an_adopted_row() {
    let lib = Library::new(true).await;
    let gone = lib.delete(0);
    let outcome = lib.scanner.scan_paths(&[gone]).await.expect("scan");
    assert_eq!(outcome.removed, 1, "{outcome:?}");
    let rows = lib.rows().await;
    assert!(!rows.contains_key(&lib.movie(0)));
    for n in [1, 2, 3] {
        assert_eq!(
            rows[&lib.movie(n)].0,
            Some(lib.physical),
            "{} is outside the scope and untouched",
            MOVIES[n]
        );
    }
    assert_eq!(lib.browse().await, ["B", "C", "D"]);
}

/// A location that lists empty (a dropped mount) or is missing prunes
/// nothing of an adopted library — neither the rows that carry the
/// physical folder nor the folder itself — and neither does the scan once
/// it is back.
#[tokio::test]
async fn an_unreachable_adopted_location_prunes_nothing() {
    let lib = Library::new(true).await;
    let before = lib.rows().await;
    let away = lib.media.with_extension("away");
    std::fs::rename(&lib.media, &away).expect("unmount");

    std::fs::create_dir(&lib.media).expect("empty mountpoint");
    let empty = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(empty.removed, 0, "{empty:?}");
    assert_eq!(lib.rows().await.len(), before.len());

    std::fs::remove_dir(&lib.media).expect("no mountpoint");
    let missing = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(missing.removed, 0, "{missing:?}");
    assert_eq!(lib.rows().await.len(), before.len());

    std::fs::rename(&away, &lib.media).expect("remount");
    let back = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(back.removed, 0, "{back:?}");
    assert_eq!(lib.browse().await, ["A", "B", "C", "D"]);
}

/// The library's own folders are never pruned — even a physical folder its
/// `PhysicalFolderIds` still names whose directory is gone (a location
/// removed since) — while an adopted `Folder` row whose directory is gone
/// is, and one whose directory is still there is not.
#[tokio::test]
async fn a_library_s_own_folders_and_folders_on_disk_are_never_pruned() {
    let lib = Library::new(true).await;
    let old_location = Uuid::from_u128(0x0A_0001);
    let old = guid_to_db(old_location);
    insert_folder(
        &lib.db,
        &old,
        None,
        &old,
        "/nonexistent-ferrofin-6a/old-location",
    )
    .await;
    lib.set_physical_folder_ids(&[lib.physical, old_location])
        .await;
    let gone_folder = derive_item_id(
        BaseItemKind::Folder,
        &lib.media.join("Gone").to_string_lossy(),
    )
    .expect("id");
    insert_folder(
        &lib.db,
        &guid_to_db(gone_folder),
        Some(&guid_to_db(lib.physical)),
        &guid_to_db(lib.physical),
        &lib.media.join("Gone").to_string_lossy(),
    )
    .await;
    assert_eq!(
        lib.persistence
            .library_top_parents(lib.cf)
            .await
            .expect("read")
            .expect("answers"),
        vec![lib.cf, lib.physical, old_location]
    );

    let outcome = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(outcome.removed, 1, "{outcome:?}");
    let rows = lib.rows().await;
    assert!(!rows.contains_key(&gone_folder), "its directory is gone");
    assert!(
        rows.contains_key(&old_location),
        "named by PhysicalFolderIds"
    );
    assert!(
        rows.contains_key(&lib.physical),
        "named by PhysicalFolderIds"
    );
    assert!(
        rows.contains_key(&lib.subfolder),
        "its directory is on disk"
    );
    assert!(rows.contains_key(&lib.cf));
}

/// A native library names its location's folder in `PhysicalFolderIds` as
/// Jellyfin's does (owner decision D1), so it is read by its collection folder
/// and that folder, and pruned as it always was; a row of a kind the scan
/// never plans is kept while its path is on disk.
#[tokio::test]
async fn a_native_library_is_read_by_its_collection_folder_and_location() {
    let lib = Library::new(false).await;
    let location = derive_item_id(BaseItemKind::Folder, &lib.media.to_string_lossy()).expect("id");
    assert_eq!(
        lib.persistence
            .library_top_parents(lib.cf)
            .await
            .expect("read")
            .expect("answers"),
        vec![lib.cf, location]
    );
    let stray = derive_item_id(
        BaseItemKind::Folder,
        &lib.media.join("Collection").to_string_lossy(),
    )
    .expect("id");
    let cf = guid_to_db(lib.cf);
    insert_folder(
        &lib.db,
        &guid_to_db(stray),
        Some(&cf),
        &cf,
        &lib.media.join("Collection").to_string_lossy(),
    )
    .await;

    lib.delete(0);
    let outcome = lib.scanner.scan_all().await.expect("scan");
    assert_eq!(outcome.removed, 1, "{outcome:?}");
    let rows = lib.rows().await;
    assert!(!rows.contains_key(&lib.movie(0)));
    assert!(rows.contains_key(&stray), "its directory is on disk");
    for n in [1, 2, 3] {
        assert_eq!(rows[&lib.movie(n)], (Some(lib.cf), Some(lib.cf)));
    }
    assert_eq!(lib.browse().await, ["B", "C", "D"]);
}
