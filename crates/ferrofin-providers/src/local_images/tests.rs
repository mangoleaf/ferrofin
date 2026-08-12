//! Unit tests for the local-image providers, run over an in-memory
//! [`DirectoryService`] fake and a temp dir.
//!
//! There is no upstream xUnit suite for `MediaBrowser.LocalMetadata.Images`, so
//! these tests encode the C# filename-convention rules directly (the C# source
//! is the oracle).

use std::collections::BTreeMap;

use super::*;
use crate::container_types::{FileSystemMetadata, LocalImageInfo};
use ferrofin_model::entities::ImageType;

/// An in-memory [`DirectoryService`]: maps a directory path to its entries.
#[derive(Debug, Default)]
struct FakeDirectoryService {
    dirs: BTreeMap<String, Vec<FileSystemMetadata>>,
}

impl FakeDirectoryService {
    fn new() -> Self {
        Self::default()
    }

    /// Registers a non-empty file (1 byte) under `dir`.
    fn file(&mut self, dir: &str, name: &str) -> &mut Self {
        self.entry(dir, name, false, 1)
    }

    /// Registers an empty (zero-length) file under `dir`.
    fn empty_file(&mut self, dir: &str, name: &str) -> &mut Self {
        self.entry(dir, name, false, 0)
    }

    /// Registers a subdirectory `name` under `dir` (and creates its own bucket).
    fn dir(&mut self, dir: &str, name: &str) -> &mut Self {
        self.entry(dir, name, true, 0);
        let full = format!("{dir}/{name}");
        self.dirs.entry(full).or_default();
        self
    }

    fn entry(&mut self, dir: &str, name: &str, is_directory: bool, length: i64) -> &mut Self {
        let full_name = format!("{dir}/{name}");
        let extension = if is_directory {
            None
        } else {
            name.rfind('.').map(|idx| name[idx..].to_owned())
        };
        let meta = FileSystemMetadata {
            exists: true,
            full_name,
            name: name.to_owned(),
            extension,
            length,
            last_write_time_utc: None,
            creation_time_utc: None,
            is_directory,
        };
        self.dirs.entry(dir.to_owned()).or_default().push(meta);
        self
    }
}

impl DirectoryService for FakeDirectoryService {
    fn file_system_entries(&self, path: &str) -> Vec<FileSystemMetadata> {
        self.dirs.get(path).cloned().unwrap_or_default()
    }
}

/// Collects the discovered images as `(name, type)` pairs for easy assertions.
fn pairs(images: &[LocalImageInfo]) -> Vec<(String, ImageType)> {
    images
        .iter()
        .map(|i| (i.file_info.name.clone(), i.type_))
        .collect()
}

/// A generic movie-ish item sitting in its own folder at `/media/movie`.
fn folder_item(kind: ImageItemKind) -> ImageItem {
    ImageItem {
        kind,
        name: "The Movie".to_owned(),
        path: Some("/media/movie/The Movie.mkv".to_owned()),
        containing_folder_path: Some("/media/movie".to_owned()),
        file_name_without_extension: Some("The Movie".to_owned()),
        is_in_mixed_folder: false,
        index_number: None,
        physical_locations: Vec::new(),
    }
}

#[test]
fn supported_image_extension_ordering_prefers_png() {
    assert_eq!(supported_image_extension_index(".png"), 0);
    assert_eq!(supported_image_extension_index(".JPG"), 1);
    assert_eq!(supported_image_extension_index(".svg"), 6);
    assert_eq!(supported_image_extension_index(".mkv"), usize::MAX);
    assert!(is_supported_image_extension(".JPEG"));
    assert!(!is_supported_image_extension(".mkv"));
}

#[test]
fn local_provider_supports_excludes_episode_song_photo() {
    assert!(LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::Movie
    )));
    assert!(LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::AudioBook
    )));
    assert!(!LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::Episode
    )));
    assert!(!LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::Audio
    )));
    assert!(!LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::Photo
    )));
}

