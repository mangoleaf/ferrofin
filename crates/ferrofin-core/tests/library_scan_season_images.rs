//! A season's local artwork, end to end through a scan.
//!
//! `LocalImageProvider` reads a season's images from two places: its series'
//! folder, by season-prefixed names (`season01-poster.jpg`,
//! `season-specials-poster.jpg`, `season01-fanart.jpg`, …,
//! `PopulateSeasonImagesFromSeriesFolder`), and then its own folder
//! (`Season 1/poster.jpg`). `MergeImages` takes the first image of each type,
//! so the series folder's wins. A season with no folder of its own (its
//! episodes directly in the series folder) has only the first: `Supports`
//! accepts such a `LocationType.Virtual` season for that pass.
//!
//! No remote provider is wired, so every image here is a local file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};
use uuid::Uuid;

/// A TV library holding one series folder, `tv/Show`, over an in-memory
/// database.
struct Library {
    db: Database,
    items: Arc<dyn ItemRepository>,
    persistence: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    show: PathBuf,
}

impl Library {
    async fn new(root: &Path) -> Self {
        let tv = root.join("tv");
        let show = tv.join("Show");
        std::fs::create_dir_all(&show).expect("series folder");
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
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
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let scanner = LibraryScanner::new(
            vf,
            Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
            persistence.clone(),
        )
        .with_items(Arc::clone(&items));
        Self {
            db,
            items,
            persistence,
            scanner,
            show,
        }
    }

    /// Writes `relative` (below the series folder) with a few bytes, its
    /// folders included.
    fn write(&self, relative: &str) {
        let path = self.show.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(path, b"0123456789").expect("write");
    }

    /// The id of the season numbered `index`.
    async fn season(&self, index: i64) -> Uuid {
        let id: String = sqlx::query_scalar(
            r#"SELECT "Id" FROM "BaseItems" WHERE "Type" LIKE '%.Season' AND "IndexNumber" = ?1"#,
        )
        .bind(index)
        .fetch_one(self.db.pool())
        .await
        .expect("the season");
        Uuid::parse_str(&id).expect("an id")
    }

    /// The path of `season`'s first image of `image_type`, below the series
    /// folder when it is there.
    async fn first(&self, season: Uuid, image_type: ImageType) -> Option<String> {
        let images = self.items.get_image_infos(season).await.expect("images");
        images
            .into_iter()
            .find(|image| image.image_type == image_type)
            .map(|image| {
                Path::new(&image.path)
                    .strip_prefix(&self.show)
                    .map_or_else(|_| image.path.clone(), |p| p.to_string_lossy().into_owned())
            })
    }

    /// How many images of `image_type` `season` has.
    async fn count(&self, season: Uuid, image_type: ImageType) -> usize {
        let images = self.items.get_image_infos(season).await.expect("images");
        images
            .iter()
            .filter(|image| image.image_type == image_type)
            .count()
    }
}

/// The season's episode: in `Season 1/`, or directly in the series folder.
fn episode(flat: bool) -> &'static str {
    if flat {
        "Show S01E01.mkv"
    } else {
        "Season 1/Show S01E01.mkv"
    }
}

/// The four season-prefixed files in the series folder are the season's
/// Primary, Backdrop, Banner and Thumb, whether the season has a folder of
/// its own or not; the series keeps its own `poster.jpg`.
#[rstest::rstest]
#[case::folder_season(false)]
#[case::season_with_no_folder(true)]
#[tokio::test]
async fn a_season_takes_its_artwork_from_its_series_folder(#[case] flat: bool) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path()).await;
    library.write(episode(flat));
    for file in [
        "poster.jpg",
        "season01-poster.jpg",
        "season01-fanart.jpg",
        "season01-banner.jpg",
        "season01-landscape.jpg",
    ] {
        library.write(file);
    }
    library.scanner.scan_all().await.expect("scan");
    let season = library.season(1).await;
    assert_eq!(
        library.first(season, ImageType::Primary).await.as_deref(),
        Some("season01-poster.jpg")
    );
    assert_eq!(library.count(season, ImageType::Primary).await, 1);
    assert_eq!(
        library.first(season, ImageType::Backdrop).await.as_deref(),
        Some("season01-fanart.jpg")
    );
    assert_eq!(
        library.first(season, ImageType::Banner).await.as_deref(),
        Some("season01-banner.jpg")
    );
    assert_eq!(
        library.first(season, ImageType::Thumb).await.as_deref(),
        Some("season01-landscape.jpg")
    );
}

