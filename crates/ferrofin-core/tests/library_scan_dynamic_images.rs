//! The library scan's dynamic-image pass, end to end: a movie library whose
//! NFO declares a genre and whose poster sits beside the file must leave the
//! materialized `Genre` row with a generated 600×600 Primary on disk and in
//! `BaseItemImageInfos` — the file `GET /Genres/{name}/Images/Primary` serves.
//!
//! Port target: `GenreImageProvider` over `BaseDynamicImageProvider`, which
//! Jellyfin runs from `GenresValidator` at the end of library validation.

use std::path::Path;
use std::sync::Arc;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_drawing::{ImageCrateEncoder, ImageProcessor};
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::{CollectionTypeOptions, ImageType};
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;

/// One movie with a Kodi sidecar naming the genre, and a real PNG poster.
fn write_movie_library(media: &Path) {
    let dir = media.join("Heat (1995)");
    std::fs::create_dir_all(&dir).expect("fixture dirs");
    std::fs::write(dir.join("Heat (1995).mkv"), b"").expect("media file");
    std::fs::write(
        dir.join("movie.nfo"),
        r#"<?xml version="1.0"?>
<movie><title>Heat</title><year>1995</year><genre>Crime</genre></movie>"#,
    )
    .expect("nfo");
    let mut poster = image::RgbImage::new(40, 60);
    for px in poster.pixels_mut() {
        *px = image::Rgb([180, 30, 30]);
    }
    poster.save(dir.join("poster.png")).expect("poster");
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_generates_the_genre_collage_from_the_library_posters() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let media = tmp.path().join("movies");
    write_movie_library(&media);
    let meta_root = tmp.path().join("metadata").join("library");

    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
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
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let processor: Arc<dyn ferrofin_traits::drawing::ImageProcessor> = Arc::new(
        ImageProcessor::new(Arc::new(ImageCrateEncoder::new()), tmp.path().join("cache")),
    );
    let scanner = LibraryScanner::new(
        vf,
        Arc::new(ferrofin_core::file_system::FerrofinFileSystem::new()),
        persistence,
    )
    .with_items(Arc::clone(&items))
    .with_image_processor(processor)
    .with_metadata_dir(meta_root.clone());

    scanner.scan_all().await.expect("scan");

    let genre = items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Genre],
            name: Some("Crime".to_owned()),
            ..InternalItemsQuery::default()
        })
        .await
        .expect("genre query")
        .into_iter()
        .next()
        .expect("the NFO genre is materialized as a Genre row");
    let genre_id = uuid::Uuid::parse_str(&genre.id).expect("genre id");
    let images = items.get_image_infos(genre_id).await.expect("images");
    let primary = images
        .iter()
        .find(|i| i.image_type == ImageType::Primary)
        .expect("the scan generated a Primary for the genre");
    assert_eq!(
        Path::new(&primary.path),
        meta_root.join(guid_to_db(genre_id)).join("primary.png"),
        "written into the genre's own art folder"
    );
    let decoded = image::open(&primary.path).expect("a decodable PNG");
    assert_eq!((decoded.width(), decoded.height()), (600, 600));
    assert_eq!((primary.width, primary.height), (600, 600));
    // The poster's colour lands in the top-left tile — proof the collage was
    // composed from the library's artwork rather than an empty canvas.
    assert_eq!(decoded.to_rgb8().get_pixel(10, 10).0, [180, 30, 30]);

    // A second scan finds the image current and leaves it untouched.
    let before = std::fs::metadata(&primary.path)
        .and_then(|m| m.modified())
        .expect("mtime");
    scanner.scan_all().await.expect("rescan");
    let after = std::fs::metadata(&primary.path)
        .and_then(|m| m.modified())
        .expect("mtime");
    assert_eq!(before, after, "a current collage is not redrawn on rescan");
    let images = items.get_image_infos(genre_id).await.expect("images");
    assert_eq!(
        images
            .iter()
            .filter(|i| i.image_type == ImageType::Primary)
            .count(),
        1
    );
}
