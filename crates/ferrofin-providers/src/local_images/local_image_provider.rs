//! Filename-convention local image discovery.
//!
//! Port of `MediaBrowser.LocalMetadata.Images.LocalImageProvider` — the
//! convention-based scanner that maps files sitting next to a library item
//! (`poster.png`, `folder.jpg`, `movie-fanart.jpg`, `season01-poster.png`, …) to
//! typed [`LocalImageInfo`] entries. The algorithm, filename tables and match
//! rules are translated line-for-line from the C# so the discovered set is
//! identical.

use crate::container_types::{FileSystemMetadata, LocalImageInfo};
use crate::local_images::directory_service::{
    DirectoryService, is_supported_image_extension, supported_image_extension_index,
};
use crate::local_images::item::{ImageItem, ImageItemKind};
use ferrofin_model::entities::ImageType;

/// Primary-image filenames for common items (`_commonImageFileNames`).
const COMMON_IMAGE_FILE_NAMES: [&str; 4] = ["poster", "folder", "cover", "default"];

/// Primary-image filenames for music items (`_musicImageFileNames`).
const MUSIC_IMAGE_FILE_NAMES: [&str; 6] =
    ["folder", "poster", "cover", "jacket", "default", "albumart"];

/// Primary-image filenames for people (`_personImageFileNames`).
const PERSON_IMAGE_FILE_NAMES: [&str; 2] = ["folder", "poster"];

/// Primary-image filenames for series (`_seriesImageFileNames`).
const SERIES_IMAGE_FILE_NAMES: [&str; 5] = ["poster", "folder", "cover", "default", "show"];

/// Primary-image filenames for videos (`_videoImageFileNames`).
const VIDEO_IMAGE_FILE_NAMES: [&str; 5] = ["poster", "folder", "cover", "default", "movie"];

/// The convention-based local image provider.
///
/// Port of `LocalImageProvider`. It is stateless: [`DirectoryService`] supplies
/// all filesystem access, so a single instance serves every item.
///
/// `Name` is `"Local Images"` and `Order` is `0` upstream; those descriptor
/// values are exposed as [`LocalImageProvider::NAME`] / [`LocalImageProvider::ORDER`].
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalImageProvider;

impl LocalImageProvider {
    /// The provider name (`Name => "Local Images"`).
    pub const NAME: &'static str = "Local Images";

    /// The provider order (`Order => 0`).
    pub const ORDER: i32 = 0;

    /// Creates a local image provider.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Whether this provider handles `item` (`Supports`).
    ///
    /// Port of `LocalImageProvider.Supports`: everything that supports local
    /// metadata *except* episodes (their own provider), songs (`Audio` but not
    /// `AudioBook`) and photos. The C# virtual-location season-from-series
    /// branch has no analogue without a live library, so a virtual item is
    /// unsupported here.
    #[must_use]
    pub fn supports(item: &ImageItem) -> bool {
        !matches!(
            item.kind,
            ImageItemKind::Episode | ImageItemKind::Audio | ImageItemKind::Photo
        )
    }

    /// Discovers local images for `item` by scanning its containing folder.
    ///
    /// Port of `GetImages(BaseItem, IDirectoryService)`: lists the folder
    /// (directories included, so `extrafanart` can be found) then populates the
    /// image set with parent-series support enabled.
    #[must_use]
    pub fn get_images<D: DirectoryService>(
        item: &ImageItem,
        directory_service: &D,
    ) -> Vec<LocalImageInfo> {
        let files = Self::get_files(item, true, directory_service);
        let mut list = Vec::new();
        Self::populate_images(item, &mut list, &files, true, directory_service);
        list
    }