/// A season's own `poster.jpg` is its Primary when the series folder has
/// none for it, and gives way to the series folder's when it has one
/// (`PopulateImages` adds the series folder's first): `MergeImages` sets
/// that one as the season's only Primary.
#[tokio::test]
async fn a_seasons_own_poster_stands_until_its_series_folder_has_one() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path()).await;
    library.write(episode(false));
    library.write("Season 1/poster.jpg");
    library.scanner.scan_all().await.expect("scan");
    let season = library.season(1).await;
    assert_eq!(
        library.first(season, ImageType::Primary).await.as_deref(),
        Some("Season 1/poster.jpg")
    );

    library.write("season01-poster.jpg");
    library.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        library.first(season, ImageType::Primary).await.as_deref(),
        Some("season01-poster.jpg")
    );
    assert_eq!(library.count(season, ImageType::Primary).await, 1);
}

/// Season 0 is matched by its name (`Specials`) and by the `-specials`
/// marker: `season-specials-poster.jpg`.
#[rstest::rstest]
#[case::folder_season(false)]
#[case::season_with_no_folder(true)]
#[tokio::test]
async fn season_zero_takes_the_season_specials_artwork(#[case] flat: bool) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path()).await;
    library.write(if flat {
        "Show S00E01.mkv"
    } else {
        "Season 0/Show S00E01.mkv"
    });
    library.write("season-specials-poster.jpg");
    library.write("season01-poster.jpg");
    library.scanner.scan_all().await.expect("scan");
    let season = library.season(0).await;
    assert_eq!(
        library.first(season, ImageType::Primary).await.as_deref(),
        Some("season-specials-poster.jpg")
    );
}

/// A season that already holds a Primary from elsewhere (a downloaded one in
/// the metadata folder) takes the series folder's poster on the next plain
/// scan: `MergeImages` replaces a stored image whose path differs from the
/// first local one found.
#[rstest::rstest]
#[case::folder_season(false)]
#[case::season_with_no_folder(true)]
#[tokio::test]
async fn a_rescan_replaces_a_stored_primary_with_the_series_folder_poster(#[case] flat: bool) {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path()).await;
    library.write(episode(flat));
    library.scanner.scan_all().await.expect("scan");
    let season = library.season(1).await;
    assert_eq!(library.first(season, ImageType::Primary).await, None);

    let downloaded = tmp.path().join("metadata").join("primary.jpg");
    std::fs::create_dir_all(downloaded.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&downloaded, b"jpeg").expect("downloaded");
    library
        .persistence
        .save_item_images(
            season,
            &[ItemImageInfo {
                path: downloaded.to_string_lossy().into_owned(),
                image_type: ImageType::Primary,
                date_modified: Utc::now(),
                width: 0,
                height: 0,
                blur_hash: None,
            }],
        )
        .await
        .expect("store a Primary");
    library.scanner.scan_all().await.expect("quiet rescan");
    assert_eq!(
        library.first(season, ImageType::Primary).await,
        Some(downloaded.to_string_lossy().into_owned()),
        "kept while no local poster exists"
    );

    library.write("season01-poster.jpg");
    library.scanner.scan_all().await.expect("rescan");
    assert_eq!(
        library.first(season, ImageType::Primary).await.as_deref(),
        Some("season01-poster.jpg")
    );
}

/// A season refreshed on its own, as the library monitor refreshes the
/// nearest item to a changed path, carries its series for context only:
/// its series folder comes from the stored series, and its series folder's
/// poster still comes before the one just added to its own folder.
#[tokio::test]
async fn a_season_refreshed_on_its_own_reads_its_stored_series_folder() {
    let tmp = tempfile::tempdir().expect("tmp");
    let library = Library::new(tmp.path()).await;
    library.write(episode(false));
    library.scanner.scan_all().await.expect("scan");
    let season = library.season(1).await;
    assert_eq!(library.first(season, ImageType::Primary).await, None);

    library.write("season01-poster.jpg");
    library.write("Season 1/poster.jpg");
    let changed = library.show.join("Season 1").join("poster.jpg");
    library
        .scanner
        .scan_paths(&[changed.to_string_lossy().into_owned()])
        .await
        .expect("path scan");
    assert_eq!(
        library.first(season, ImageType::Primary).await.as_deref(),
        Some("season01-poster.jpg")
    );
    assert_eq!(library.count(season, ImageType::Primary).await, 1);
}
