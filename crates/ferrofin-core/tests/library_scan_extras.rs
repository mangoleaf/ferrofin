//! Extras stay owned by their movie and outside library browse (#32).

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id, stored_type_name};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

struct Fixture {
    tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    repo: Arc<FerrofinItemRepository>,
    store: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    library: Uuid,
    fs: Arc<SelectiveFs>,
}

impl Fixture {
    async fn new(files: &[&str]) -> Self {
        Self::with_type(files, CollectionTypeOptions::movies).await
    }

    async fn with_type(files: &[&str], kind: CollectionTypeOptions) -> Self {
        Self::with_options(files, kind, LibraryOptions::default()).await
    }

    async fn with_options(
        files: &[&str],
        kind: CollectionTypeOptions,
        options: LibraryOptions,
    ) -> Self {
        Self::build(files, Some(kind), options).await
    }

    /// A library with no collection type at all.
    async fn untyped(files: &[&str]) -> Self {
        Self::build(files, None, LibraryOptions::default()).await
    }

    async fn build(
        files: &[&str],
        kind: Option<CollectionTypeOptions>,
        options: LibraryOptions,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        for file in files {
            let path = media.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let store = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
                .with_item_store(store.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            kind,
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..options
            },
        )
        .await
        .unwrap();
        let library = Uuid::parse_str(
            vf.get_virtual_folders().await.unwrap()[0]
                .item_id
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        let repo = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let fs = Arc::new(SelectiveFs::default());
        let scanner = LibraryScanner::new(vf, fs.clone(), store.clone()).with_items(repo.clone());
        Self {
            tmp,
            media,
            db,
            repo,
            store,
            scanner,
            library,
            fs,
        }
    }

    async fn row(&self, relative: &str) -> BaseItemEntity {
        sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Path" = ?"#)
            .bind(self.media.join(relative).to_string_lossy().as_ref())
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn ancestors(&self, id: &str) -> Vec<String> {
        sqlx::query_scalar(r#"SELECT "ParentItemId" FROM "AncestorIds" WHERE "ItemId" = ?1"#)
            .bind(id)
            .fetch_all(self.db.pool())
            .await
            .unwrap()
    }

    async fn assert_browse(&self, expected: usize) {
        for recursive in [false, true] {
            let rows = self
                .repo
                .get_item_list(&InternalItemsQuery {
                    parent_id: self.library,
                    recursive,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(rows.len(), expected, "recursive={recursive}: {rows:?}");
            assert!(
                rows.iter().all(|r| r.owner_id.is_none()),
                "owned extra in browse"
            );
        }
    }
}

const MOVIE: &str = "Heat (1995)/Heat.mkv";
const EXTRA: &str = "Heat (1995)/Extras/Deleted.Scenes.avi";

#[tokio::test]
async fn owned_extras_are_accessible_without_becoming_library_children() {
    let f = Fixture::new(&[
        MOVIE,
        EXTRA,
        "Heat (1995)/Heat-trailer.mkv",
        "Heat (1995)/theme.mp3",
        "Heat (1995)/Heat-sample.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    f.assert_browse(1).await;
    let owner = Uuid::parse_str(&f.row(MOVIE).await.id).unwrap();
    let extras = f
        .repo
        .get_item_list(&InternalItemsQuery {
            owner_ids: vec![owner],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(extras.len(), 4);
    assert!(
        extras
            .iter()
            .all(|r| r.parent_id.is_none() && r.top_parent_id.is_none())
    );
    for extra in extras {
        assert!(
            f.repo
                .retrieve_item(Uuid::parse_str(&extra.id).unwrap())
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn normal_scan_repairs_locked_unchanged_extra_relationships() {
    let f = Fixture::new(&[MOVIE, EXTRA]).await;
    f.scanner.scan_all().await.unwrap();
    let mut extra = f.row(EXTRA).await;
    let id = extra.id.clone();
    extra.parent_id = Some(guid_to_db(f.library));
    extra.top_parent_id = Some(guid_to_db(f.library));
    extra.is_locked = true;
    extra.overview = Some("Preserve this description".into());
    f.store
        .save_items(std::slice::from_ref(&extra))
        .await
        .unwrap();
    f.store
        .add_locked_fields(
            Uuid::parse_str(&id).unwrap(),
            &[ferrofin_db::enums::metadata_field::to_i32(
                ferrofin_model::entities::MetadataField::Name,
            )],
        )
        .await
        .unwrap();
    play(&f.db, &id).await;
    let poster = f.media.join("extra-poster.png");
    std::fs::write(&poster, b"retained image").unwrap();
    sqlx::query(r#"INSERT INTO "BaseItemImageInfos" ("Id", "ItemId", "Path", "ImageType", "Width", "Height") VALUES (?, ?, ?, 0, 100, 150)"#)
        .bind(guid_to_db(Uuid::new_v4())).bind(&id).bind(poster.to_string_lossy().as_ref()).execute(f.db.pool()).await.unwrap();
    sqlx::query(r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue") VALUES (?, 'Imdb', 'tt12345')"#)
        .bind(&id).execute(f.db.pool()).await.unwrap();
    f.scanner.scan_all().await.unwrap();
    f.assert_browse(1).await;
    let history: (i64, i64) =
        sqlx::query_as(r#"SELECT "Played", "PlayCount" FROM "UserData" WHERE "ItemId"=?"#)
            .bind(&id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(history, (1, 1));
    let image: String =
        sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItemImageInfos" WHERE "ItemId"=?"#)
            .bind(&id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(image, poster.to_string_lossy());
    let provider: String = sqlx::query_scalar(r#"SELECT "ProviderValue" FROM "BaseItemProviders" WHERE "ItemId"=? AND "ProviderId"='Imdb'"#).bind(&id).fetch_one(f.db.pool()).await.unwrap();
    assert_eq!(provider, "tt12345");
    let repaired = f.row(EXTRA).await;
    assert_eq!(repaired.id, id);
    assert_eq!(repaired.overview, extra.overview);
    assert!(repaired.is_locked);
    assert!(repaired.parent_id.is_none() && repaired.top_parent_id.is_none());
    let saved = repaired.date_last_saved;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(EXTRA).await.date_last_saved, saved);
}

#[tokio::test]
async fn scan_ignores_resource_forks_and_dot_samples_but_keeps_owned_suffix_samples() {
    let f = Fixture::new(&[
        MOVIE,
        "Heat (1995)/._Heat.mkv",
        "Heat (1995)/sample.mkv",
        "Heat (1995)/Heat.sample.mkv",
        "Heat (1995)/Heat-sample.mkv",
        "Heat (1995)/Heat_sample.mkv",
        "Heat (1995)/samples/clip.mkv",
        "@eaDir/stray.mkv",
        ".hidden/stray.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let paths: Vec<String> = sqlx::query_scalar(
        r#"SELECT "Path" FROM "BaseItems" WHERE "MediaType" IS NOT NULL ORDER BY "Path""#,
    )
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(paths.len(), 4, "{paths:?}");
    assert_eq!(
        f.row("Heat (1995)/Heat-sample.mkv").await.extra_type,
        Some(7)
    );
    assert_eq!(
        f.row("Heat (1995)/Heat_sample.mkv").await.extra_type,
        Some(7)
    );
    assert_eq!(
        f.row("Heat (1995)/samples/clip.mkv").await.extra_type,
        Some(7)
    );
}

#[tokio::test]
async fn removed_parentless_extras_are_pruned_by_full_and_scoped_scans() {
    for scoped in [false, true] {
        let f = Fixture::new(&[MOVIE, EXTRA]).await;
        f.scanner.scan_all().await.unwrap();
        let extra = f.row(EXTRA).await;
        let path = f.media.join(EXTRA);
        std::fs::remove_file(&path).unwrap();
        let result = if scoped {
            f.scanner
                .scan_paths(&[path.to_string_lossy().into_owned()])
                .await
                .unwrap()
        } else {
            f.scanner.scan_all().await.unwrap()
        };
        assert_eq!(result.removed, 1);
        assert!(
            f.repo
                .retrieve_item(Uuid::parse_str(&extra.id).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        f.assert_browse(1).await;
    }
}

#[tokio::test]
async fn deleting_the_owner_removes_parentless_extras() {
    let f = Fixture::new(&[MOVIE, EXTRA]).await;
    f.scanner.scan_all().await.unwrap();
    let movie = Uuid::parse_str(&f.row(MOVIE).await.id).unwrap();
    let extra = Uuid::parse_str(&f.row(EXTRA).await.id).unwrap();
    f.store.delete_items(&[movie]).await.unwrap();
    assert!(f.repo.retrieve_item(extra).await.unwrap().is_none());
    assert!(
        f.media.join(EXTRA).exists(),
        "database deletion keeps the file"
    );
}

#[tokio::test]
async fn nested_release_and_mixed_folders_do_not_lend_the_wrong_owner() {
    let f = Fixture::new(&[
        MOVIE,
        EXTRA,
        "Heat (1995)/Heat-sample.mkv",
        "Heat (1995)/Release/Other.mkv",
        "Heat (1995)/Release/other-sample.mkv",
        "Mixed/First.mkv",
        "Mixed/Second.mkv",
        "Mixed/First-trailer.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let extras = f
        .repo
        .get_item_list(&InternalItemsQuery {
            has_owner_id: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    // `Heat (1995)` has a real subfolder, so it is no own-folder movie; its
    // files resolve to one item (`Heat-sample.mkv` is left out of the count),
    // which is not in a mixed folder and so owns the folder's extras
    // (`MovieResolver.cs:283-287`, `BaseItem.SearchesContainingFolderForExtras`).
    // `Mixed/` resolves to three items (an extra counts), so its videos are
    // mixed and own nothing.
    let heat = f.row(MOVIE).await;
    let other = f.row("Heat (1995)/Release/Other.mkv").await;
    let mut owners: Vec<_> = extras
        .iter()
        .map(|extra| (extra.path.clone().unwrap(), extra.owner_id.clone()))
        .collect();
    owners.sort();
    let at = |relative: &str| f.media.join(relative).to_string_lossy().into_owned();
    assert_eq!(
        owners,
        vec![
            (at(EXTRA), Some(heat.id.clone())),
            (at("Heat (1995)/Heat-sample.mkv"), Some(heat.id.clone())),
            (
                at("Heat (1995)/Release/other-sample.mkv"),
                Some(other.id.clone())
            ),
        ]
    );
    assert!(!heat.is_in_mixed_folder);
    assert!(!other.is_in_mixed_folder);
    assert!(f.row("Mixed/First.mkv").await.is_in_mixed_folder);
    assert!(f.row("Mixed/Second.mkv").await.is_in_mixed_folder);
}

#[tokio::test]
async fn scoped_discovery_ignores_files_and_preserves_metadata_sidecars() {
    let f = Fixture::new(&[MOVIE]).await;
    let nfo = f.media.join("Heat (1995)/movie.nfo");
    std::fs::write(
        &nfo,
        "<movie><title>Local title</title><plot>Local plot</plot></movie>",
    )
    .unwrap();
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(MOVIE).await.overview.as_deref(), Some("Local plot"));
    for path in [
        "Heat (1995)/._Heat.mkv",
        "Heat (1995)/sample.mkv",
        ".hidden/stray.mkv",
    ] {
        let path = f.media.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let result = f
            .scanner
            .scan_paths(&[path.to_string_lossy().into_owned()])
            .await
            .unwrap();
        assert_eq!(result.created, 0);
    }
    f.assert_browse(1).await;
    assert_eq!(f.row(MOVIE).await.overview.as_deref(), Some("Local plot"));
}

#[tokio::test]
async fn grouped_movies_keep_generic_and_version_specific_extras_in_full_and_scoped_scans() {
    for scoped in [false, true] {
        for (movies, owners) in [
            (
                vec![
                    "Film/Film.mkv",
                    "Film/Film - 4K.mkv",
                    "Film/Film - 4Kish.mkv",
                ],
                vec![
                    "Film/Film.mkv",
                    "Film/Film - 4K.mkv",
                    "Film/Film - 4Kish.mkv",
                ],
            ),
            (
                vec!["Film/Film cd1.mkv", "Film/Film cd2.mkv"],
                vec!["Film/Film cd1.mkv"; 3],
            ),
        ] {
            let f = Fixture::new(&movies).await;
            f.scanner.scan_all().await.unwrap();
            let extras = [
                "Film/Extras/clip.mkv",
                "Film/Film - 4K-trailer.mkv",
                "Film/Film - 4Kish-trailer.mkv",
            ];
            for (extra, owner) in extras.into_iter().zip(owners) {
                let path = f.media.join(extra);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b"").unwrap();
                if scoped {
                    f.scanner
                        .scan_paths(&[path.to_string_lossy().into_owned()])
                        .await
                        .unwrap();
                } else {
                    f.scanner.scan_all().await.unwrap();
                }
                let extra = f.row(extra).await;
                assert_eq!(extra.owner_id, Some(f.row(owner).await.id));
                assert!(extra.parent_id.is_none());
            }
        }
    }
}

/// An adopted alternate version stored as a `Video` under an id of its own,
/// where the movie walk plans a `Movie`, moves to the id the `Movie` kind
/// derives (owner decision D9b) — in a full scan, and in a path-scoped scan
/// of a new extra of it, which plans the version for context yet refreshes
/// and saves it as the new item it is — and the new extra is owned there.
#[tokio::test]
async fn adopted_version_row_moves_before_its_new_extra_takes_an_owner() {
    for scoped in [false, true] {
        let f = Fixture::new(&["Film/Film.mkv", "Film/Film - 4K.mkv"]).await;
        f.scanner.scan_all().await.unwrap();
        let mut version = f.row("Film/Film - 4K.mkv").await;
        let planned_id = version.id.clone();
        let adopted_id = guid_to_db(Uuid::new_v4());
        f.store
            .delete_items(&[Uuid::parse_str(&version.id).unwrap()])
            .await
            .unwrap();
        version.id.clone_from(&adopted_id);
        version.type_ = "MediaBrowser.Controller.Entities.Video".into();
        f.store.save_items(&[version]).await.unwrap();
        let path = "Film/Film - 4K-trailer.mkv";
        std::fs::write(f.media.join(path), b"").unwrap();
        scan_after_adding(&f, path, scoped).await;
        let moved = f.row("Film/Film - 4K.mkv").await;
        assert_eq!(moved.id, planned_id);
        assert!(moved.type_.ends_with(".Movie"), "{}", moved.type_);
        // Refreshed and saved as the new item it is, even where the scoped
        // scan carries it for context only.
        assert!(moved.date_last_refreshed.is_some(), "scoped={scoped}");
        assert_eq!(f.row(path).await.owner_id, Some(planned_id));
        assert!(
            f.repo
                .retrieve_item(Uuid::parse_str(&adopted_id).unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[derive(Default)]
struct SelectiveFs {
    fail: std::sync::Mutex<Option<String>>,
}

impl ferrofin_traits::filesystem::FileSystem for SelectiveFs {
    fn get_file_system_entries(&self, path: &str) -> Vec<ferrofin_model::io::FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<ferrofin_model::io::FileSystemEntryInfo>, ferrofin_traits::error::ServiceError>
    {
        if self.fail.lock().unwrap().as_deref() == Some(path) {
            return Err(ferrofin_traits::error::ServiceError::NotFound(
                "injected listing failure".into(),
            ));
        }
        FerrofinFileSystem::new().try_get_file_system_entries(path)
    }
    fn get_drives(&self) -> Vec<ferrofin_model::io::FileSystemEntryInfo> {
        Vec::new()
    }
    fn file_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().file_exists(path)
    }
    fn directory_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().directory_exists(path)
    }
    fn validate_writable(&self, path: &str) -> Result<(), ferrofin_traits::error::ServiceError> {
        FerrofinFileSystem::new().validate_writable(path)
    }
    fn get_files(
        &self,
        path: &str,
        extensions: &[&str],
    ) -> Vec<ferrofin_traits::filesystem::FileMetadata> {
        FerrofinFileSystem::new().get_files(path, extensions)
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ferrofin_traits::error::ServiceError> {
        FerrofinFileSystem::new().read_file(path)
    }
}

impl Fixture {
    async fn legacy(&self, path: &str, owner: bool) -> String {
        let mut row = self.row(MOVIE).await;
        row.id = guid_to_db(Uuid::new_v4());
        row.path = Some(self.media.join(path).to_string_lossy().into_owned());
        row.type_ = "MediaBrowser.Controller.Entities.Video".into();
        row.is_movie = false;
        row.is_locked = true;
        row.extra_type = Some(7);
        row.owner_id = if owner {
            Some(self.row(MOVIE).await.id)
        } else {
            None
        };
        row.parent_id = Some(guid_to_db(self.library));
        row.top_parent_id = Some(guid_to_db(self.library));
        let id = row.id.clone();
        self.store.save_items(&[row]).await.unwrap();
        id
    }

    async fn has(&self, id: &str) -> bool {
        self.repo
            .retrieve_item(Uuid::parse_str(id).unwrap())
            .await
            .unwrap()
            .is_some()
    }
}

#[tokio::test]
async fn one_scan_removes_confirmed_legacy_exclusions_without_removing_files() {
    let excluded = [
        "Heat (1995)/._Heat.mkv",
        "Heat (1995)/Heat.sample.mkv",
        "Heat (1995)/@eaDir/stray.mkv",
        "Heat (1995)/.hidden/clip.mkv",
    ];
    for scoped in [false, true] {
        let mut files = vec![MOVIE, EXTRA, "Heat (1995)/Heat-sample.mkv"];
        files.extend(excluded);
        let f = Fixture::new(&files).await;
        f.scanner.scan_all().await.unwrap();
        let mut ids = Vec::new();
        for (i, path) in excluded.iter().enumerate() {
            ids.push(f.legacy(path, i % 2 == 0).await);
        }
        let good_id = f.row(EXTRA).await.id;
        if scoped {
            // One excluded exact path cannot remove siblings outside scope.
            let result = f
                .scanner
                .scan_paths(&[f.media.join(excluded[0]).to_string_lossy().into_owned()])
                .await
                .unwrap();
            assert_eq!(result.removed, 1);
            assert!(!f.has(&ids[0]).await);
            for id in &ids[1..] {
                assert!(f.has(id).await);
            }
            let result = f
                .scanner
                .scan_paths(&[f.media.join("Heat (1995)").to_string_lossy().into_owned()])
                .await
                .unwrap();
            assert_eq!(result.removed, 3);
        } else {
            assert_eq!(f.scanner.scan_all().await.unwrap().removed, 4);
        }
        for (path, id) in excluded.iter().zip(&ids) {
            assert!(!f.has(id).await);
            assert!(f.media.join(path).exists());
        }
        assert!(f.has(&good_id).await);
        assert_eq!(
            f.row("Heat (1995)/Heat-sample.mkv").await.extra_type,
            Some(7)
        );
        assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    }
}

#[tokio::test]
/// A legacy extra row (another id, extra type and owner) at a file the scan
/// plans otherwise is cleaned: the user locked it and the scan's own row
/// there is bare, so the locked row takes the planned item's id, extra type
/// and owner (the scan's row folded into it) — no row is pruned, no movie or
/// file deleted.
async fn extras_in_ineligible_folders_are_cleaned_without_deleting_movies() {
    let f = Fixture::new(&[MOVIE, EXTRA, "Heat (1995)/Release/Other.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let planned = f.row(EXTRA).await;
    let id = f.legacy(EXTRA, false).await;
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    assert!(!f.has(&id).await);
    assert_eq!(rows_at(&f, EXTRA).await, 1);
    let row = f.row(EXTRA).await;
    assert_eq!((row.id, row.type_), (planned.id, planned.type_));
    assert!(row.is_locked, "the locked row was kept");
    assert_eq!(
        (row.extra_type, row.owner_id),
        (planned.extra_type, planned.owner_id)
    );
    assert!(f.media.join(EXTRA).exists());
    f.assert_browse(2).await;
}

#[tokio::test]
async fn exclusions_are_not_pruned_after_failed_listing_missing_mount_or_cancellation() {
    use ferrofin_core::library_scan::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::MetadataRefreshOptions;
    let ignored = "Heat (1995)/._Heat.mkv";
    let f = Fixture::new(&[MOVIE, ignored]).await;
    f.scanner.scan_all().await.unwrap();
    let id = f.legacy(ignored, false).await;
    *f.fs.fail.lock().unwrap() = Some(f.media.join("Heat (1995)").to_string_lossy().into_owned());
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    assert!(f.has(&id).await);
    *f.fs.fail.lock().unwrap() = None;
    let parked = f.tmp.path().join("parked");
    std::fs::rename(&f.media, &parked).unwrap();
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    std::fs::create_dir(&f.media).unwrap();
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    assert!(f.has(&id).await);
    std::fs::remove_dir(&f.media).unwrap();
    std::fs::rename(&parked, &f.media).unwrap();
    let cancel = ScanCancel::new();
    let at_end = {
        let cancel = cancel.clone();
        move |pct: f64| {
            if pct >= 96.0 {
                cancel.cancel();
            }
        }
    };
    let options = MetadataRefreshOptions::default();
    let result = f
        .scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_progress(&at_end),
        )
        .await
        .unwrap();
    assert!(result.stopped);
    assert_eq!(result.removed, 0);
    assert!(f.has(&id).await);
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 1);
}

async fn play(db: &Database, item: &str) {
    let user = "0000000A-0000-0000-0000-00000000000A";
    sqlx::query(
        r#"INSERT OR IGNORE INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'u', 'U')"#,
    )
    .bind(user)
    .execute(db.writer())
    .await
    .expect("user");
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?1, 0, 1, 0, 1)"#,
    )
    .bind(item)
    .bind(user)
    .execute(db.writer())
    .await
    .expect("user data");
}

#[tokio::test]
async fn plain_space_samples_follow_multi_item_resolution() {
    let path = "Heat (1995)/Bonus sample.mkv";
    for scoped in [false, true] {
        let f = Fixture::new(&[
            MOVIE,
            path,
            "Heat (1995)/Heat-sample.mkv",
            "Loose sample.mkv",
            "Mixed/One.mkv",
            "Mixed/Two.mkv",
            "Mixed/Bonus sample.mkv",
            "Sample Title/Sample Title.mkv",
        ])
        .await;
        f.scanner.scan_all().await.unwrap();
        assert!(f.row("Loose sample.mkv").await.extra_type.is_none());
        assert_eq!(
            f.row("Heat (1995)/Heat-sample.mkv").await.extra_type,
            Some(7)
        );
        let old = f.legacy(path, false).await;
        if scoped {
            f.scanner
                .scan_paths(&[f.media.join(path).to_string_lossy().into_owned()])
                .await
                .unwrap();
        } else {
            f.scanner.scan_all().await.unwrap();
        }
        assert!(!f.has(&old).await);
        assert!(f.media.join(path).exists());
        assert!(
            f.row("Sample Title/Sample Title.mkv")
                .await
                .extra_type
                .is_none()
        );
        let mixed_sample: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Path = ?")
            .bind(
                f.media
                    .join("Mixed/Bonus sample.mkv")
                    .to_string_lossy()
                    .as_ref(),
            )
            .fetch_one(f.db.pool())
            .await
            .unwrap();
        assert_eq!(mixed_sample, 0);
        f.assert_browse(5).await;
        assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Full/scoped repair, unavailable directory and play history in one fixture.
async fn series_and_season_extras_are_owned_and_legacy_episodes_are_repaired() {
    for scoped in [false, true] {
        let extra_path = "Show/Season 01/Extras/bonus.mkv";
        let f = Fixture::with_type(
            &[
                "Show/Season 01/Show S01E01.mkv",
                "Show/Extras/bonus.mkv",
                "Show/Show-trailer.mkv",
                "Show/theme.mp3",
                "Show/trailer.mkv",
                "Show/Extras/sample.mkv",
                extra_path,
                "Show/Season 01/Season 01-trailer.mkv",
                "Show/Season 01/theme.mp3",
                "Show/Extras/nested/ignored.mkv",
            ],
            CollectionTypeOptions::tvshows,
        )
        .await;
        f.scanner.scan_all().await.unwrap();
        let series = f.row("Show").await;
        let season = f.row("Show/Season 01").await;
        for (owner, paths) in [
            (
                &series.id,
                vec![
                    "Show/Extras/bonus.mkv",
                    "Show/Show-trailer.mkv",
                    "Show/theme.mp3",
                ],
            ),
            (
                &season.id,
                vec![
                    extra_path,
                    "Show/Season 01/Season 01-trailer.mkv",
                    "Show/Season 01/theme.mp3",
                ],
            ),
        ] {
            for path in paths {
                let row = f.row(path).await;
                assert_eq!(row.owner_id.as_ref(), Some(owner));
                assert!(row.parent_id.is_none() && row.top_parent_id.is_none());
                assert!(row.extra_type.is_some());
            }
        }
        assert_eq!(
            f.row("Show/theme.mp3").await.name.as_deref(),
            Some("Theme Song")
        );
        assert_eq!(
            f.row("Show/trailer.mkv").await.name.as_deref(),
            Some("Trailer 2")
        );
        assert!(f.row("Show/Extras/bonus.mkv").await.is_in_mixed_folder);
        assert!(!f.row(extra_path).await.is_in_mixed_folder);
        std::fs::remove_file(f.media.join("Show/Show-trailer.mkv")).unwrap();
        f.scanner.scan_all().await.unwrap();
        assert_eq!(
            f.row("Show/trailer.mkv").await.name.as_deref(),
            Some("Trailer")
        );
        let episode = f.row("Show/Season 01/Show S01E01.mkv").await;
        assert_eq!(episode.parent_id.as_ref(), Some(&season.id));
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Type LIKE '%.Episode'")
                .fetch_one(f.db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Path LIKE '%ignored.mkv'")
                .fetch_one(f.db.pool())
                .await
                .unwrap();
        assert_eq!(count, 0);
        // Simulate the old scanner's Episode at the same path, including
        // locked metadata and episode grouping columns. It moves to the id
        // the extra's `Video` kind derives (owner decision D9b) with its
        // metadata and watch state.
        let mut old = f.row(extra_path).await;
        let planned_id = old.id.clone();
        f.store
            .delete_items(&[Uuid::parse_str(&old.id).unwrap()])
            .await
            .unwrap();
        let old_id = guid_to_db(Uuid::new_v4());
        old.id = old_id.clone();
        old.type_ = "MediaBrowser.Controller.Entities.TV.Episode".into();
        old.owner_id = None;
        old.extra_type = None;
        let mut bogus = season.clone();
        bogus.id = guid_to_db(Uuid::new_v4());
        bogus.path = Some(
            f.media
                .join("Show/Season 01/Extras")
                .to_string_lossy()
                .into_owned(),
        );
        bogus.name = Some("Extras".into());
        let bogus_id = Uuid::parse_str(&bogus.id).unwrap();
        old.parent_id = Some(bogus.id.clone());
        f.store.save_items(&[bogus]).await.unwrap();
        old.top_parent_id = Some(guid_to_db(f.library));
        old.series_presentation_unique_key = Some("old-series".into());
        old.index_number = Some(42);
        old.parent_index_number = Some(1);
        old.name = Some("Kept title".into());
        old.is_locked = true;
        f.store.save_items(&[old]).await.unwrap();
        f.store
            .add_locked_fields(
                Uuid::parse_str(&old_id).unwrap(),
                &[ferrofin_db::enums::metadata_field::to_i32(
                    ferrofin_model::entities::MetadataField::Name,
                )],
            )
            .await
            .unwrap();
        play(&f.db, &old_id).await;
        *f.fs.fail.lock().unwrap() = Some(
            f.media
                .join("Show/Season 01/Extras")
                .to_string_lossy()
                .into_owned(),
        );
        f.scanner.scan_all().await.unwrap();
        assert!(f.repo.retrieve_item(bogus_id).await.unwrap().is_some());
        assert_eq!(f.row(extra_path).await.id, old_id);
        *f.fs.fail.lock().unwrap() = None;
        if scoped {
            f.scanner
                .scan_paths(&[f.media.join(extra_path).to_string_lossy().into_owned()])
                .await
                .unwrap();
        } else {
            f.scanner.scan_all().await.unwrap();
        }
        let repaired = f.row(extra_path).await;
        assert_eq!(repaired.id, planned_id);
        assert!(
            f.repo
                .retrieve_item(Uuid::parse_str(&old_id).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(repaired.name.as_deref(), Some("Kept title"));
        assert!(repaired.is_locked);
        assert_eq!(repaired.owner_id.as_ref(), Some(&season.id));
        assert_eq!(repaired.type_, "MediaBrowser.Controller.Entities.Video");
        assert!(repaired.series_presentation_unique_key.is_none());
        assert_eq!(repaired.index_number, Some(42)); // locked user metadata
        let history: (i64, i64) =
            sqlx::query_as("SELECT Played, PlayCount FROM UserData WHERE ItemId=?")
                .bind(&planned_id)
                .fetch_one(f.db.pool())
                .await
                .unwrap();
        assert_eq!(history, (1, 1));
        // An exact-file scan cannot prune the surrounding season; the next
        // full scan removes it after the extra has been reparented.
        f.scanner.scan_all().await.unwrap();
        assert!(f.repo.retrieve_item(bogus_id).await.unwrap().is_none());
        assert_eq!(f.row(extra_path).await.id, planned_id);
        assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    }
}

/// A music video an older scan stored as a `Movie` under an id of its own
/// moves to the id its `MusicVideo` kind derives (owner decision D9b), its
/// play history with it, and owns its extras there.
#[tokio::test]
async fn musicvideo_library_owns_extras_and_moves_an_older_movie_row() {
    let main = "Artist/Artist - Song (2020).mkv";
    let extra = "Artist/Extras/clip.mkv";
    let f = Fixture::with_type(
        &[
            main,
            extra,
            "Artist/theme.mp3",
            "Artist/Artist - Song (2020)-trailer.mkv",
        ],
        CollectionTypeOptions::musicvideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let mut owner = f.row(main).await;
    let derived = owner.id.clone();
    assert_eq!(owner.type_, "MediaBrowser.Controller.Entities.MusicVideo");
    assert_eq!(owner.name.as_deref(), Some("Artist - Song (2020)"));
    assert!(!owner.is_movie);
    assert_eq!(f.row(extra).await.owner_id.as_ref(), Some(&owner.id));
    // Simulate a pre-fix Movie with an identity already used by clients.
    f.store
        .delete_items(&[Uuid::parse_str(&owner.id).unwrap()])
        .await
        .unwrap();
    owner.id = guid_to_db(Uuid::new_v4());
    owner.type_ = "MediaBrowser.Controller.Entities.Movies.Movie".into();
    owner.is_movie = true;
    f.store
        .save_items(std::slice::from_ref(&owner))
        .await
        .unwrap();
    play(&f.db, &owner.id).await;
    f.scanner.scan_all().await.unwrap();
    let repaired = f.row(main).await;
    assert_eq!(repaired.id, derived);
    assert_eq!(
        repaired.type_,
        "MediaBrowser.Controller.Entities.MusicVideo"
    );
    assert!(
        f.repo
            .retrieve_item(Uuid::parse_str(&owner.id).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.row(extra).await.owner_id.as_ref(), Some(&derived));
    f.assert_browse(1).await;
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    assert_eq!(f.row(main).await.id, derived);
    let history: Vec<(String, i64)> = sqlx::query_as("SELECT ItemId, PlayCount FROM UserData")
        .fetch_all(f.db.pool())
        .await
        .unwrap();
    assert_eq!(history, vec![(derived, 1)]);
}

#[tokio::test]
async fn tv_extras_match_the_owner_after_media_specific_rule_selection() {
    let f = Fixture::with_type(
        &[
            "Show/Season 01/Show S01E01.mkv",
            "Show/Extras/theme.mp3",
            "Show/theme-music/unrelated-trailer.mkv",
            "Show/theme-music/Show-trailer.mkv",
            "Show/Extras/bonus-trailer.mkv",
            "Show/theme.mp3",
        ],
        CollectionTypeOptions::tvshows,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let owner = f.row("Show").await;
    for rejected in [
        "Show/Extras/theme.mp3",
        "Show/theme-music/unrelated-trailer.mkv",
    ] {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Path=?")
            .bind(f.media.join(rejected).to_str().unwrap())
            .fetch_one(f.db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "{rejected}");
    }
    for accepted in [
        "Show/theme-music/Show-trailer.mkv",
        "Show/Extras/bonus-trailer.mkv",
        "Show/theme.mp3",
    ] {
        assert_eq!(
            f.row(accepted).await.owner_id.as_ref(),
            Some(&owner.id),
            "{accepted}"
        );
    }
    assert_eq!(
        f.row("Show/Extras/bonus-trailer.mkv").await.extra_type,
        Some(0)
    );
}

#[tokio::test]
async fn musicvideo_owner_grouping_does_not_parse_filename_years() {
    let f = Fixture::with_type(
        &[
            "Artist/Artist - Song (2020).mkv",
            "Artist/Artist - Song (2021).mkv",
            "Artist/Extras/clip.mkv",
        ],
        CollectionTypeOptions::musicvideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let extra = f.row("Artist/Extras/clip.mkv").await;
    assert!(extra.owner_id.is_some());
    assert!(extra.parent_id.is_none());
    for path in [
        "Artist/Artist - Song (2020).mkv",
        "Artist/Artist - Song (2021).mkv",
    ] {
        assert!(!f.row(path).await.is_in_mixed_folder);
    }
}

/// Two rows stored at a music video's path — an adopted `Video` its user
/// watched and put in a playlist, and a duplicate `Movie` an older scan
/// stored first, each under an id of its own. A scan — full, or path-scoped
/// to a new extra of it — keeps the row users made the most of: the `Video`
/// moves to the id the `MusicVideo` kind derives with its watch state and
/// playlist entry, the `Movie` folds into it and is gone, and the new extra
/// is owned there.
#[tokio::test]
async fn musicvideo_duplicate_rows_fold_into_the_watched_one() {
    for scoped in [false, true] {
        let main = "Artist/Artist - Song.mkv";
        let f = Fixture::with_type(&[main], CollectionTypeOptions::musicvideos).await;
        f.scanner.scan_all().await.unwrap();
        let derived = f.row(main).await.id;
        let (adopted, user, playlist) = as_stored_kind(&f, main, BaseItemKind::Video, |row| {
            row.id = guid_to_db(Uuid::new_v4());
        })
        .await;
        let duplicate = store_duplicate(&f, main, BaseItemKind::Movie).await;
        let extra = "Artist/Artist - Song-trailer.mkv";
        std::fs::write(f.media.join(extra), b"").unwrap();
        scan_after_adding(&f, extra, scoped).await;
        assert_moved(
            &f,
            main,
            BaseItemKind::MusicVideo,
            adopted,
            (user, playlist),
        )
        .await;
        assert!(f.repo.retrieve_item(duplicate).await.unwrap().is_none());
        assert_eq!(f.row(main).await.id, derived);
        assert_eq!(rows_at(&f, main).await, 1);
        assert_eq!(f.row(extra).await.owner_id.as_ref(), Some(&derived));
        f.scanner.scan_all().await.unwrap();
        assert_eq!(f.row(main).await.id, derived);
        assert_eq!(f.row(extra).await.owner_id.as_ref(), Some(&derived));
    }
}

/// Two rows at one path both played: the one played last is kept, and the
/// other's user data folds into it — a user's row for both kept by the
/// later `LastPlayedDate`, a user's row only the other has moved over.
#[tokio::test]
async fn duplicate_rows_both_played_fold_into_the_one_played_last() {
    let main = "Heat (1995)/Heat.mkv";
    let f = Fixture::new(&[main]).await;
    f.scanner.scan_all().await.unwrap();
    let derived = f.row(main).await.id;
    let (older, both, _) = as_stored_kind(&f, main, BaseItemKind::Video, |row| {
        row.id = guid_to_db(Uuid::new_v4());
    })
    .await;
    let later = store_duplicate(&f, main, BaseItemKind::MusicVideo).await;
    let only = seed_user(&f.db).await;
    for (item, user, played, count) in [
        (older, both, "2020-01-01 00:00:00", 7),
        (later, both, "2024-01-01 00:00:00", 2),
        (older, only, "2021-01-01 00:00:00", 4),
    ] {
        sqlx::query(
            r#"INSERT OR REPLACE INTO "UserData" ("ItemId", "UserId", "CustomDataKey",
               "IsFavorite", "LastPlayedDate", "PlayCount", "PlaybackPositionTicks", "Played")
               VALUES (?1, ?2, ?3, 0, ?4, ?5, 0, 1)"#,
        )
        .bind(guid_to_db(item))
        .bind(guid_to_db(user))
        .bind(item.to_string())
        .bind(played)
        .bind(count)
        .execute(f.db.writer())
        .await
        .unwrap();
    }
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(main).await.id, derived);
    assert_eq!(rows_at(&f, main).await, 1);
    for gone in [older, later] {
        assert!(f.repo.retrieve_item(gone).await.unwrap().is_none());
    }
    let id = Uuid::parse_str(&derived).unwrap();
    let mut kept: Vec<(String, String, String, i64)> = sqlx::query_as(
        r#"SELECT "ItemId", "UserId", "CustomDataKey", "PlayCount" FROM "UserData""#,
    )
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    kept.sort();
    let mut expected = vec![
        (derived.clone(), guid_to_db(both), id.to_string(), 2),
        (derived.clone(), guid_to_db(only), id.to_string(), 4),
    ];
    expected.sort();
    assert_eq!(kept, expected);
}

/// A row at the path of an item stored under its own id already — of
/// another kind, or of its kind under an id it does not derive — folds into
/// it, its watch state and playlist entry with it, and is gone, never
/// pruned with them. The scan's row is bare, so the watched row is the one
/// kept: it takes the item's id, the scan's row folded into it.
#[tokio::test]
async fn a_duplicate_row_beside_a_stored_item_folds_into_it() {
    for kind in [
        "MediaBrowser.Controller.Entities.Video",
        "MediaBrowser.Controller.Entities.Movies.Movie",
    ] {
        let f = Fixture::new(&[MOVIE]).await;
        f.scanner.scan_all().await.unwrap();
        let movie = f.row(MOVIE).await.id;
        let (stray, user, playlist) = stray_beside(&f, kind, "the watched copy").await;
        f.scanner.scan_all().await.unwrap();
        assert_eq!(rows_at(&f, MOVIE).await, 1, "{kind}");
        assert!(f.repo.retrieve_item(stray).await.unwrap().is_none());
        assert_moved(&f, MOVIE, BaseItemKind::Movie, stray, (user, playlist)).await;
        let row = f.row(MOVIE).await;
        assert_eq!(row.id, movie);
        assert_eq!(row.overview.as_deref(), Some("the watched copy"), "{kind}");
    }
}

/// A stored item a user edited (locked) keeps its row when a watched
/// duplicate stands beside it: the duplicate's watch state and playlist
/// entry fold into it, the edit stays.
#[tokio::test]
async fn an_edited_stored_item_keeps_its_row_and_takes_a_duplicates_watch_state() {
    let f = Fixture::new(&[MOVIE]).await;
    f.scanner.scan_all().await.unwrap();
    let mut movie = f.row(MOVIE).await;
    movie.is_locked = true;
    movie.overview = Some("my edit".to_owned());
    f.store
        .save_items(std::slice::from_ref(&movie))
        .await
        .unwrap();
    let (stray, user, playlist) =
        stray_beside(&f, "MediaBrowser.Controller.Entities.Video", "a stray").await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(rows_at(&f, MOVIE).await, 1);
    assert_moved(&f, MOVIE, BaseItemKind::Movie, stray, (user, playlist)).await;
    let row = f.row(MOVIE).await;
    assert_eq!(
        (row.id, row.overview.as_deref()),
        (movie.id, Some("my edit"))
    );
}

/// Stores a copy of the row at [`MOVIE`] as `kind` under an id of its own,
/// with `overview`, played and favourited by a new user and in a new
/// playlist; returns (its id, user, playlist).
async fn stray_beside(f: &Fixture, kind: &str, overview: &str) -> (Uuid, Uuid, Uuid) {
    let mut stray = f.row(MOVIE).await;
    let id = Uuid::new_v4();
    stray.id = guid_to_db(id);
    kind.clone_into(&mut stray.type_);
    stray.overview = Some(overview.to_owned());
    f.store.save_items(&[stray]).await.unwrap();
    let user = seed_user(&f.db).await;
    play_as(&f.db, id, user).await;
    let playlist = link_in_playlist(f, id).await;
    (id, user, playlist)
}

/// Two rows at a theme song's path (an extra no re-key covers): the scan
/// reuses the watched one's id and folds the other into it. A fold that
/// fails leaves the other row as it was — out of the prune, its playlist
/// entry kept — and the next scan folds it.
#[tokio::test]
async fn a_failed_extra_fold_keeps_the_other_row_until_it_folds() {
    let theme = "Heat (1995)/theme.mp3";
    let f = Fixture::new(&[MOVIE, theme]).await;
    f.scanner.scan_all().await.unwrap();
    let (watched, user, _) = as_stored_kind(&f, theme, BaseItemKind::Audio, |row| {
        row.id = guid_to_db(Uuid::new_v4());
    })
    .await;
    let other = store_duplicate(&f, theme, BaseItemKind::Audio).await;
    let playlist = link_in_playlist(&f, other).await;
    // A fold re-points the other row's playlist entry: make that fail.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"CREATE TRIGGER "fail_fold" BEFORE UPDATE ON "LinkedChildren"
           WHEN OLD."ChildId" = '{}' BEGIN SELECT RAISE(ABORT, 'injected'); END"#,
        guid_to_db(other)
    )))
    .execute(f.db.writer())
    .await
    .unwrap();
    for _ in 0..2 {
        f.scanner.scan_all().await.unwrap();
        assert!(f.repo.retrieve_item(watched).await.unwrap().is_some());
        assert!(
            f.repo.retrieve_item(other).await.unwrap().is_some(),
            "an unfolded row is not pruned"
        );
    }
    sqlx::query(r#"DROP TRIGGER "fail_fold""#)
        .execute(f.db.writer())
        .await
        .unwrap();
    f.scanner.scan_all().await.unwrap();
    assert_eq!(rows_at(&f, theme).await, 1);
    assert!(f.repo.retrieve_item(other).await.unwrap().is_none());
    let linked: Vec<String> =
        sqlx::query_scalar(r#"SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = ?1"#)
            .bind(guid_to_db(playlist))
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    assert_eq!(linked, vec![guid_to_db(watched)]);
    let kept: Vec<String> =
        sqlx::query_scalar(r#"SELECT "ItemId" FROM "UserData" WHERE "UserId" = ?1"#)
            .bind(guid_to_db(user))
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    assert_eq!(kept, vec![guid_to_db(watched)]);
}

/// Scans after `relative` was added: a path-scoped scan of it, or a full one.
async fn scan_after_adding(f: &Fixture, relative: &str, scoped: bool) {
    if scoped {
        f.scanner
            .scan_paths(&[f.media.join(relative).to_string_lossy().into_owned()])
            .await
            .unwrap();
    } else {
        f.scanner.scan_all().await.unwrap();
    }
}

/// Stores a copy of the row at `relative` as `kind` under an id of its own;
/// returns that id.
async fn store_duplicate(f: &Fixture, relative: &str, kind: BaseItemKind) -> Uuid {
    let mut row = f.row(relative).await;
    let id = Uuid::new_v4();
    row.id = guid_to_db(id);
    stored_type_name(kind).unwrap().clone_into(&mut row.type_);
    f.store.save_items(&[row]).await.unwrap();
    id
}

/// How many rows are stored at `relative`.
async fn rows_at(f: &Fixture, relative: &str) -> i64 {
    sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Path" = ?1"#)
        .bind(f.media.join(relative).to_string_lossy().as_ref())
        .fetch_one(f.db.pool())
        .await
        .unwrap()
}

/// `user` favourited and played `item` (its guid-keyed row).
async fn play_as(db: &Database, item: Uuid, user: Uuid) {
    sqlx::query(
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
           "PlayCount", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, ?3, 1, 1, 0, 1)"#,
    )
    .bind(guid_to_db(item))
    .bind(guid_to_db(user))
    .bind(item.to_string())
    .execute(db.writer())
    .await
    .unwrap();
}

/// A new playlist holding `item`; returns its id.
async fn link_in_playlist(f: &Fixture, item: Uuid) -> Uuid {
    use ferrofin_core::FerrofinLinkedChildrenService;
    use ferrofin_traits::persistence::LinkedChildrenService as _;

    let playlist = Uuid::new_v4();
    let list = BaseItemEntity {
        id: guid_to_db(playlist),
        type_: stored_type_name(BaseItemKind::Playlist).unwrap().to_owned(),
        name: Some("list".to_owned()),
        is_folder: true,
        ..BaseItemEntity::default()
    };
    f.store.save_items(&[list]).await.unwrap();
    FerrofinLinkedChildrenService::new(f.db.clone())
        .upsert_linked_child(playlist, item, 0)
        .await
        .unwrap();
    playlist
}

/// An extra in an extras-type subfolder is `IsInMixedFolder` when that
/// folder holds more than one file, whatever the files are, as upstream's
/// `FindExtras` stores it (`LibraryManager.cs:3459-3475`); beside its owner it
/// never is (`:3478`). Below the extras folder, and in it besides the extras,
/// nothing resolves (`CoreResolutionIgnoreRule.cs:56-61`) — not a disc rip,
/// not a video no extra rule takes — and a row an older scan planned there is
/// pruned.
#[tokio::test]
async fn an_extras_subfolder_holding_several_files_is_a_mixed_folder() {
    let f = Fixture::new(&[
        "Heat (1995)/Heat (1995).mkv",
        "Heat (1995)/trailers/one.mkv",
        "Heat (1995)/trailers/two.mkv",
        "Heat (1995)/trailers/nested/three.mkv",
        "Heat (1995)/extras/BDMV/STREAM/00000.m2ts",
        "Heat (1995)/theme-music/clip.mkv",
        "Alien (1979)/Alien (1979).mkv",
        "Alien (1979)/Alien (1979)-trailer.mkv",
        "Alien (1979)/featurettes/only.mkv",
        "Blade (1998)/Blade (1998).mkv",
        "Blade (1998)/interviews/one.mkv",
        "Blade (1998)/interviews/one.nfo",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();

    let heat = f.row("Heat (1995)/Heat (1995).mkv").await;
    let blade = f.row("Blade (1998)/Blade (1998).mkv").await;
    for (path, owner) in [
        ("Heat (1995)/trailers/one.mkv", &heat),
        ("Heat (1995)/trailers/two.mkv", &heat),
        ("Blade (1998)/interviews/one.mkv", &blade),
    ] {
        let extra = f.row(path).await;
        assert_eq!(extra.owner_id.as_ref(), Some(&owner.id), "{path}");
        assert!(extra.is_in_mixed_folder, "{path}");
    }
    let alien = f.row("Alien (1979)/Alien (1979).mkv").await;
    for path in [
        "Alien (1979)/featurettes/only.mkv",
        "Alien (1979)/Alien (1979)-trailer.mkv",
    ] {
        let extra = f.row(path).await;
        assert_eq!(extra.owner_id.as_ref(), Some(&alien.id), "{path}");
        assert!(!extra.is_in_mixed_folder, "{path}");
    }
    let unresolved = || async {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM BaseItems WHERE Path LIKE '%three.mkv' \
             OR Path LIKE '%clip.mkv' OR Path LIKE '%/extras' OR Path LIKE '%.m2ts'",
        )
        .fetch_one(f.db.pool())
        .await
        .unwrap();
        count
    };
    assert_eq!(
        unresolved().await,
        0,
        "nothing else in an extras folder resolves"
    );

    // What an older scan planned below the extras folder goes.
    let mut stale = heat.clone();
    stale.id = guid_to_db(Uuid::from_u128(0x3EE));
    stale.path = Some(
        f.media
            .join("Heat (1995)/trailers/nested/three.mkv")
            .to_string_lossy()
            .into_owned(),
    );
    f.store.save_items(&[stale]).await.unwrap();
    assert_eq!(unresolved().await, 1);
    f.scanner.scan_all().await.unwrap();
    assert_eq!(unresolved().await, 0, "the older row is pruned");
}

/// Upstream `MovieResolverTests.ResolvePath_MovieFolderWithRealSubfolder_DoesNotResolveToSingleMovie`
/// (its fixture): a folder holding a real subfolder is no movie's own folder.
/// Derived from `MovieResolver.ResolveVideos` (`MovieResolver.cs:287-317`):
/// its video is then the folder's one item below the top level, so not in a
/// mixed folder, named after the folder its file is named after (a version
/// group of one, `VideoListResolver.GetVideosGroupedByVersion`,
/// `VideoListResolver.cs:121-158`), and the owner of the folder's
/// extras (`BaseItem.SearchesContainingFolderForExtras`) — those beside it and
/// in an extras-named subfolder, not those in another plain subfolder
/// (`FindExtras`, `LibraryManager.cs:3459`).
#[tokio::test]
async fn a_movie_folder_with_a_real_subfolder_holds_a_lone_unmixed_video() {
    let f = Fixture::new(&[
        "Outer Colony (2026)/Outer Colony (2026).mkv",
        "Outer Colony (2026)/Feature/notes.txt",
        "Outer Colony (2026)/trailers/teaser.mkv",
        "Outer Colony (2026)/Promo/trailer.mp4",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let lone = f.row("Outer Colony (2026)/Outer Colony (2026).mkv").await;
    assert!(!lone.is_in_mixed_folder);
    assert_eq!(lone.name.as_deref(), Some("Outer Colony (2026)"));
    assert_eq!(
        f.row("Outer Colony (2026)/trailers/teaser.mkv")
            .await
            .owner_id
            .as_ref(),
        Some(&lone.id)
    );
    let promo: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Path LIKE '%/Promo/%'")
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(promo, 0, "an extra in a plain subfolder has no owner");
}

/// A folder whose only resolved item is an extra falls back to resolving
/// each file on its own, as a mixed-folder video (`ResolvePaths` →
/// `MovieResolver.cs:167-186`): `Sample.Movie.mkv`, left out of the count by
/// `\bsample\b` and taken by no extra rule, is a movie in a mixed folder.
#[tokio::test]
async fn a_folder_resolving_to_one_extra_is_a_mixed_folder() {
    let f = Fixture::new(&["X/trailer.mkv", "X/Sample.Movie.mkv", "X/Sub/notes.txt"]).await;
    f.scanner.scan_all().await.unwrap();
    assert!(f.row("X/Sample.Movie.mkv").await.is_in_mixed_folder);
}

/// A home-video library does not group versions (`ResolveVideos<Video>(…,
/// supportMultiEditions: false, …)`, `MovieResolver.cs:206-209`): two cuts of
/// one clip beside a real subfolder are two items, so both are mixed.
#[tokio::test]
async fn home_video_cuts_are_not_grouped_into_one_item() {
    let f = Fixture::with_type(
        &[
            "Trip/Clip - 1080p.mkv",
            "Trip/Clip - 720p.mkv",
            "Trip/Day 2/a.mkv",
        ],
        CollectionTypeOptions::homevideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    for path in ["Trip/Clip - 1080p.mkv", "Trip/Clip - 720p.mkv"] {
        assert!(f.row(path).await.is_in_mixed_folder, "{path}");
    }
}

/// An extras-named folder directly under the library root is a plain folder
/// no rule ignores (`CoreResolutionIgnoreRule`: top-level folders are never
/// ignored); its files resolve with the folder as their library root, so the
/// folder's name makes none of them an extra — they are movies, a lone one
/// not in a mixed folder, several in one.
#[tokio::test]
async fn an_extras_named_folder_at_the_top_holds_movies() {
    let f = Fixture::new(&[
        "trailers/Lone.mkv",
        "featurettes/A.mkv",
        "featurettes/B.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    for (path, mixed) in [
        ("trailers/Lone.mkv", false),
        ("featurettes/A.mkv", true),
        ("featurettes/B.mkv", true),
    ] {
        let row = f.row(path).await;
        assert!(row.type_.ends_with(".Movie"), "{path}: {}", row.type_);
        assert_eq!(row.owner_id, None, "{path}");
        assert_eq!(row.is_in_mixed_folder, mixed, "{path}");
    }
}

/// A home-video library resolves as upstream's does (`MovieResolver`,
/// `PhotoAlbumResolver`): a clip is a `Video` named after its file, with no
/// year, never a movie in its own folder (`isPhotosCollection`), and in a
/// mixed folder at the library root or beside other clips; a folder holding
/// photos is a `PhotoAlbum` its clips hang off and that owns the folder's
/// extras; a disc rip is one `Video` for its folder.
#[tokio::test]
async fn a_home_video_library_resolves_clips_albums_and_rips() {
    let f = Fixture::with_type(
        &[
            "Clip.mp4",
            "Trip (2019)/Day 1 (2019).mp4",
            "Album/IMG_0001.jpg",
            "Album/Beach.mp4",
            "Album/extras/Bloopers.mp4",
            "Album/extras/Still.jpg",
            "Album/Inner/IMG_0002.jpg",
            "Album/Inner/Swim.mp4",
            "Album/Inner/Deep/IMG_0004.jpg",
            "Album/Inner/Deep/Dive.mp4",
            "Album/Plain/Walk.mp4",
            "Album/Plain/extras/Bloop.mp4",
            "Album/AlbumRip/VIDEO_TS/VTS_01_1.VOB",
            "Pair/IMG_0003.jpg",
            "Pair/One.mp4",
            "Pair/Two.mp4",
            "Rip/VIDEO_TS/VTS_01_1.VOB",
            "Rip/BDMV/META/DL/cover.jpg",
        ],
        CollectionTypeOptions::homevideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let library = guid_to_db(f.library);

    let root = f.row("Clip.mp4").await;
    assert!(root.type_.ends_with("Entities.Video"), "{}", root.type_);
    assert!(root.is_in_mixed_folder, "the library root is a top parent");
    assert!(!root.is_movie);
    assert_eq!(root.parent_id.as_ref(), Some(&library));

    let day = f.row("Trip (2019)/Day 1 (2019).mp4").await;
    assert!(day.type_.ends_with("Entities.Video"), "{}", day.type_);
    assert_eq!(
        day.name.as_deref(),
        Some("Day 1 (2019)"),
        "the raw file name"
    );
    assert_eq!(day.production_year, None, "names are not parsed");
    assert!(!day.is_in_mixed_folder, "the folder's lone clip");
    assert_eq!(day.parent_id.as_ref(), Some(&library));

    let album = f.row("Album").await;
    assert!(album.type_.ends_with(".PhotoAlbum"), "{}", album.type_);
    let beach = f.row("Album/Beach.mp4").await;
    assert_eq!(beach.parent_id.as_ref(), Some(&album.id));
    assert!(!beach.is_in_mixed_folder);
    let bloopers = f.row("Album/extras/Bloopers.mp4").await;
    assert_eq!(
        bloopers.owner_id.as_ref(),
        Some(&album.id),
        "the album owns the folder's extras"
    );

    let rip = f.row("Rip").await;
    assert!(rip.type_.ends_with("Entities.Video"), "{}", rip.type_);
    assert!(!rip.is_in_mixed_folder);
    assert!(!rip.is_movie);
    assert_eq!(rip.name.as_deref(), Some("Rip"));
    assert!(
        rip.data
            .as_deref()
            .unwrap_or_default()
            .contains(r#""VideoType":"Dvd""#)
    );

    // A nested album hangs off its album; its clip off it.
    let inner = f.row("Album/Inner").await;
    assert!(inner.type_.ends_with(".PhotoAlbum"), "{}", inner.type_);
    assert_eq!(inner.parent_id.as_ref(), Some(&album.id));
    let swim = f.row("Album/Inner/Swim.mp4").await;
    assert_eq!(swim.parent_id.as_ref(), Some(&inner.id));
    // Three albums deep, the ancestors are the whole chain.
    let deep = f.row("Album/Inner/Deep").await;
    let dive = f.row("Album/Inner/Deep/Dive.mp4").await;
    assert_eq!(dive.parent_id.as_ref(), Some(&deep.id));
    let ancestors = f.ancestors(&dive.id).await;
    for id in [&album.id, &inner.id, &deep.id, &library] {
        assert!(ancestors.contains(id), "{id} in {ancestors:?}");
    }
    // A clip in a plain folder of an album hangs off the album and, alone
    // in a folder no album is, owns that folder's extras.
    let walk = f.row("Album/Plain/Walk.mp4").await;
    assert_eq!(walk.parent_id.as_ref(), Some(&album.id));
    assert!(!walk.is_in_mixed_folder);
    assert_eq!(
        f.row("Album/Plain/extras/Bloop.mp4")
            .await
            .owner_id
            .as_ref(),
        Some(&walk.id)
    );
    // A rip inside an album hangs off it.
    let album_rip = f.row("Album/AlbumRip").await;
    assert_eq!(album_rip.parent_id.as_ref(), Some(&album.id));
    // Two clips in an album are mixed.
    for path in ["Pair/One.mp4", "Pair/Two.mp4"] {
        assert!(f.row(path).await.is_in_mixed_folder, "{path}");
    }
    // Neither an extras folder nor a rip holds a photo album or photos.
    let unresolved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM BaseItems WHERE Path LIKE '%/extras' \
         OR Path LIKE '%Still.jpg' OR Path LIKE '%/DL' OR Path LIKE '%cover.jpg'",
    )
    .fetch_one(f.db.pool())
    .await
    .unwrap();
    assert_eq!(unresolved, 0);
}

/// Without `EnablePhotos` a home-video library has no albums
/// (`PhotoAlbumResolver.cs:50-51`): a folder's lone clip hangs off the library
/// and owns the folder's extras.
#[tokio::test]
async fn a_home_video_library_without_photos_has_no_albums() {
    let f = Fixture::with_options(
        &[
            "Album/IMG_0001.jpg",
            "Album/Beach.mp4",
            "Album/extras/trailer.mp4",
        ],
        CollectionTypeOptions::homevideos,
        LibraryOptions {
            enable_photos: false,
            ..LibraryOptions::default()
        },
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let beach = f.row("Album/Beach.mp4").await;
    assert_eq!(beach.parent_id, Some(guid_to_db(f.library)));
    assert!(!beach.is_in_mixed_folder);
    assert_eq!(
        f.row("Album/extras/trailer.mp4").await.owner_id.as_ref(),
        Some(&beach.id)
    );
    let albums: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Type LIKE '%PhotoAlbum'")
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(albums, 0);
}

/// A music video's name is its file's, but it keeps the year its name gives
/// (`MusicVideo.BeforeMetadataRefresh`); a home video's carries none.
#[tokio::test]
async fn a_music_video_keeps_the_year_its_name_gives() {
    let f = Fixture::with_type(
        &["Band - Song (1999).mkv"],
        CollectionTypeOptions::musicvideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    let video = f.row("Band - Song (1999).mkv").await;
    assert_eq!(video.name.as_deref(), Some("Band - Song (1999)"));
    assert_eq!(video.production_year, Some(1999));
}

/// A movie or music video whose name gives no year but that is not in a
/// mixed folder takes its folder's (`Movie`/`MusicVideo`
/// `.BeforeMetadataRefresh`), on its first scan as on any later one.
#[tokio::test]
async fn an_undated_video_takes_its_folders_year() {
    let f = Fixture::new(&["Heat (1995)/Heat.mkv", "Ronin (1998)/VIDEO_TS/VTS_01_1.VOB"]).await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(
        f.row("Heat (1995)/Heat.mkv").await.production_year,
        Some(1995)
    );
    assert_eq!(
        f.row("Ronin (1998)").await.production_year,
        Some(1998),
        "a disc rip"
    );

    let f = Fixture::with_type(
        &["Band (2001)/Clip.mkv"],
        CollectionTypeOptions::musicvideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(
        f.row("Band (2001)/Clip.mkv").await.production_year,
        Some(2001)
    );
}

/// Photos an older scan planned inside a disc rip of a home-video library
/// are pruned: below a rip nothing resolves.
#[tokio::test]
async fn photo_rows_inside_a_disc_rip_are_pruned() {
    let f = Fixture::with_type(
        &[
            "Rip/BDMV/STREAM/00000.m2ts",
            "Rip/BDMV/META/DL/cover.jpg",
            "Clip.mp4",
        ],
        CollectionTypeOptions::homevideos,
    )
    .await;
    f.scanner.scan_all().await.unwrap();
    // What an older scan stored: an album at the rip's art folder.
    let mut album = f.row("Clip.mp4").await;
    album.id = guid_to_db(Uuid::from_u128(0xDA));
    album.type_ = "MediaBrowser.Controller.Entities.PhotoAlbum".to_owned();
    album.path = Some(
        f.media
            .join("Rip/BDMV/META/DL")
            .to_string_lossy()
            .into_owned(),
    );
    album.is_folder = true;
    f.store.save_items(&[album]).await.unwrap();
    f.scanner.scan_all().await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Path LIKE '%/DL'")
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A library without a collection type resolves as upstream's does
/// (`SeriesResolver`, `SeasonResolver`, `MovieResolver`, `AudioResolver` with no
/// collection type) — `mixed` and no type alike: a folder holding
/// `tvshow.nfo` (matched exactly) or a season folder is an undated `Series`,
/// named by the naming rules, whose episodes resolve one by one, so none is in
/// a mixed folder, and whose audio is `Audio`; a movie in its own folder is a
/// `Movie` owning its theme song; a folder holding `season.nfo` (or a
/// differently cased `TVShow.nfo`) resolves its files as plain `Video`s, raw
/// names, undated, sample names kept; an audio file is `Audio`; a theme song
/// outside a movie is nothing.
#[tokio::test]
async fn an_untyped_library_resolves_series_movies_videos_and_audio() {
    const FILES: &[&str] = &[
        "Show/tvshow.nfo",
        "Show/Show S01E01.mkv",
        "Show/opening.mp3",
        "Firefly (2002)/Season 1/Firefly S01E01.mkv",
        "Firefly (2002)/Season 1/Firefly S01E02.mkv",
        "Firefly (2002)/Season 1/score.flac",
        "Heat (1995)/Heat.mkv",
        "Heat (1995)/theme.mp3",
        "Notes/season.nfo",
        "Notes/Other Film (2001).mkv",
        "Notes/A sample.mkv",
        "Cased/TVShow.nfo",
        "Cased/Cased S01E01.mkv",
        "Root Movie (2001).mkv",
        "song.mp3",
        "theme.mp3",
    ];
    let typed = Fixture::with_type(FILES, CollectionTypeOptions::mixed).await;
    let untyped = Fixture::untyped(FILES).await;
    for f in [typed, untyped] {
        f.scanner.scan_all().await.unwrap();
        assert_untyped_library(&f).await;
    }
}

async fn assert_untyped_library(f: &Fixture) {
    let kind = |row: &BaseItemEntity| row.type_.rsplit('.').next().unwrap_or_default().to_owned();
    let firefly = f.row("Firefly (2002)").await;
    assert_eq!(kind(&firefly), "Series");
    assert_eq!(
        firefly.name.as_deref(),
        Some("Firefly"),
        "named by the naming rules"
    );
    assert_eq!(
        firefly.production_year, None,
        "an untyped series is undated"
    );
    let season = f.row("Firefly (2002)/Season 1").await;
    assert_eq!(kind(&season), "Season");
    assert_eq!(season.index_number, Some(1));
    assert_eq!(season.parent_id.as_ref(), Some(&firefly.id));
    for path in [
        "Firefly (2002)/Season 1/Firefly S01E01.mkv",
        "Firefly (2002)/Season 1/Firefly S01E02.mkv",
    ] {
        let episode = f.row(path).await;
        assert_eq!(kind(&episode), "Episode", "{path}");
        assert!(!episode.is_in_mixed_folder, "{path}");
        assert_eq!(episode.parent_id.as_ref(), Some(&season.id));
        assert_eq!(episode.series_id.as_ref(), Some(&firefly.id));
        assert_eq!(
            episode.series_presentation_unique_key,
            firefly.presentation_unique_key
        );
        assert_eq!(episode.parent_index_number, Some(1));
    }
    let score = f.row("Firefly (2002)/Season 1/score.flac").await;
    assert_eq!(kind(&score), "Audio");
    assert_eq!(score.parent_id.as_ref(), Some(&season.id));

    let show = f.row("Show").await;
    assert_eq!(kind(&show), "Series");
    let flat = f.row("Show/Show S01E01.mkv").await;
    assert_eq!(kind(&flat), "Episode");
    assert!(!flat.is_in_mixed_folder);
    let opening = f.row("Show/opening.mp3").await;
    assert_eq!(kind(&opening), "Audio");
    assert_eq!(opening.parent_id.as_ref(), Some(&show.id));

    let heat = f.row("Heat (1995)/Heat.mkv").await;
    assert_eq!(kind(&heat), "Movie");
    assert!(!heat.is_in_mixed_folder);
    assert_eq!(
        f.row("Heat (1995)/theme.mp3").await.owner_id.as_ref(),
        Some(&heat.id)
    );

    for path in ["Notes/Other Film (2001).mkv", "Notes/A sample.mkv"] {
        let video = f.row(path).await;
        assert_eq!(kind(&video), "Video", "{path}");
        assert!(video.is_in_mixed_folder, "{path}");
        assert_eq!(video.production_year, None, "{path}");
    }
    assert_eq!(
        f.row("Notes/Other Film (2001).mkv").await.name.as_deref(),
        Some("Other Film (2001)"),
        "the raw file name"
    );
    let cased = f.row("Cased/Cased S01E01.mkv").await;
    assert_eq!(kind(&cased), "Video", "TVShow.nfo makes no series");

    let root_movie = f.row("Root Movie (2001).mkv").await;
    assert_eq!(kind(&root_movie), "Movie");
    assert!(root_movie.is_in_mixed_folder);
    let song = f.row("song.mp3").await;
    assert_eq!(kind(&song), "Audio");
    assert!(song.is_in_mixed_folder);
    let at = f.media.join("theme.mp3").to_string_lossy().into_owned();
    let themes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM BaseItems WHERE Path = ?1")
        .bind(at)
        .fetch_one(f.db.pool())
        .await
        .unwrap();
    assert_eq!(themes, 0);
}

/// Seeds a user and returns its id.
async fn seed_user(db: &Database) -> Uuid {
    let user = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, ?2, upper(?2))"#,
    )
    .bind(guid_to_db(user))
    .bind(user.simple().to_string())
    .execute(db.writer())
    .await
    .unwrap();
    user
}

/// Turns the item at `relative` into the `kind` row another scan stored for
/// it — the id that kind derives, its type and `IsMovie` — further shaped by
/// `shape` (which may give it another id), favourited by a user and in a
/// playlist; the extras it owned point at it. Returns (its id, user,
/// playlist).
async fn as_stored_kind(
    f: &Fixture,
    relative: &str,
    kind: BaseItemKind,
    shape: impl FnOnce(&mut BaseItemEntity),
) -> (Uuid, Uuid, Uuid) {
    use ferrofin_core::FerrofinLinkedChildrenService;
    use ferrofin_traits::persistence::LinkedChildrenService as _;

    let mut row = f.row(relative).await;
    let mut list = row.clone();
    let mut owned: Vec<BaseItemEntity> =
        sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "OwnerId" = ?1"#)
            .bind(&row.id)
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    let path = row.path.clone().unwrap();
    f.store
        .delete_items(&[Uuid::parse_str(&row.id).unwrap()])
        .await
        .unwrap();
    row.id = guid_to_db(derive_item_id(kind, &path).unwrap());
    stored_type_name(kind).unwrap().clone_into(&mut row.type_);
    row.is_movie = kind == BaseItemKind::Movie;
    if kind != BaseItemKind::Episode {
        row.series_id = None;
        row.season_id = None;
    }
    shape(&mut row);
    let stored = Uuid::parse_str(&row.id).unwrap();
    f.store.save_items(&[row]).await.unwrap();
    for extra in &mut owned {
        extra.owner_id = Some(guid_to_db(stored));
    }
    f.store.save_items(&owned).await.unwrap();
    let user = seed_user(&f.db).await;
    sqlx::query(
        r#"INSERT INTO "UserData" ("ItemId", "UserId", "CustomDataKey", "IsFavorite",
           "PlayCount", "PlaybackPositionTicks", "Played") VALUES (?1, ?2, ?3, 1, 1, 0, 1)"#,
    )
    .bind(guid_to_db(stored))
    .bind(guid_to_db(user))
    .bind(stored.to_string())
    .execute(f.db.writer())
    .await
    .unwrap();
    let playlist = Uuid::new_v4();
    list.id = guid_to_db(playlist);
    stored_type_name(BaseItemKind::Playlist)
        .unwrap()
        .clone_into(&mut list.type_);
    list.is_folder = true;
    list.series_id = None;
    list.season_id = None;
    list.path = None;
    list.parent_id = None;
    f.store.save_items(&[list]).await.unwrap();
    FerrofinLinkedChildrenService::new(f.db.clone())
        .upsert_linked_child(playlist, stored, 0)
        .await
        .unwrap();
    (stored, user, playlist)
}

/// A row's `(Id, Type, Path, OwnerId, ParentId)`.
type IdentityRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Every row but those at `relative`, as `(Id, Type, Path, OwnerId,
/// ParentId)` with `from` read as `to`, sorted: what a move of the item at
/// `relative` from `from` to `to` leaves as it was.
async fn other_rows(
    f: &Fixture,
    relative: &str,
    (from, to): (Uuid, Uuid),
) -> Vec<[Option<String>; 5]> {
    let rows: Vec<IdentityRow> = sqlx::query_as(
        r#"SELECT "Id", "Type", "Path", "OwnerId", "ParentId" FROM "BaseItems"
           WHERE "Path" IS NOT ?1"#,
    )
    .bind(f.media.join(relative).to_string_lossy().as_ref())
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    let (from, to) = (guid_to_db(from), guid_to_db(to));
    let moved = |id: Option<String>| id.map(|id| if id == from { to.clone() } else { id });
    let mut rows: Vec<[Option<String>; 5]> = rows
        .into_iter()
        .map(|(id, kind, path, owner, parent)| {
            [
                moved(Some(id)),
                Some(kind),
                path,
                moved(owner),
                moved(parent),
            ]
        })
        .collect();
    rows.sort();
    rows
}

/// After a rescan, the item at `relative` is `kind` under the id that kind
/// derives, and still the user's favourite and in the playlist; the row
/// under `from` is gone.
async fn assert_moved(
    f: &Fixture,
    relative: &str,
    kind: BaseItemKind,
    from: Uuid,
    (user, playlist): (Uuid, Uuid),
) {
    let row = f.row(relative).await;
    assert_eq!(row.type_, stored_type_name(kind).unwrap());
    assert!(!row.is_movie);
    let id = Uuid::parse_str(&row.id).unwrap();
    assert_eq!(
        id,
        derive_item_id(kind, row.path.as_deref().unwrap()).unwrap()
    );
    assert_ne!(id, from);
    assert!(f.repo.retrieve_item(from).await.unwrap().is_none());
    let kept: Vec<(String, String, bool)> = sqlx::query_as(
        r#"SELECT "ItemId", "CustomDataKey", "IsFavorite" FROM "UserData" WHERE "UserId" = ?1"#,
    )
    .bind(guid_to_db(user))
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(kept, vec![(row.id.clone(), id.to_string(), true)]);
    let linked: Vec<String> =
        sqlx::query_scalar(r#"SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = ?1"#)
            .bind(guid_to_db(playlist))
            .fetch_all(f.db.pool())
            .await
            .unwrap();
    assert_eq!(linked, vec![row.id]);
}

/// Stores the item at `relative` as `stored` (further shaped by `shape`),
/// rescans, and checks it moved to the id `planned` derives with its user
/// data and playlist entry, its old id gone and no other row changed.
async fn assert_kind_change_moves(
    f: &Fixture,
    relative: &str,
    (stored, planned): (BaseItemKind, BaseItemKind),
    shape: impl FnOnce(&mut BaseItemEntity),
) {
    f.scanner.scan_all().await.unwrap();
    let path = f.media.join(relative).to_string_lossy().into_owned();
    let to = derive_item_id(planned, &path).unwrap();
    assert_eq!(
        f.row(relative).await.id,
        guid_to_db(to),
        "planned as {planned:?}"
    );
    let (from, user, playlist) = as_stored_kind(f, relative, stored, shape).await;
    let others = other_rows(f, relative, (from, to)).await;
    f.scanner.scan_all().await.unwrap();
    assert_moved(f, relative, planned, from, (user, playlist)).await;
    assert_eq!(other_rows(f, relative, (from, to)).await, others);
}

/// A clip an older Ferrofin scan stored as a `Movie` — and an episode of an
/// untyped library's series it stored as one — moves to the id its new kind
/// derives (owner decision D4), its watch state and playlist entry with it,
/// instead of being pruned and planned anew.
#[tokio::test]
async fn an_older_scans_movie_keeps_its_user_data_under_its_new_kind() {
    let f = Fixture::with_type(&["Trip/Day 1.mp4"], CollectionTypeOptions::homevideos).await;
    assert_kind_change_moves(
        &f,
        "Trip/Day 1.mp4",
        (BaseItemKind::Movie, BaseItemKind::Video),
        |_| {},
    )
    .await;

    let f = Fixture::untyped(&["Show/Season 1/Show S01E01.mkv"]).await;
    assert_kind_change_moves(
        &f,
        "Show/Season 1/Show S01E01.mkv",
        (BaseItemKind::Movie, BaseItemKind::Episode),
        |_| {},
    )
    .await;
}

/// Every video library re-keys a video whose kind changed (owner decision
/// D9b): an `Episode` a home-video library now resolves as a `Video`, a
/// `Movie` a TV library resolves as an `Episode`, a `MusicVideo` a movie
/// library resolves as a `Movie` (the library's type changed).
#[tokio::test]
async fn a_video_whose_kind_changed_moves_in_every_video_library() {
    let f = Fixture::with_type(&["Show/Show S01E01.mkv"], CollectionTypeOptions::homevideos).await;
    assert_kind_change_moves(
        &f,
        "Show/Show S01E01.mkv",
        (BaseItemKind::Episode, BaseItemKind::Video),
        |_| {},
    )
    .await;

    let f = Fixture::with_type(
        &["Show/Season 1/Show S01E01.mkv"],
        CollectionTypeOptions::tvshows,
    )
    .await;
    assert_kind_change_moves(
        &f,
        "Show/Season 1/Show S01E01.mkv",
        (BaseItemKind::Movie, BaseItemKind::Episode),
        |_| {},
    )
    .await;

    let f = Fixture::new(&["Heat (1995)/Heat.mkv"]).await;
    assert_kind_change_moves(
        &f,
        "Heat (1995)/Heat.mkv",
        (BaseItemKind::MusicVideo, BaseItemKind::Movie),
        |_| {},
    )
    .await;
}

/// A home video an older scan stored as a `Movie` owns its extras there; the
/// move takes them along, and the rescan never re-points them at the `Movie`
/// row the move left behind.
#[tokio::test]
async fn a_moved_owner_keeps_its_extras() {
    let f = Fixture::with_options(
        &["Album/Beach.mp4", "Album/extras/trailer.mp4"],
        CollectionTypeOptions::homevideos,
        LibraryOptions {
            enable_photos: false,
            ..LibraryOptions::default()
        },
    )
    .await;
    assert_kind_change_moves(
        &f,
        "Album/Beach.mp4",
        (BaseItemKind::Movie, BaseItemKind::Video),
        |_| {},
    )
    .await;
    assert_eq!(
        f.row("Album/extras/trailer.mp4").await.owner_id,
        Some(f.row("Album/Beach.mp4").await.id)
    );
}

/// A Jellyfin 10.11 local alternate version — a `Video` owned by its primary
/// `Movie`, no parent, under the id the `Video` kind derives — where the
/// movie walk plans a `Movie` moves to the `Movie` id with its user data:
/// the data-preserving form of upstream's wrong-type delete
/// (`LibraryManager.ResolveAlternateVersion`, `LibraryManager.cs:870-893`;
/// owner decision D9b).
#[tokio::test]
async fn an_adopted_alternate_version_video_moves_to_the_planned_kind() {
    let f = Fixture::new(&["Film/Film.mkv", "Film/Film - 4K.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let primary = f.row("Film/Film.mkv").await.id;
    assert_kind_change_moves(
        &f,
        "Film/Film - 4K.mkv",
        (BaseItemKind::Video, BaseItemKind::Movie),
        |row| {
            row.owner_id = Some(primary.clone());
            row.parent_id = None;
            row.top_parent_id = None;
            row.presentation_unique_key = Some(row.id.to_lowercase().replace('-', ""));
            row.is_in_mixed_folder = false;
        },
    )
    .await;
    assert_eq!(f.row("Film/Film.mkv").await.id, primary);
}

/// A file that became an extra or stopped being one moves too (owner
/// decision, the principle of D4/D9b), its `ExtraType` and `OwnerId` then
/// following the planner: a `Movie` row at a path the scan now plans as a
/// trailer extra becomes that `Trailer`, and a stored extra `Video` at a path
/// the scan now plans as a `Movie` becomes that movie — each with its watch
/// state and playlist entry, where upstream deletes the row and plans anew.
#[tokio::test]
async fn a_file_that_becomes_or_stops_being_an_extra_moves() {
    let trailer = "Heat (1995)/Heat-trailer.mkv";
    let f = Fixture::new(&[MOVIE, trailer]).await;
    f.scanner.scan_all().await.unwrap();
    let movie = f.row(MOVIE).await;
    let planned = f.row(trailer).await;
    assert_kind_change_moves(
        &f,
        trailer,
        (BaseItemKind::Movie, BaseItemKind::Trailer),
        |row| {
            row.extra_type = None;
            row.owner_id = None;
            row.parent_id.clone_from(&movie.parent_id);
            row.top_parent_id.clone_from(&movie.top_parent_id);
        },
    )
    .await;
    let row = f.row(trailer).await;
    assert_eq!(
        (row.extra_type, row.owner_id, row.parent_id),
        (planned.extra_type, Some(movie.id.clone()), None)
    );

    let ronin = "Ronin (1998)/Ronin.mkv";
    let f = Fixture::new(&[MOVIE, ronin]).await;
    f.scanner.scan_all().await.unwrap();
    let owner = f.row(MOVIE).await.id;
    let planned = f.row(ronin).await;
    assert_kind_change_moves(
        &f,
        ronin,
        (BaseItemKind::Video, BaseItemKind::Movie),
        |row| {
            row.extra_type = Some(ferrofin_model::entities::ExtraType::Clip as i32);
            row.owner_id = Some(owner);
            row.parent_id = None;
            row.top_parent_id = None;
        },
    )
    .await;
    let row = f.row(ronin).await;
    assert_eq!(
        (row.extra_type, row.owner_id, row.parent_id),
        (None, None, planned.parent_id)
    );
}
