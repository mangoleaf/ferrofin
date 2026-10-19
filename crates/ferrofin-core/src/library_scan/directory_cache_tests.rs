//! How a scan keeps its run's directory listings ([`DirectoryCache`])
//! current where it writes: the artwork it saves into a media folder, and
//! the item refreshes it serves between its items.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_model::io::FileSystemEntryInfo;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::filesystem::{FileMetadata, FileSystem};
use ferrofin_traits::library::{ScanTarget, VirtualFolderManager};
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::ItemRepository;

use super::{
    DirectoryCache, DirectoryCacheLimits, LaneRefresh, LibraryScanner, PriorityLane, ScanCancel,
    ScanOutcome, ScanPasses, ScanRun,
};
use crate::file_system::FerrofinFileSystem;
use crate::item_persistence_service::FerrofinItemPersistenceService;
use crate::item_repository::FerrofinItemRepository;
use crate::item_type_lookup::{ItemTypeLookup, stored_type_name};
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;

/// The real filesystem, counting the listings of one directory.
struct Counting {
    dir: String,
    listings: AtomicUsize,
}

impl FileSystem for Counting {
    fn get_file_system_entries(&self, path: &str) -> Vec<FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<FileSystemEntryInfo>, ServiceError> {
        if path == self.dir {
            self.listings.fetch_add(1, Ordering::SeqCst);
        }
        FerrofinFileSystem::new().try_get_file_system_entries(path)
    }
    fn get_drives(&self) -> Vec<FileSystemEntryInfo> {
        Vec::new()
    }
    fn file_exists(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }
    fn directory_exists(&self, path: &str) -> bool {
        Path::new(path).is_dir()
    }
    fn validate_writable(&self, _path: &str) -> Result<(), ServiceError> {
        Ok(())
    }
    fn get_files(&self, _path: &str, _extensions: &[&str]) -> Vec<FileMetadata> {
        Vec::new()
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ServiceError> {
        FerrofinFileSystem::new().read_file(path)
    }
}

/// A lane holding one refresh, of a path no library holds.
#[derive(Default)]
struct OneRefresh {
    taken: AtomicBool,
}

impl PriorityLane for OneRefresh {
    fn next(&self) -> Option<LaneRefresh> {
        (!self.taken.swap(true, Ordering::SeqCst)).then(|| LaneRefresh {
            target: ScanTarget::Paths(vec!["/nowhere/at/all".to_owned()]),
            options: ferrofin_traits::providers::MetadataRefreshOptions::default(),
            ancestors: ferrofin_traits::providers::MetadataRefreshOptions::default(),
            passes: ScanPasses::Touched,
            cancel: ScanCancel::new(),
            key: 0,
        })
    }
    fn done(&self, _refresh: LaneRefresh, _outcome: &Result<ScanOutcome, ServiceError>) {}
}

#[tokio::test]
async fn a_refresh_the_run_serves_clears_its_listings() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_string_lossy().into_owned();
    let fs = Arc::new(Counting {
        dir: dir.clone(),
        listings: AtomicUsize::new(0),
    });
    let db = crate::test_support::test_db().await;
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db));
    let vf: Arc<dyn VirtualFolderManager> =
        Arc::new(FerrofinVirtualFolderManager::new(tmp.path().join("views")));
    let scanner = LibraryScanner::new(vf, fs.clone(), persistence);
    let dirs = Arc::new(DirectoryCache::new(
        fs.clone(),
        DirectoryCacheLimits::default(),
    ));
    dirs.get_file_system_entries(&dir);
    dirs.get_file_system_entries(&dir);
    assert_eq!(fs.listings.load(Ordering::SeqCst), 1);

    let lane = OneRefresh::default();
    let run = ScanRun {
        dirs: Some(&dirs),
        ..ScanRun::defaults().with_lane(&lane)
    };
    Box::pin(scanner.serve_lane(run)).await;

    assert!(lane.taken.load(Ordering::SeqCst), "the refresh was served");
    dirs.get_file_system_entries(&dir);
    assert_eq!(fs.listings.load(Ordering::SeqCst), 2, "listed again");
}

#[tokio::test]
async fn artwork_saved_into_a_media_folder_lands_in_the_runs_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("media");
    let folder = media.join("Heat (1995)");
    let video = folder.join("Heat (1995).mkv");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(&video, b"video").unwrap();

    let db = crate::test_support::test_db().await;
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db,
        Arc::new(ItemTypeLookup::new()),
    ));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Movies",
        Some(CollectionTypeOptions::movies),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: media.to_string_lossy().into_owned(),
            }],
            save_local_metadata: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let library = vf.get_virtual_folders().await.unwrap()[0]
        .item_id
        .clone()
        .unwrap();
    let paths = Arc::new(crate::FerrofinServerApplicationPaths::new(
        tmp.path(),
        tmp.path().join("log"),
        tmp.path().join("server-config"),
        tmp.path().join("cache"),
        tmp.path().join("web"),
    ));
    let configuration = Arc::new(
        crate::FerrofinServerConfigurationManager::load(paths)
            .await
            .unwrap(),
    );
    let meta = tmp.path().join("metadata");
    let saver = Arc::new(ferrofin_providers::image_save::ImageFileSaver::new(
        items.clone(),
        vf.clone(),
        configuration,
        meta.clone().into(),
    ));
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(items)
        .with_metadata_dir(meta.clone())
        .with_image_saver(saver);

    let id = uuid::Uuid::new_v4();
    let row = BaseItemEntity {
        id: ferrofin_db::store::guid_to_db(id),
        type_: stored_type_name(BaseItemKind::Movie).unwrap().to_owned(),
        name: Some("Heat".into()),
        path: Some(video.to_string_lossy().into_owned()),
        top_parent_id: Some(library),
        ..Default::default()
    };
    // A downloaded poster, in the item's metadata folder.
    let downloaded = meta.join(id.to_string()).join("poster.jpg");
    std::fs::create_dir_all(downloaded.parent().unwrap()).unwrap();
    std::fs::write(&downloaded, b"poster").unwrap();
    let mut images = vec![ItemImageInfo {
        path: downloaded.to_string_lossy().into_owned(),
        image_type: ImageType::Primary,
        date_modified: super::file_date_modified(&downloaded),
        width: 0,
        height: 0,
        blur_hash: None,
    }];

    let dirs = DirectoryCache::new(
        Arc::new(FerrofinFileSystem::new()),
        DirectoryCacheLimits::default(),
    );
    let folder_path = folder.to_string_lossy().into_owned();
    assert_eq!(dirs.get_file_system_entries(&folder_path).len(), 1);

    let failures = scanner
        .publish_media_artwork(&row, &mut images, &[], Some(&dirs))
        .await;

    assert_eq!(failures, 0);
    assert!(
        Path::new(&images[0].path).starts_with(&folder),
        "saved beside the movie: {}",
        images[0].path
    );
    let listed: Vec<String> = dirs
        .get_file_system_entries(&folder_path)
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    assert!(listed.contains(&images[0].path), "{listed:?}");
}