    /// Discovers local images for `item` from the given explicit `paths`.
    ///
    /// Port of `GetImages(BaseItem, IEnumerable<string>, IDirectoryService)` used
    /// by the collection-folder and internal-metadata providers: the image files
    /// under each path are gathered (recursively), sorted by supported-extension
    /// order, then populated with parent-series support disabled.
    #[must_use]
    pub fn get_images_from_paths<D: DirectoryService>(
        item: &ImageItem,
        paths: &[String],
        directory_service: &D,
    ) -> Vec<LocalImageInfo> {
        let mut files: Vec<FileSystemMetadata> = paths
            .iter()
            .flat_map(|p| directory_service.image_files(p, true))
            .collect();
        files.sort_by_key(|f| {
            supported_image_extension_index(f.extension.as_deref().unwrap_or_default())
        });
        let mut list = Vec::new();
        Self::populate_images(item, &mut list, &files, false, directory_service);
        list
    }

    /// Lists the containing folder's entries, filtered to image files (and,
    /// when `include_directories`, subdirectories) and ordered by
    /// supported-extension index.
    ///
    /// Port of the private `GetFiles`. A missing / non-file-protocol item yields
    /// no files.
    fn get_files<D: DirectoryService>(
        item: &ImageItem,
        include_directories: bool,
        directory_service: &D,
    ) -> Vec<FileSystemMetadata> {
        let Some(path) = item.containing_folder_path.as_deref() else {
            return Vec::new();
        };

        let mut files: Vec<FileSystemMetadata> = directory_service
            .file_system_entries(path)
            .into_iter()
            .filter(|i| {
                (include_directories && i.is_directory)
                    || i.extension
                        .as_deref()
                        .is_some_and(is_supported_image_extension)
            })
            .collect();
        // OrderBy is a stable sort in C#/LINQ; sort_by_key is stable in Rust.
        files.sort_by_key(|i| {
            supported_image_extension_index(i.extension.as_deref().unwrap_or_default())
        });
        files
    }

    /// Populates `images` from `files` following the full per-kind convention.
    ///
    /// Port of the private `PopulateImages`.
    fn populate_images<D: DirectoryService>(
        item: &ImageItem,
        images: &mut Vec<LocalImageInfo>,
        files: &[FileSystemMetadata],
        support_parent_series_files: bool,
        directory_service: &D,
    ) {
        if support_parent_series_files && item.kind == ImageItemKind::Season {
            Self::populate_season_images_from_series_folder(item, images, directory_service);
        }

        let image_prefix = format!(
            "{}-",
            item.file_name_without_extension.as_deref().unwrap_or("")
        );
        let is_in_mixed_folder = item.is_in_mixed_folder;

        Self::populate_primary_images(item, images, files, &image_prefix, is_in_mixed_folder);

        let is_episode = item.kind == ImageItemKind::Episode;
        let is_song = item.kind.is_song();
        let is_person = item.kind == ImageItemKind::Person;
        let non_av = !is_episode && !is_song && !is_person;

        // Logo
        if non_av {
            let added = Self::add_image_both(
                files,
                images,
                "logo",
                &image_prefix,
                is_in_mixed_folder,
                ImageType::Logo,
            );
            if !added {
                Self::add_image_both(
                    files,
                    images,
                    "clearlogo",
                    &image_prefix,
                    is_in_mixed_folder,
                    ImageType::Logo,
                );
            }
        }

        // Art
        if non_av {
            Self::add_image_both(
                files,
                images,
                "clearart",
                &image_prefix,
                is_in_mixed_folder,
                ImageType::Art,
            );
        }

        // Disc
        Self::populate_disc_images(item, images, files, &image_prefix, is_in_mixed_folder);

        // Banner
        if non_av {
            Self::add_image_both(
                files,
                images,
                "banner",
                &image_prefix,
                is_in_mixed_folder,
                ImageType::Banner,
            );
        }

        // Thumb — landscape preferred, then thumb.
        if non_av {
            let added = Self::add_image_both(
                files,
                images,
                "landscape",
                &image_prefix,
                is_in_mixed_folder,
                ImageType::Thumb,
            );
            if !added {
                Self::add_image_both(
                    files,
                    images,
                    "thumb",
                    &image_prefix,
                    is_in_mixed_folder,
                    ImageType::Thumb,
                );
            }
        }

        if non_av {
            Self::populate_backdrops(
                item,
                images,
                files,
                &image_prefix,
                is_in_mixed_folder,
                directory_service,
            );
        }
    }