#[test]
fn primary_prefers_item_named_file_over_convention() {
    // "The Movie.png" (item's own name) wins over "poster.png".
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "The Movie.png")
        .file("/media/movie", "poster.jpg");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let primaries: Vec<_> = images
        .iter()
        .filter(|i| i.type_ == ImageType::Primary)
        .collect();
    assert_eq!(primaries.len(), 1);
    assert_eq!(primaries[0].file_info.name, "The Movie.png");
}

#[test]
fn primary_falls_back_to_convention_table() {
    // No item-named file; "poster" is first in the video table.
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "folder.jpg")
        .file("/media/movie", "poster.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let primary = images
        .iter()
        .find(|i| i.type_ == ImageType::Primary)
        .expect("primary found");
    assert_eq!(primary.file_info.name, "poster.png");
}

#[test]
fn empty_files_are_ignored() {
    let mut fs = FakeDirectoryService::new();
    fs.empty_file("/media/movie", "poster.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    assert!(images.is_empty());
}

#[test]
fn logo_art_banner_thumb_discovered_for_movie() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png")
        .file("/media/movie", "logo.png")
        .file("/media/movie", "clearart.png")
        .file("/media/movie", "banner.png")
        .file("/media/movie", "landscape.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let types: Vec<ImageType> = images.iter().map(|i| i.type_).collect();
    assert!(types.contains(&ImageType::Logo));
    assert!(types.contains(&ImageType::Art));
    assert!(types.contains(&ImageType::Banner));
    assert!(types.contains(&ImageType::Thumb));
}

#[test]
fn clearlogo_used_when_logo_absent() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png")
        .file("/media/movie", "clearlogo.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let logo = images
        .iter()
        .find(|i| i.type_ == ImageType::Logo)
        .expect("logo from clearlogo");
    assert_eq!(logo.file_info.name, "clearlogo.png");
}

#[test]
fn music_album_prefers_cdart_for_disc() {
    let mut item = folder_item(ImageItemKind::MusicAlbum);
    item.file_name_without_extension = None;
    item.path = Some("/media/album".to_owned());
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "folder.png")
        .file("/media/movie", "cdart.png")
        .file("/media/movie", "disc.png");
    let images = LocalImageProvider::get_images(&item, &fs);
    let disc = images
        .iter()
        .find(|i| i.type_ == ImageType::Disc)
        .expect("disc image");
    assert_eq!(disc.file_info.name, "cdart.png");
}

#[test]
fn video_prefers_disc_over_cdart() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png")
        .file("/media/movie", "cdart.png")
        .file("/media/movie", "disc.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let disc = images
        .iter()
        .find(|i| i.type_ == ImageType::Disc)
        .expect("disc image");
    assert_eq!(disc.file_info.name, "disc.png");
}

#[test]
fn backdrops_from_fanart_and_numbered_series() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png")
        .file("/media/movie", "fanart.png")
        .file("/media/movie", "fanart-1.png")
        .file("/media/movie", "fanart-2.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let backdrops: Vec<_> = images
        .iter()
        .filter(|i| i.type_ == ImageType::Backdrop)
        .map(|i| i.file_info.name.clone())
        .collect();
    assert!(backdrops.contains(&"fanart.png".to_owned()));
    assert!(backdrops.contains(&"fanart-1.png".to_owned()));
    assert!(backdrops.contains(&"fanart-2.png".to_owned()));
}

