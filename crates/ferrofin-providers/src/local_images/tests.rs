//! Unit tests for the local-image providers, run over an in-memory
//! [`DirectoryService`] fake and a temp dir.
//!
//! There is no upstream xUnit suite for `MediaBrowser.LocalMetadata.Images`, so
//! these tests encode the C# filename-convention rules directly (the C# source
//! is the oracle).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;
use crate::container_types::{FileSystemMetadata, LocalImageInfo};
use ferrofin_model::entities::ImageType;
use ferrofin_traits::options::ItemImageInfo;

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
    fn file_system_entries(&self, path: &str) -> Arc<[FileSystemMetadata]> {
        self.dirs
            .get(path)
            .map_or_else(|| Arc::from([]), |entries| Arc::from(entries.as_slice()))
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
        series_folder_path: None,
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
    let on_disk = |kind| ImageItem {
        path: Some("/media/item".to_owned()),
        ..ImageItem::new(kind)
    };
    assert!(LocalImageProvider::supports(&on_disk(ImageItemKind::Movie)));
    assert!(LocalImageProvider::supports(&on_disk(
        ImageItemKind::AudioBook
    )));
    assert!(LocalImageProvider::supports(&on_disk(
        ImageItemKind::Season
    )));
    assert!(!LocalImageProvider::supports(&on_disk(
        ImageItemKind::Episode
    )));
    assert!(!LocalImageProvider::supports(&on_disk(
        ImageItemKind::Audio
    )));
    assert!(!LocalImageProvider::supports(&on_disk(
        ImageItemKind::Photo
    )));
}

