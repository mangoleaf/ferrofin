//! The post-scan splashscreen pass — port of `SplashscreenPostScanTask`.
//!
//! After a validation, the posters of random movies and series rated at most
//! 13, and their thumbs (their backdrops when no item has a thumb), go to the
//! image encoder's `CreateSplashscreen`. These tests record what the pass
//! hands the encoder.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::drawing::{ImageDimensions, ImageFormat, ImageOrientation};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::drawing::ImageEncoder;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::{ImageCollageOptions, ImageProcessingOptions};
use ferrofin_traits::persistence::ItemRepository;

/// An encoder that records each `create_splashscreen` call's file names.
#[derive(Default)]
struct RecordingEncoder {
    splashscreens: Mutex<Vec<(Vec<String>, Vec<String>)>>,
}

impl RecordingEncoder {
    /// The posters and backdrops of the last splashscreen, as sorted
    /// `<movie folder>/<file>` names.
    fn last(&self) -> (Vec<String>, Vec<String>) {
        self.splashscreens
            .lock()
            .expect("calls")
            .last()
            .cloned()
            .expect("a splashscreen was requested")
    }
}

/// `<movie folder>/<file>` for each path, sorted (the pass's order is random).
fn names(paths: &[String]) -> Vec<String> {
    let mut names: Vec<String> = paths
        .iter()
        .map(|path| {
            let path = Path::new(path);
            let folder = path.parent().and_then(Path::file_name).unwrap_or_default();
            let file = path.file_name().unwrap_or_default();
            format!("{}/{}", folder.to_string_lossy(), file.to_string_lossy())
        })
        .collect();
    names.sort();
    names
}

#[async_trait]
impl ImageEncoder for RecordingEncoder {
    fn supported_input_formats(&self) -> Vec<String> {
        Vec::new()
    }
    fn supported_output_formats(&self) -> Vec<ImageFormat> {
        Vec::new()
    }
    fn name(&self) -> String {
        "recording".to_owned()
    }
    fn supports_image_collage_creation(&self) -> bool {
        false
    }
    fn supports_image_encoding(&self) -> bool {
        false
    }
    async fn get_image_size(&self, _path: &str) -> Result<ImageDimensions, ServiceError> {
        Ok(ImageDimensions::default())
    }
    async fn get_image_blur_hash(&self, _: i32, _: i32, _: &str) -> Result<String, ServiceError> {
        Ok(String::new())
    }
    async fn encode_image(
        &self,
        input_path: &str,
        _: DateTime<Utc>,
        _: &str,
        _: bool,
        _: Option<ImageOrientation>,
        _: i32,
        _: &ImageProcessingOptions,
        _: ImageFormat,
    ) -> Result<String, ServiceError> {
        Ok(input_path.to_owned())
    }
    async fn create_image_collage(
        &self,
        _: &ImageCollageOptions,
        _: Option<&str>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn create_splashscreen(
        &self,
        posters: &[String],
        backdrops: &[String],
    ) -> Result<(), ServiceError> {
        self.splashscreens
            .lock()
            .expect("calls")
            .push((names(posters), names(backdrops)));
        Ok(())
    }
    async fn create_trickplay_tile(
        &self,
        _: &ImageCollageOptions,
        _: i32,
        _: i32,
        _: Option<i32>,
    ) -> Result<i32, ServiceError> {
        Ok(0)
    }
}

/// A movie folder with a media file and the given artwork files.
fn movie(media: &Path, name: &str, artwork: &[&str]) {
    let folder = media.join(name);
    std::fs::create_dir_all(&folder).expect("movie dir");
    std::fs::write(folder.join(format!("{name}.mkv")), b"").expect("media file");
    for file in artwork {
        std::fs::write(folder.join(file), b"jpeg").expect("artwork");
    }
}

/// A movies library over `movies/` and a scanner wired to `encoder`.
async fn library(root: &Path, encoder: Arc<RecordingEncoder>) -> (LibraryScanner, Database) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Movies",
        Some(CollectionTypeOptions::movies),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: root.join("movies").to_string_lossy().into_owned(),
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
        persistence,
    )
    .with_items(items)
    .with_image_encoder(encoder);
    (scanner, db)
}

/// Posters come from every item with one; the thumbs win over backdrops
/// whenever any item has a thumb.
#[tokio::test]
async fn a_scan_hands_the_posters_and_thumbs_to_the_splashscreen() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    movie(&media, "Alpha (2020)", &["poster.jpg", "fanart.jpg"]);
    movie(&media, "Bravo (2021)", &["poster.jpg", "landscape.jpg"]);
    let encoder = Arc::new(RecordingEncoder::default());
    let (scanner, _db) = library(tmp.path(), encoder.clone()).await;
    scanner.scan_all().await.expect("scan");
    assert_eq!(
        encoder.last(),
        (
            vec![
                "Alpha (2020)/poster.jpg".to_owned(),
                "Bravo (2021)/poster.jpg".to_owned()
            ],
            vec!["Bravo (2021)/landscape.jpg".to_owned()],
        )
    );
}

/// With no thumb anywhere the backdrops stand in, and an item rated above 13
/// is left out (an unrated one is kept).
#[tokio::test]
async fn without_thumbs_the_backdrops_stand_in_and_mature_items_are_left_out() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    movie(&media, "Alpha (2020)", &["poster.jpg", "fanart.jpg"]);
    movie(&media, "Mature (2022)", &["poster.jpg", "fanart.jpg"]);
    let encoder = Arc::new(RecordingEncoder::default());
    let (scanner, db) = library(tmp.path(), encoder.clone()).await;
    scanner.scan_all().await.expect("first scan");
    sqlx::query(
        r#"UPDATE "BaseItems" SET "InheritedParentalRatingValue" = 17 WHERE "Name" LIKE 'Mature%'"#,
    )
    .execute(db.writer())
    .await
    .expect("rate");
    scanner.scan_all().await.expect("second scan");
    assert_eq!(
        encoder.last(),
        (
            vec!["Alpha (2020)/poster.jpg".to_owned()],
            vec!["Alpha (2020)/fanart.jpg".to_owned()],
        )
    );
}