#[test]
fn numbered_backdrops_stop_after_three_gaps() {
    // fanart-1 present, then 2/3/4 missing -> loop bails; fanart-5 never seen.
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png")
        .file("/media/movie", "fanart-1.png")
        .file("/media/movie", "fanart-5.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let backdrops: Vec<_> = images
        .iter()
        .filter(|i| i.type_ == ImageType::Backdrop)
        .map(|i| i.file_info.name.clone())
        .collect();
    assert!(backdrops.contains(&"fanart-1.png".to_owned()));
    assert!(!backdrops.contains(&"fanart-5.png".to_owned()));
}

#[test]
fn extrafanart_folder_backdrops_collected() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png")
        .dir("/media/movie", "extrafanart")
        .file("/media/movie/extrafanart", "a.png")
        .file("/media/movie/extrafanart", "b.jpg")
        .empty_file("/media/movie/extrafanart", "empty.png");
    let images = LocalImageProvider::get_images(&folder_item(ImageItemKind::Movie), &fs);
    let backdrops: Vec<_> = images
        .iter()
        .filter(|i| i.type_ == ImageType::Backdrop)
        .map(|i| i.file_info.name.clone())
        .collect();
    assert!(backdrops.contains(&"a.png".to_owned()));
    assert!(backdrops.contains(&"b.jpg".to_owned()));
    assert!(!backdrops.contains(&"empty.png".to_owned()));
}

#[test]
fn mixed_folder_requires_prefix() {
    // In a mixed folder, a bare "poster.png" must NOT match; only the prefixed
    // "The Movie-poster.png" would.
    let mut item = folder_item(ImageItemKind::Movie);
    item.is_in_mixed_folder = true;
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/movie", "poster.png");
    let images = LocalImageProvider::get_images(&item, &fs);
    assert!(images.iter().all(|i| i.file_info.name != "poster.png"));

    // With the prefix it is found.
    let mut fs2 = FakeDirectoryService::new();
    fs2.file("/media/movie", "The Movie-poster.png");
    let images2 = LocalImageProvider::get_images(&item, &fs2);
    assert_eq!(
        pairs(&images2)
            .into_iter()
            .filter(|(_, t)| *t == ImageType::Primary)
            .count(),
        1
    );
}

#[test]
fn season_pulls_from_series_folder() {
    // Season 1 with name "Season 1" -> prefix "season1"; season marker "01" ->
    // "season01". Both prefixes are tried. Place a "season01-poster.png".
    let season = ImageItem {
        kind: ImageItemKind::Season,
        name: "Season 1".to_owned(),
        path: None,
        containing_folder_path: Some("/media/series".to_owned()),
        file_name_without_extension: None,
        is_in_mixed_folder: false,
        index_number: Some(1),
        physical_locations: Vec::new(),
    };
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/series", "season01-poster.png")
        .file("/media/series", "season01-fanart.png");
    let images = LocalImageProvider::get_images(&season, &fs);
    let names: Vec<_> = images.iter().map(|i| i.file_info.name.clone()).collect();
    assert!(names.contains(&"season01-poster.png".to_owned()));
    assert!(names.contains(&"season01-fanart.png".to_owned()));
    let poster = images
        .iter()
        .find(|i| i.file_info.name == "season01-poster.png")
        .unwrap();
    assert_eq!(poster.type_, ImageType::Primary);
    let fanart = images
        .iter()
        .find(|i| i.file_info.name == "season01-fanart.png")
        .unwrap();
    assert_eq!(fanart.type_, ImageType::Backdrop);
}

#[test]
fn season_zero_uses_specials_marker() {
    let season = ImageItem {
        kind: ImageItemKind::Season,
        name: "Specials".to_owned(),
        path: None,
        containing_folder_path: Some("/media/series".to_owned()),
        file_name_without_extension: None,
        is_in_mixed_folder: false,
        index_number: Some(0),
        physical_locations: Vec::new(),
    };
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/series", "season-specials-poster.png");
    let images = LocalImageProvider::get_images(&season, &fs);
    assert!(
        images
            .iter()
            .any(|i| i.file_info.name == "season-specials-poster.png")
    );
}