    /// Adds the disc image, honouring the per-kind preference order.
    ///
    /// Port of the disc branch of `PopulateImages`: music albums prefer `cdart`
    /// then `disc`; videos and box sets prefer `disc`, then `cdart`, then
    /// `discart`; other kinds get no disc image.
    fn populate_disc_images(
        item: &ImageItem,
        images: &mut Vec<LocalImageInfo>,
        files: &[FileSystemMetadata],
        image_prefix: &str,
        is_in_mixed_folder: bool,
    ) {
        let candidates: &[&str] = if item.kind == ImageItemKind::MusicAlbum {
            &["cdart", "disc"]
        } else if item.kind.is_video() || item.kind == ImageItemKind::BoxSet {
            &["disc", "cdart", "discart"]
        } else {
            return;
        };

        for name in candidates {
            if Self::add_image_both(
                files,
                images,
                name,
                image_prefix,
                is_in_mixed_folder,
                ImageType::Disc,
            ) {
                return;
            }
        }
    }

    /// Selects the primary-image filename table and adds the first match.
    ///
    /// Port of `PopulatePrimaryImages`.
    fn populate_primary_images(
        item: &ImageItem,
        images: &mut Vec<LocalImageInfo>,
        files: &[FileSystemMetadata],
        image_prefix: &str,
        is_in_mixed_folder: bool,
    ) {
        let image_file_names: &[&str] = match item.kind {
            ImageItemKind::MusicAlbum | ImageItemKind::MusicArtist | ImageItemKind::PhotoAlbum => {
                &MUSIC_IMAGE_FILE_NAMES
            }
            ImageItemKind::Person => &PERSON_IMAGE_FILE_NAMES,
            ImageItemKind::Series => &SERIES_IMAGE_FILE_NAMES,
            // `Video && not Episode` — a plain/movie/music video.
            ImageItemKind::Video | ImageItemKind::Movie | ImageItemKind::MusicVideo => {
                &VIDEO_IMAGE_FILE_NAMES
            }
            _ => &COMMON_IMAGE_FILE_NAMES,
        };

        if let Some(name) = item.file_name_without_extension.as_deref()
            && !name.is_empty()
            && Self::add_image(files, images, name, ImageType::Primary, None)
        {
            return;
        }

        for name in image_file_names {
            if Self::add_image(files, images, name, ImageType::Primary, Some(image_prefix)) {
                return;
            }
        }

        if !is_in_mixed_folder {
            for name in image_file_names {
                if Self::add_image(files, images, name, ImageType::Primary, None) {
                    return;
                }
            }
        }
    }

    /// Adds all backdrop images (`fanart`, `background`, `art`, `backdrop`,
    /// per-item `-fanart`, and any `extrafanart/` folder).
    ///
    /// Port of the item-level `PopulateBackdrops`.
    fn populate_backdrops<D: DirectoryService>(
        item: &ImageItem,
        images: &mut Vec<LocalImageInfo>,
        files: &[FileSystemMetadata],
        image_prefix: &str,
        is_in_mixed_folder: bool,
        directory_service: &D,
    ) {
        if item.path.as_deref().is_some_and(|p| !p.is_empty())
            && let Some(name) = item.file_name_without_extension.as_deref()
            && !name.is_empty()
        {
            let fanart = format!("{name}-fanart");
            Self::add_image(
                files,
                images,
                &fanart,
                ImageType::Backdrop,
                Some(image_prefix),
            );
            if !is_in_mixed_folder {
                Self::add_image(files, images, &fanart, ImageType::Backdrop, None);
            }
        }

        Self::populate_backdrops_named(
            images,
            files,
            image_prefix,
            "fanart",
            "fanart-",
            is_in_mixed_folder,
            ImageType::Backdrop,
        );
        Self::populate_backdrops_named(
            images,
            files,
            image_prefix,
            "background",
            "background-",
            is_in_mixed_folder,
            ImageType::Backdrop,
        );
        Self::populate_backdrops_named(
            images,
            files,
            image_prefix,
            "art",
            "art-",
            is_in_mixed_folder,
            ImageType::Backdrop,
        );

        if let Some(extra_fanart) = files
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case("extrafanart"))
        {
            Self::populate_backdrops_from_extra_fanart(
                &extra_fanart.full_name,
                images,
                directory_service,
            );
        }

