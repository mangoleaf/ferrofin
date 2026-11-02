//! A library on a case-insensitive filesystem (an SMB/CIFS share of a
//! Windows or NAS volume, a macOS volume), where a path spelled in another
//! case still resolves.
//!
//! Item ids derive from the case-sensitive path, so renaming a file or a
//! folder by case only gives the items under it new ids. Upstream's
//! `Folder.ValidateChildrenInternal2` removes the old items: they are not
//! among the valid children. The scan's prune backstop keeps a stale row
//! whose file is still on disk, and on such a filesystem the old spelling
//! still resolves, so the old row has to be recognised as the renamed file,
//! not as a file the listing missed. On a case-sensitive filesystem two
//! names that differ only in case are two files, and stay two items.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{IdDerivation, ItemTypeLookup};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_model::io::FileSystemEntryInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use rstest::rstest;

/// The real filesystem, answering existence ignoring case below `root`, as
/// a case-insensitive mount does.
struct CaseInsensitive {
    real: FerrofinFileSystem,
    root: PathBuf,
}

impl CaseInsensitive {
    /// The on-disk spelling of `path`, matching each component below `root`
    /// ignoring case.
    fn on_disk(&self, path: &str) -> Option<PathBuf> {
        let relative = Path::new(path).strip_prefix(&self.root).ok()?;
        let mut current = self.root.clone();
        for component in relative.components() {
            let wanted = component.as_os_str().to_string_lossy().to_lowercase();
            let found = std::fs::read_dir(&current)
                .ok()?
                .flatten()
                .find(|entry| entry.file_name().to_string_lossy().to_lowercase() == wanted)?;
            current = found.path();
        }
        Some(current)
    }
}

impl FileSystem for CaseInsensitive {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.real.get_file_system_entries(path)
    }

    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        self.real.try_get_file_system_entries(path)
    }

    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        self.real.get_drives()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.on_disk(path).is_some_and(|p| p.is_file())
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.on_disk(path).is_some_and(|p| p.is_dir())
    }

    /// Two spellings are one file when they resolve to the same entry.
    fn same_file(&self, a: &str, b: &str) -> bool {
        matches!((self.on_disk(a), self.on_disk(b)), (Some(a), Some(b)) if a == b)
    }

    fn validate_writable(&self, path: &str) -> Result<(), ServiceError> {
        self.real.validate_writable(path)
    }

    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        self.real.get_files(path, extensions)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        self.real.read_file(path)
    }
}

/// The real (case-sensitive) filesystem, whose listings leave out one
/// entry once `hidden` names it: a listing that came back short without an
/// error.
struct ShortListing {
    real: FerrofinFileSystem,
    hidden: Mutex<Option<PathBuf>>,
}

impl ShortListing {
    fn shown(&self, entries: Vec<FileSystemEntryInfo>) -> Vec<FileSystemEntryInfo> {
        let hidden = self.hidden.lock().expect("hidden").clone();
        entries
            .into_iter()
            .filter(|entry| hidden.as_deref() != Some(Path::new(&entry.path)))
            .collect()
    }
}

impl FileSystem for ShortListing {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.shown(self.real.get_file_system_entries(path))
    }

    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        Ok(self.shown(self.real.try_get_file_system_entries(path)?))
    }

    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        self.real.get_drives()
    }

    fn path_exists(&self, path: &str) -> bool {
        self.real.path_exists(path)
    }

    fn same_file(&self, a: &str, b: &str) -> bool {
        self.real.same_file(a, b)
    }

    fn file_exists(&self, path: &str) -> bool {
        self.real.file_exists(path)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.real.directory_exists(path)
    }

    fn validate_writable(&self, path: &str) -> Result<(), ServiceError> {
        self.real.validate_writable(path)
    }

    fn get_files(&self, path: &str, extensions: &[&str]) -> Vec<FileMetadata> {
        self.real.get_files(path, extensions)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        self.real.read_file(path)
    }
}

/// A movie library at `media`, scanned through `file_system`.
async fn library(
    tmp: &Path,
    media: &Path,
    file_system: Arc<dyn FileSystem>,
) -> (LibraryScanner, Arc<dyn ItemRepository>) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let folders: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.join("default")).with_item_store(persistence.clone()),
    );
    folders
        .add_virtual_folder(
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
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db,
        Arc::new(ItemTypeLookup::new()),
    ));
    // The server's mode for fresh and adopted databases alike: ids follow
    // the case-sensitive path (`EnableCaseSensitiveItemIds`, default true).
    let scanner = LibraryScanner::new(folders, file_system, persistence)
        .with_items(Arc::clone(&items))
        .with_id_derivation(IdDerivation::Jellyfin {
            program_data_path: None,
        });
    (scanner, items)
}