#[test]
fn episode_provider_matches_name_and_thumb() {
    let episode = ImageItem {
        kind: ImageItemKind::Episode,
        name: "S01E01".to_owned(),
        path: Some("/media/show/S01E01.mkv".to_owned()),
        containing_folder_path: Some("/media/show".to_owned()),
        file_name_without_extension: Some("S01E01".to_owned()),
        is_in_mixed_folder: true,
        index_number: None,
        physical_locations: Vec::new(),
    };
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/show", "S01E01.png")
        .file("/media/show", "S01E01-thumb.jpg")
        .file("/media/show", "S01E02.png");
    let images = EpisodeLocalImageProvider::get_images(&episode, &fs);
    let names: Vec<_> = images.iter().map(|i| i.file_info.name.clone()).collect();
    assert!(names.contains(&"S01E01.png".to_owned()));
    assert!(names.contains(&"S01E01-thumb.jpg".to_owned()));
    assert!(!names.contains(&"S01E02.png".to_owned()));
    assert!(images.iter().all(|i| i.type_ == ImageType::Primary));
}

#[test]
fn episode_provider_scans_metadata_subfolder() {
    let episode = ImageItem {
        kind: ImageItemKind::Episode,
        name: "S01E01".to_owned(),
        path: Some("/media/show/S01E01.mkv".to_owned()),
        containing_folder_path: Some("/media/show".to_owned()),
        file_name_without_extension: Some("S01E01".to_owned()),
        is_in_mixed_folder: true,
        index_number: None,
        physical_locations: Vec::new(),
    };
    let mut fs = FakeDirectoryService::new();
    fs.dir("/media/show", "metadata")
        .file("/media/show/metadata", "S01E01.png");
    let images = EpisodeLocalImageProvider::get_images(&episode, &fs);
    assert!(images.iter().any(|i| i.file_info.name == "S01E01.png"));
}

#[test]
fn collection_folder_scans_physical_locations() {
    let mut item = ImageItem::new(ImageItemKind::CollectionFolder);
    item.physical_locations = vec!["/media/lib-a".to_owned(), "/media/lib-b".to_owned()];
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/lib-a", "poster.png")
        .file("/media/lib-b", "banner.png");
    let images = CollectionFolderLocalImageProvider::get_images(&item, &fs);
    let types: Vec<ImageType> = images.iter().map(|i| i.type_).collect();
    assert!(types.contains(&ImageType::Primary));
    assert!(types.contains(&ImageType::Banner));
    assert!(CollectionFolderLocalImageProvider::supports(&item));
}

#[test]
fn internal_metadata_provider_scans_given_path() {
    let item = ImageItem::new(ImageItemKind::Movie);
    let mut fs = FakeDirectoryService::new();
    fs.file("/internal/meta", "poster.png");
    let images = InternalMetadataFolderImageProvider::get_images(&item, "/internal/meta", &fs);
    assert!(images.iter().any(|i| i.type_ == ImageType::Primary));

    // Empty path -> no images.
    let none = InternalMetadataFolderImageProvider::get_images(&item, "", &fs);
    assert!(none.is_empty());

    assert!(!InternalMetadataFolderImageProvider::supports(
        &ImageItem::new(ImageItemKind::Photo)
    ));
    assert!(InternalMetadataFolderImageProvider::supports(
        &ImageItem::new(ImageItemKind::Audio)
    ));
}

#[test]
fn fs_directory_service_reads_real_temp_dir() {
    // Exercise the real FsDirectoryService against a temp dir.
    let base = std::env::temp_dir().join(format!("ferrofin-li-{}", uuid::Uuid::new_v4()));
    let movie_dir = base.join("movie");
    std::fs::create_dir_all(&movie_dir).expect("create temp dir");
    std::fs::write(movie_dir.join("poster.png"), b"x").expect("write poster");

    let item = ImageItem {
        kind: ImageItemKind::Movie,
        name: "M".to_owned(),
        path: Some(movie_dir.join("M.mkv").to_string_lossy().into_owned()),
        containing_folder_path: Some(movie_dir.to_string_lossy().into_owned()),
        file_name_without_extension: Some("M".to_owned()),
        is_in_mixed_folder: false,
        index_number: None,
        physical_locations: Vec::new(),
    };
    let images = LocalImageProvider::get_images(&item, &FsDirectoryService::new());
    assert!(images.iter().any(|i| i.file_info.name == "poster.png"));

    std::fs::remove_dir_all(&base).ok();
}
