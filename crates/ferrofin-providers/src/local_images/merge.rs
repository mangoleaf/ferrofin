//! The rows of the images a local image provider found, their merge onto an
//! item's stored images (`ItemImageProvider.MergeImages`), and the removal of
//! the item's own images before a refresh replaces them all
//! (`ItemImageProvider.RemoveImages`).
//!
//! Shared by the library scan and the provider manager's refresh of the
//! items the scan does not refresh, so both store what they find the same way.

use std::path::{Path, PathBuf};

pub use ferrofin_db::art::{earlier_art_dir, internal_image_stem, legacy_art_dir, own_art_dir};
use ferrofin_model::entities::ImageType;
use ferrofin_traits::options::{ItemImageInfo, file_date_modified};
use uuid::Uuid;

use crate::container_types::LocalImageInfo;
use crate::image_save::BesideArt;

/// Image rows for images a local provider found, dated by their files.
///
/// Dimensions are left `0` (unknown) and the blurhash empty, as
/// `BaseItem.SetImagePath` leaves them.
#[must_use]
pub fn local_image_infos(found: Vec<LocalImageInfo>) -> Vec<ItemImageInfo> {
    found
        .into_iter()
        .map(|local| ItemImageInfo {
            date_modified: file_date_modified(Path::new(&local.file_info.full_name)),
            path: local.file_info.full_name,
            image_type: local.type_,
            width: 0,
            height: 0,
            blur_hash: None,
        })
        .collect()
}

/// What `MergeImages` takes from a found set, in its order: the first image
/// of each singular type (`GetFirstLocalImageInfoByType`) and every backdrop
/// (`UpdateMultiImages`). A season's series-folder poster and its own
/// folder's are both found, the series folder's first: only it is the
/// season's Primary. The local image providers find only upstream's singular
/// types (`_singularImages`) and backdrops, so every other type is kept as a
/// singular one here.
#[must_use]
pub fn first_of_each_singular_type(found: Vec<ItemImageInfo>) -> Vec<ItemImageInfo> {
    let mut seen: Vec<ImageType> = Vec::new();
    found
        .into_iter()
        .filter(|image| {
            if image.image_type == ImageType::Backdrop {
                return true;
            }
            if seen.contains(&image.image_type) {
                return false;
            }
            seen.push(image.image_type);
            true
        })
        .collect()
}

/// Port of `ItemImageProvider.MergeImages` for images found on disk: for each
/// singular type, the first found image replaces the item's first image of
/// that type when the paths differ (ignoring case). Found backdrops are added
/// as `BaseItem.AddImages` adds them: one not stored yet is appended, and
/// none is removed or reordered. An image found again at the path it is
/// stored under is refreshed (`refresh_found`). Returns whether anything is to
/// be written.
pub fn merge_local_images(images: &mut Vec<ItemImageInfo>, found: &[ItemImageInfo]) -> bool {
    let mut changed = false;
    let mut singular: Vec<ImageType> = Vec::new();
    for image in found {
        if image.image_type != ImageType::Backdrop && !singular.contains(&image.image_type) {
            singular.push(image.image_type);
        }
    }
    for image_type in singular {
        let Some(new) = found.iter().find(|i| i.image_type == image_type) else {
            continue;
        };
        match images.iter_mut().find(|i| i.image_type == image_type) {
            Some(current) if current.path.eq_ignore_ascii_case(&new.path) => {
                changed |= refresh_found(current, new);
            }
            Some(current) => {
                *current = new.clone();
                changed = true;
            }
            None => {
                images.push(new.clone());
                changed = true;
            }
        }
    }
    for new in found.iter().filter(|i| i.image_type == ImageType::Backdrop) {
        if let Some(current) = images
            .iter_mut()
            .find(|i| i.image_type == ImageType::Backdrop && i.path.eq_ignore_ascii_case(&new.path))
        {
            changed |= refresh_found(current, new);
        } else {
            images.push(new.clone());
            changed = true;
        }
    }
    changed
}

/// An image found again at the path it is stored under: when its file's
/// modification time moved by more than a second and it has stored
/// dimensions, they are cleared and the time taken, and the row is to be
/// written. A smaller difference is the database's precision, not a change
/// (`MergeImages`/`AddImages`: "an exact comparison would treat every
/// unchanged image as modified").
fn refresh_found(current: &mut ItemImageInfo, new: &ItemImageInfo) -> bool {
    let moved = (current.date_modified - new.date_modified)
        .num_milliseconds()
        .abs()
        > 1000;
    if moved && (current.width > 0 || current.height > 0) {
        current.width = 0;
        current.height = 0;
        current.date_modified = new.date_modified;
        return true;
    }
    false
}

/// An item's own art folders under the metadata `root`, the one art is saved
/// in first: [`own_art_dir`], which an adopted database's images keep too,
/// then the [`earlier_art_dir`] and the [`legacy_art_dir`] earlier builds
/// saved art in.
#[must_use]
pub fn internal_metadata_dirs(root: &Path, item_id: Uuid) -> [PathBuf; 3] {
    [
        own_art_dir(root, item_id),
        earlier_art_dir(root, item_id),
        legacy_art_dir(root, item_id),
    ]
}

/// Port of `ItemImageProvider.RemoveImages` (`ItemImageProvider.cs:74-103`,
/// run by `MetadataService.cs:112-118` for `RemoveOldMetadata` with
/// `ReplaceAllImages`): the item's images that live in its own metadata
/// `folders` ([`internal_metadata_dirs`]) — the first image of each singular
/// type (`_singularImages`, `:42-53`) and every backdrop — are taken out of
/// `images` and their files deleted (`PruneImages`), so the refresh supplies
/// them afresh. Screenshots, chapter images and a second image of a singular
/// type always stay. One stored outside those folders stays too, unless the
/// item's art is saved `beside` it (`item.IsSaveLocalMetadataEnabled()`) and
/// [`BesideArt::removes`] takes it: a remote one's row goes without a file to
/// delete. Returns whether any image was removed.
pub fn remove_images(
    images: &mut Vec<ItemImageInfo>,
    folders: &[PathBuf],
    item_id: &str,
    beside: Option<&BesideArt>,
) -> bool {
    let before = images.len();
    let mut seen: Vec<ImageType> = Vec::new();
    images.retain(|image| {
        let singular = matches!(
            image.image_type,
            ImageType::Primary
                | ImageType::Art
                | ImageType::Banner
                | ImageType::Box
                | ImageType::BoxRear
                | ImageType::Disc
                | ImageType::Logo
                | ImageType::Menu
                | ImageType::Thumb
        );
        // `GetImageInfo(type, 0)`: a singular type's first image only.
        let removable = image.image_type == ImageType::Backdrop
            || (singular && !seen.contains(&image.image_type));
        if singular {
            seen.push(image.image_type);
        }
        let path = Path::new(&image.path);
        let own = folders.iter().any(|folder| path.starts_with(folder));
        if !removable || !(own || beside.is_some_and(|beside| beside.removes(image))) {
            return true;
        }
        // A remote image: the row goes, there is no file (`PruneImages`).
        if !image.is_local_file() {
            return false;
        }
        match std::fs::remove_file(path) {
            // Two image types can share one file.
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(%err, item_id, path = %path.display(), "failed to remove an image to replace");
            }
            _ => {}
        }
        false
    });
    images.len() != before
}