/// `Supports`: an item with no path does not support local metadata, and of
/// those (`LocationType.Virtual`) only a season whose series is on the file
/// system is supported.
#[test]
fn local_provider_supports_a_path_less_season_of_a_series_on_disk_only() {
    let series_folder = Some("/media/series".to_owned());
    assert!(LocalImageProvider::supports(&ImageItem {
        series_folder_path: series_folder.clone(),
        ..ImageItem::new(ImageItemKind::Season)
    }));
    assert!(!LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::Season
    )));
    assert!(!LocalImageProvider::supports(&ImageItem {
        series_folder_path: series_folder,
        ..ImageItem::new(ImageItemKind::Movie)
    }));
    assert!(!LocalImageProvider::supports(&ImageItem::new(
        ImageItemKind::Series
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
    assert!(images.is_empty(), "{images:?}");
}

// An item with no containing folder (`GetFiles`' non-file-protocol exit) finds
// nothing, even when a folder of artwork sits where it would have looked.
#[test]
fn item_without_containing_folder_finds_no_images() {
    let mut dir = FakeDirectoryService::new();
    dir.file("/media/movie", "poster.jpg");
    let mut item = folder_item(ImageItemKind::Movie);
    item.containing_folder_path = None;
    let images = LocalImageProvider::get_images(&item, &dir);
    assert!(images.is_empty(), "{images:?}");
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
        containing_folder_path: None,
        file_name_without_extension: None,
        is_in_mixed_folder: false,
        index_number: Some(1),
        series_folder_path: Some("/media/series".to_owned()),
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
        containing_folder_path: None,
        file_name_without_extension: None,
        is_in_mixed_folder: false,
        index_number: Some(0),
        series_folder_path: Some("/media/series".to_owned()),
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

/// A season in its own folder, as the scan sees it: its folder is
/// `/media/series/Season 1`, its series' folder `/media/series`.
fn folder_season(series_folder: Option<&str>) -> ImageItem {
    ImageItem {
        kind: ImageItemKind::Season,
        name: "Season 1".to_owned(),
        path: Some("/media/series/Season 1".to_owned()),
        containing_folder_path: Some("/media/series/Season 1".to_owned()),
        file_name_without_extension: Some("Season 1".to_owned()),
        index_number: Some(1),
        series_folder_path: series_folder.map(str::to_owned),
        ..ImageItem::new(ImageItemKind::Season)
    }
}

/// `PopulateImages` adds a season's series-folder images before its own
/// folder's, and `MergeImages` takes the first of each type: the series
/// folder's `season01-*` files are the season's Primary, Banner and Thumb
/// even with a `poster`/`banner` in its own folder, and its own folder's
/// images still follow them.
#[test]
fn a_folder_season_lists_its_series_folder_images_before_its_own() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/series", "season01-poster.jpg")
        .file("/media/series", "season01-banner.jpg")
        .file("/media/series", "season01-landscape.jpg")
        .file("/media/series", "season02-poster.jpg")
        .file("/media/series", "poster.jpg")
        .file("/media/series/Season 1", "poster.jpg")
        .file("/media/series/Season 1", "banner.jpg");
    let images = LocalImageProvider::get_images(&folder_season(Some("/media/series")), &fs);
    let first = |type_| {
        images
            .iter()
            .find(|i| i.type_ == type_)
            .map(|i| i.file_info.full_name.as_str())
    };
    assert_eq!(
        first(ImageType::Primary),
        Some("/media/series/season01-poster.jpg")
    );
    assert_eq!(
        first(ImageType::Banner),
        Some("/media/series/season01-banner.jpg")
    );
    assert_eq!(
        first(ImageType::Thumb),
        Some("/media/series/season01-landscape.jpg")
    );
    let names: Vec<_> = images
        .iter()
        .map(|i| i.file_info.full_name.as_str())
        .collect();
    assert!(
        names.contains(&"/media/series/Season 1/poster.jpg"),
        "{names:?}"
    );
    assert!(
        !names.contains(&"/media/series/season02-poster.jpg"),
        "another season's poster: {names:?}"
    );
    assert!(
        !names.contains(&"/media/series/poster.jpg"),
        "the series' own poster: {names:?}"
    );
}

/// A season whose series is not on the file system reads its own folder
/// only.
#[test]
fn a_season_with_no_series_folder_reads_its_own_folder_only() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/series", "season01-poster.jpg")
        .file("/media/series/Season 1", "poster.jpg");
    let images = LocalImageProvider::get_images(&folder_season(None), &fs);
    assert_eq!(
        pairs(&images),
        [("poster.jpg".to_owned(), ImageType::Primary)]
    );
}

/// The season's name is tried before its number: `Season 1` as `season1-…`,
/// then `season01-…`. A season named for its marker is tried once.
#[test]
fn a_season_is_matched_by_its_name_before_its_number() {
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/series", "season01-poster.jpg")
        .file("/media/series", "season1-poster.jpg");
    let images = LocalImageProvider::get_images(&folder_season(Some("/media/series")), &fs);
    assert_eq!(
        pairs(&images),
        [
            ("season1-poster.jpg".to_owned(), ImageType::Primary),
            ("season01-poster.jpg".to_owned(), ImageType::Primary),
        ]
    );
    let named_for_marker = ImageItem {
        name: "01".to_owned(),
        ..folder_season(Some("/media/series"))
    };
    let mut fs = FakeDirectoryService::new();
    fs.file("/media/series", "01-poster.jpg")
        .file("/media/series", "season01-poster.jpg");
    let images = LocalImageProvider::get_images(&named_for_marker, &fs);
    assert_eq!(
        pairs(&images),
        [("01-poster.jpg".to_owned(), ImageType::Primary)]
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
        series_folder_path: None,
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
        series_folder_path: None,
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

/// `LocalImageProvider.GetImages(item, paths, …)` lists each path with
/// `GetFiles(path, SupportedImageExtensions, true, false)`: not recursively,
/// so a title's poster in a subfolder of a library's location is not the
/// library's.
#[test]
fn collection_folder_reads_its_locations_without_their_subfolders() {
    let mut item = ImageItem::new(ImageItemKind::CollectionFolder);
    item.physical_locations = vec!["/media/movies".to_owned()];
    let mut fs = FakeDirectoryService::new();
    fs.dir("/media/movies", "Heat (1995)")
        .file("/media/movies/Heat (1995)", "poster.png");
    let images = CollectionFolderLocalImageProvider::get_images(&item, &fs);
    assert!(images.is_empty(), "{images:?}");
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
    assert!(none.is_empty(), "{none:?}");

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
        series_folder_path: None,
        physical_locations: Vec::new(),
    };
    let images = LocalImageProvider::get_images(&item, &FsDirectoryService::new());
    assert!(images.iter().any(|i| i.file_info.name == "poster.png"));

    std::fs::remove_dir_all(&base).ok();
}

// `FsDirectoryService` memoizes each listing for the lifetime of the instance,
// exactly like the upstream `DirectoryService._cache`. Without the memo every
// lookup of a folder pays its own full `readdir` sweep, which across the items
// of a folder sharing one instance is O(items × entries) syscalls.
//
// The memo is proved by deleting the directory between the two calls: a cached
// service still answers from the first listing, an uncached one returns empty.
#[test]
fn fs_directory_service_memoizes_each_listing() {
    let base = std::env::temp_dir().join(format!("ferrofin-dsc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&base).expect("create temp dir");
    std::fs::write(base.join("a.png"), b"x").expect("write a");
    std::fs::create_dir_all(base.join("metadata")).expect("create subdir");
    let path = base.to_string_lossy().into_owned();

    let service = FsDirectoryService::new();
    let first = service.file_system_entries(&path);
    assert_eq!(first.len(), 2, "temp dir holds a.png + metadata/");

    // Both derived views must be served from the one memoized listing.
    assert_eq!(service.files(&path).len(), 1);
    assert_eq!(service.directories(&path).len(), 1);

    std::fs::remove_dir_all(&base).ok();

    let cached = service.file_system_entries(&path);
    assert_eq!(cached, first, "second listing must come from the memo");
    assert_eq!(service.files(&path).len(), 1);
    assert_eq!(service.directories(&path).len(), 1);

    // A fresh service re-reads: the memo is instance-scoped, never global.
    assert!(
        FsDirectoryService::new()
            .file_system_entries(&path)
            .is_empty(),
        "the memo must not outlive the instance"
    );
}

// The memo hands out the listing it holds, not a copy: upstream's
// `GetFileSystemEntries` returns the cached array itself, and its `GetFiles` /
// `GetDirectories` list references into it. A copy per call cost the providers
// three `String`s per entry for every item in the folder.
#[test]
fn fs_directory_service_shares_the_memoized_listing() {
    let base = std::env::temp_dir().join(format!("ferrofin-dss-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&base).expect("create temp dir");
    std::fs::write(base.join("a.png"), b"x").expect("write a");
    let path = base.to_string_lossy().into_owned();

    let service = FsDirectoryService::new();
    let first = service.file_system_entries(&path);
    let second = service.file_system_entries(&path);
    assert!(
        Arc::ptr_eq(&first, &second),
        "both calls must share the one memoized listing"
    );

    std::fs::remove_dir_all(&base).ok();
}

// A directory that does not exist memoizes as an empty listing (the upstream
// `DirectoryNotFoundException → []` branch), and creating it later does not
// retroactively populate that instance.
#[test]
fn fs_directory_service_memoizes_missing_directories() {
    let base = std::env::temp_dir().join(format!("ferrofin-dsm-{}", uuid::Uuid::new_v4()));
    let path = base.to_string_lossy().into_owned();

    let service = FsDirectoryService::new();
    let file_system_entries = service.file_system_entries(&path);
    assert!(file_system_entries.is_empty(), "{file_system_entries:?}");

    std::fs::create_dir_all(&base).expect("create temp dir");
    std::fs::write(base.join("a.png"), b"x").expect("write a");
    assert!(
        service.file_system_entries(&path).is_empty(),
        "a missing directory memoizes as empty"
    );
    assert_eq!(
        FsDirectoryService::new().file_system_entries(&path).len(),
        1
    );

    std::fs::remove_dir_all(&base).ok();
}

/// A stored or found image row of `image_type` at `path`, dated `seconds`
/// past the epoch, with no dimensions.
fn image_row(path: &str, image_type: ImageType, seconds: i64) -> ItemImageInfo {
    ItemImageInfo {
        path: path.to_owned(),
        image_type,
        date_modified: chrono::DateTime::from_timestamp(seconds, 0).expect("a date"),
        width: 0,
        height: 0,
        blur_hash: None,
    }
}

/// `MergeImages`: the first found image of a singular type replaces the
/// stored one when the paths differ (a downloaded Primary gives way to the
/// season's local poster), a type found for the first time is added, a
/// stored type nothing found is kept, and a found backdrop is appended
/// unless it is stored already (ignoring case).
#[test]
fn merging_found_images_replaces_singular_types_and_appends_new_backdrops() {
    let mut images = vec![
        ItemImageInfo {
            width: 680,
            height: 1000,
            ..image_row("/metadata/library/ab/primary.jpg", ImageType::Primary, 0)
        },
        image_row("/media/Show/fanart.jpg", ImageType::Backdrop, 0),
        image_row("/media/Show/Season 1/logo.png", ImageType::Logo, 0),
    ];
    let found = [
        image_row("/media/Show/season01-poster.jpg", ImageType::Primary, 0),
        image_row("/media/Show/Season 1/poster.jpg", ImageType::Primary, 0),
        image_row("/media/Show/season01-banner.jpg", ImageType::Banner, 0),
        image_row("/media/Show/FANART.jpg", ImageType::Backdrop, 0),
        image_row("/media/Show/season01-fanart.jpg", ImageType::Backdrop, 0),
    ];
    assert!(merge_local_images(&mut images, &found));
    let rows: Vec<_> = images
        .iter()
        .map(|i| (i.path.as_str(), i.image_type, i.width))
        .collect();
    assert_eq!(
        rows,
        [
            ("/media/Show/season01-poster.jpg", ImageType::Primary, 0),
            ("/media/Show/fanart.jpg", ImageType::Backdrop, 0),
            ("/media/Show/Season 1/logo.png", ImageType::Logo, 0),
            ("/media/Show/season01-banner.jpg", ImageType::Banner, 0),
            ("/media/Show/season01-fanart.jpg", ImageType::Backdrop, 0),
        ]
    );
    let again = images.clone();
    assert!(
        !merge_local_images(&mut images, &found),
        "the same files again change nothing"
    );
    assert_eq!(images, again);
}

/// `MergeImages` takes the first found image of each singular type and every
/// backdrop, in the order found.
#[test]
fn only_the_first_found_image_of_a_singular_type_is_kept() {
    let found = vec![
        image_row("/media/Show/season01-poster.jpg", ImageType::Primary, 0),
        image_row("/media/Show/season01-fanart.jpg", ImageType::Backdrop, 0),
        image_row("/media/Show/Season 1/poster.jpg", ImageType::Primary, 0),
        image_row("/media/Show/Season 1/Season 1-logo.png", ImageType::Logo, 0),
        image_row("/media/Show/Season 1/logo.png", ImageType::Logo, 0),
        image_row("/media/Show/Season 1/fanart.jpg", ImageType::Backdrop, 0),
    ];
    let kept: Vec<_> = first_of_each_singular_type(found)
        .into_iter()
        .map(|i| i.path)
        .collect();
    assert_eq!(
        kept,
        [
            "/media/Show/season01-poster.jpg",
            "/media/Show/season01-fanart.jpg",
            "/media/Show/Season 1/Season 1-logo.png",
            "/media/Show/Season 1/fanart.jpg",
        ]
    );
}

/// An image found again at its stored path whose file time moved by more
/// than a second loses its stored dimensions and takes the new time; a
/// second or less is the database's precision.
#[test]
fn an_image_found_again_with_a_moved_file_time_loses_its_dimensions() {
    let stored = ItemImageInfo {
        width: 680,
        height: 1000,
        ..image_row("/media/Show/season01-poster.jpg", ImageType::Primary, 10)
    };
    let mut images = vec![stored.clone()];
    let within_a_second = [image_row(
        "/media/Show/season01-poster.jpg",
        ImageType::Primary,
        11,
    )];
    assert!(!merge_local_images(&mut images, &within_a_second));
    assert_eq!(images, [stored]);

    let moved = [image_row(
        "/media/Show/season01-poster.jpg",
        ImageType::Primary,
        12,
    )];
    assert!(merge_local_images(&mut images, &moved));
    assert_eq!(images, moved);
}

/// Found images become rows dated by their files, with no dimensions.
#[test]
fn found_images_become_rows_dated_by_their_files() {
    let dir = tempfile::tempdir().expect("tmp");
    let poster = dir.path().join("season01-poster.jpg");
    std::fs::write(&poster, b"x").expect("poster");
    let modified: chrono::DateTime<chrono::Utc> = std::fs::metadata(&poster)
        .and_then(|m| m.modified())
        .expect("mtime")
        .into();
    let found = LocalImageInfo {
        file_info: FileSystemMetadata {
            full_name: poster.to_string_lossy().into_owned(),
            ..FileSystemMetadata::default()
        },
        type_: ImageType::Primary,
    };
    let rows = local_image_infos(vec![found]);
    assert_eq!(
        rows,
        [ItemImageInfo {
            path: poster.to_string_lossy().into_owned(),
            image_type: ImageType::Primary,
            date_modified: modified,
            width: 0,
            height: 0,
            blur_hash: None,
        }]
    );
}