/// The stored paths of the items of `kind` below `media`, relative to it
/// and sorted.
async fn stored(items: &dyn ItemRepository, kind: BaseItemKind, media: &Path) -> Vec<String> {
    let mut paths: Vec<String> = items
        .get_items(&InternalItemsQuery {
            include_item_types: vec![kind],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("items")
        .items
        .into_iter()
        .filter_map(|item| {
            let path = PathBuf::from(item.path?);
            Some(
                path.strip_prefix(media)
                    .ok()?
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect();
    paths.sort();
    paths
}

/// A case-only rename of the file, of its title folder or of the plain
/// subfolder above it replaces the items under it: the old spellings still
/// resolve, but they are the same files the scan planned under the new ones.
#[rstest]
#[case::the_file(
    "Collection/Alien (1979)/alien (1979).mkv",
    "Collection/Alien (1979)/Alien (1979).mkv",
    "Collection",
    "Collection/Alien (1979)/Alien (1979).mkv"
)]
#[case::the_title_folder(
    "Collection/Alien (1979)",
    "Collection/alien (1979)",
    "Collection",
    "Collection/alien (1979)/alien (1979).mkv"
)]
#[case::the_plain_subfolder(
    "Collection",
    "collection",
    "collection",
    "collection/Alien (1979)/alien (1979).mkv"
)]
#[tokio::test]
async fn a_case_only_rename_replaces_the_items_instead_of_keeping_duplicates(
    #[case] from: &str,
    #[case] to: &str,
    #[case] folder: &str,
    #[case] movie: &str,
) {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media.join("Collection/Alien (1979)/alien (1979).mkv");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, b"").expect("write");
    let file_system = Arc::new(CaseInsensitive {
        real: FerrofinFileSystem::new(),
        root: media.clone(),
    });
    let (scanner, items) = library(tmp.path(), &media, file_system).await;

    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        stored(items.as_ref(), BaseItemKind::Movie, &media).await,
        ["Collection/Alien (1979)/alien (1979).mkv"]
    );
    assert_eq!(
        stored(items.as_ref(), BaseItemKind::Folder, &media).await,
        ["Collection"]
    );

    std::fs::rename(media.join(from), media.join(to)).expect("rename by case");
    let rescan = scanner.scan_all().await.expect("rescan");

    // One movie and one folder, under the new spelling: the old rows are
    // the same files, not items the listing missed.
    assert_eq!(
        stored(items.as_ref(), BaseItemKind::Movie, &media).await,
        [movie],
        "{rescan:?}"
    );
    assert_eq!(
        stored(items.as_ref(), BaseItemKind::Folder, &media).await,
        [folder],
        "{rescan:?}"
    );
}

/// On a case-sensitive filesystem, a stale row whose name differs only in
/// case from a planned one is another file: a listing that misses it does
/// not take it, as for any file still on disk.
#[tokio::test]
async fn names_differing_only_in_case_stay_two_items_on_a_case_sensitive_filesystem() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    for movie in [
        "Alien (1979)/Alien (1979).mkv",
        "alien (1979)/alien (1979).mkv",
    ] {
        let file = media.join(movie);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, movie).expect("write");
    }
    if std::fs::read_dir(&media).expect("list").count() != 2 {
        eprintln!("skipped: the temp directory is on a case-insensitive filesystem");
        return;
    }
    let file_system = Arc::new(ShortListing {
        real: FerrofinFileSystem::new(),
        hidden: Mutex::new(None),
    });
    let (scanner, items) = library(tmp.path(), &media, file_system.clone()).await;
    let both = [
        "Alien (1979)/Alien (1979).mkv",
        "alien (1979)/alien (1979).mkv",
    ];

    scanner.scan_all().await.expect("first scan");
    assert_eq!(
        stored(items.as_ref(), BaseItemKind::Movie, &media).await,
        both
    );

    *file_system.hidden.lock().expect("hidden") = Some(media.join("alien (1979)"));
    let rescan = scanner.scan_all().await.expect("rescan");

    assert_eq!(rescan.removed, 0, "{rescan:?}");
    assert_eq!(
        stored(items.as_ref(), BaseItemKind::Movie, &media).await,
        both
    );
}