        Self::populate_backdrops_named(
            images,
            files,
            image_prefix,
            "backdrop",
            "backdrop",
            is_in_mixed_folder,
            ImageType::Backdrop,
        );
    }

    /// Adds every non-empty image file inside an `extrafanart` folder as a
    /// backdrop.
    ///
    /// Port of `PopulateBackdropsFromExtraFanart`.
    fn populate_backdrops_from_extra_fanart<D: DirectoryService>(
        path: &str,
        images: &mut Vec<LocalImageInfo>,
        directory_service: &D,
    ) {
        for file in directory_service.image_files(path, false) {
            if file.length > 0 {
                images.push(LocalImageInfo {
                    file_info: file,
                    type_: ImageType::Backdrop,
                });
            }
        }
    }

    /// Adds up to 20 numbered backdrops for a `firstFileName` / `prefix`
    /// convention, bailing after 3 consecutive misses.
    ///
    /// Port of the six-argument private `PopulateBackdrops`.
    fn populate_backdrops_named(
        images: &mut Vec<LocalImageInfo>,
        files: &[FileSystemMetadata],
        image_prefix: &str,
        first_file_name: &str,
        subsequent_file_name_prefix: &str,
        is_in_mixed_folder: bool,
        type_: ImageType,
    ) {
        let first = format!("{image_prefix}{first_file_name}");
        Self::add_image(files, images, &first, type_, None);

        let mut unfound = 0;
        for i in 1..=20 {
            let candidate = format!("{image_prefix}{subsequent_file_name_prefix}{i}");
            if !Self::add_image(files, images, &candidate, type_, None) {
                unfound += 1;
                if unfound >= 3 {
                    break;
                }
            }
        }

        if !is_in_mixed_folder {
            Self::add_image(files, images, first_file_name, type_, None);

            unfound = 0;
            for i in 1..=20 {
                let candidate = format!("{subsequent_file_name_prefix}{i}");
                if !Self::add_image(files, images, &candidate, type_, None) {
                    unfound += 1;
                    if unfound >= 3 {
                        break;
                    }
                }
            }
        }
    }

    /// Pulls season poster/fanart/banner/landscape images from the parent series
    /// folder, using the season name and the `seasonNN` / `-specials` markers.
    ///
    /// Port of `PopulateSeasonImagesFromSeriesFolder`. Requires the season's
    /// `index_number`, its `name` (used as a filename prefix) and the parent
    /// series folder (carried on `containing_folder_path` of the standalone
    /// [`ImageItem`] passed as the season here — see the module tests).
    fn populate_season_images_from_series_folder<D: DirectoryService>(
        season: &ImageItem,
        images: &mut Vec<LocalImageInfo>,
        directory_service: &D,
    ) {
        let Some(season_number) = season.index_number else {
            return;
        };
        // The parent series folder is the season's containing folder here.
        let Some(series_folder) = season.containing_folder_path.as_deref() else {
            return;
        };

        let series_files: Vec<FileSystemMetadata> = {
            let mut f: Vec<FileSystemMetadata> = directory_service
                .file_system_entries(series_folder)
                .into_iter()
                .filter(|i| {
                    !i.is_directory
                        && i.extension
                            .as_deref()
                            .is_some_and(is_supported_image_extension)
                })
                .collect();
            f.sort_by_key(|i| {
                supported_image_extension_index(i.extension.as_deref().unwrap_or_default())
            });
            f
        };

        // Try using the season name (spaces stripped, lower-cased).
        let prefix = season.name.replace(' ', "").to_lowercase();

        let mut filename_prefixes = vec![prefix.clone()];

        let season_marker = if season_number == 0 {
            "-specials".to_owned()
        } else {
            format!("{season_number:02}")
        };

        if !prefix.eq_ignore_ascii_case(&season_marker) {
            filename_prefixes.push(format!("season{season_marker}"));
        }

        for filename in &filename_prefixes {
            Self::add_image(
                &series_files,
                images,
                &format!("{filename}-poster"),
                ImageType::Primary,
                None,
            );
            Self::add_image(
                &series_files,
                images,
                &format!("{filename}-fanart"),
                ImageType::Backdrop,
                None,
            );
            Self::add_image(
                &series_files,
                images,
                &format!("{filename}-banner"),
                ImageType::Banner,
                None,
            );
            Self::add_image(
                &series_files,
                images,
                &format!("{filename}-landscape"),
                ImageType::Thumb,
                None,
            );
        }
    }

    /// Adds an image by `name` with the prefix, and — when not in a mixed folder
    /// — again without the prefix.
    ///
    /// Port of the six-argument private `AddImage`.
    fn add_image_both(
        files: &[FileSystemMetadata],
        images: &mut Vec<LocalImageInfo>,
        name: &str,
        image_prefix: &str,
        is_in_mixed_folder: bool,
        type_: ImageType,
    ) -> bool {
        let mut added = Self::add_image(files, images, name, type_, Some(image_prefix));
        if !is_in_mixed_folder && Self::add_image(files, images, name, type_, None) {
            added = true;
        }
        added
    }

    /// Finds the file matching `name` (optionally with `prefix`) and appends a
    /// typed [`LocalImageInfo`] for it.
    ///
    /// Port of the static `AddImage`. Returns whether a file was found.
    fn add_image(
        files: &[FileSystemMetadata],
        images: &mut Vec<LocalImageInfo>,
        name: &str,
        type_: ImageType,
        prefix: Option<&str>,
    ) -> bool {
        match Self::get_image(files, name, prefix) {
            None => false,
            Some(image) => {
                images.push(LocalImageInfo {
                    file_info: image.clone(),
                    type_,
                });
                true
            }
        }
    }

    /// Finds the first non-directory, non-empty file whose name-without-extension
    /// is exactly `prefix + name` (case-insensitive, exact length).
    ///
    /// Port of the static `GetImage`. The exact-length check prevents
    /// `poster.png` from matching a query for `post`, etc.
    fn get_image<'a>(
        files: &'a [FileSystemMetadata],
        name: &str,
        prefix: Option<&str>,
    ) -> Option<&'a FileSystemMetadata> {
        let prefix = prefix.unwrap_or("");
        let file_name_length = name.chars().count() + prefix.chars().count();

        files.iter().find(|file| {
            if file.is_directory || file.length <= 0 {
                return false;
            }
            let file_name = file_name_without_extension(&file.full_name);
            file_name.chars().count() == file_name_length
                && starts_with_ignore_case(file_name, prefix)
                && ends_with_ignore_case(file_name, name)
        })
    }
}

/// Returns the filename without its final extension, from a full path.
///
/// Port of `Path.GetFileNameWithoutExtension`.
pub(crate) fn file_name_without_extension(full_name: &str) -> &str {
    let file_name = full_name.rsplit(['/', '\\']).next().unwrap_or(full_name);
    match file_name.rfind('.') {
        // A leading dot (`.hidden`) is not an extension separator.
        Some(0) | None => file_name,
        Some(idx) => &file_name[..idx],
    }
}

/// Case-insensitive `str::starts_with` over ASCII, matching C#
/// `StringComparison.OrdinalIgnoreCase`.
fn starts_with_ignore_case(haystack: &str, prefix: &str) -> bool {
    haystack
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|h| h.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Case-insensitive `str::ends_with` over ASCII, matching C#
/// `StringComparison.OrdinalIgnoreCase`.
fn ends_with_ignore_case(haystack: &str, suffix: &str) -> bool {
    haystack
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| haystack.as_bytes().get(start..))
        .is_some_and(|h| h.eq_ignore_ascii_case(suffix.as_bytes()))
}
