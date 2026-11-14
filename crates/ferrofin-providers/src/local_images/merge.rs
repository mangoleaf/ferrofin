//! The rows of the images a local image provider found, their merge onto an
//! item's stored images (`ItemImageProvider.MergeImages`), and the removal of
//! the item's own images before a refresh replaces them all
//! (`ItemImageProvider.RemoveImages`).
//!
//! Shared by the library scan and the provider manager's refresh of the
//! items the scan does not refresh, so both store what they find the same way.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::entities::ImageType;
use ferrofin_traits::options::ItemImageInfo;
use uuid::Uuid;

use crate::container_types::LocalImageInfo;

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

/// The image file's mtime as UTC, falling back to [`Utc::now`] when the stat
/// fails. Port of Jellyfin's `IFileSystem.GetLastWriteTimeUtc`, which stamps
/// `ItemImageInfo.DateModified`.
///
/// This MUST be the file's mtime, not the scan time: `date_modified` feeds both
/// the client-facing `ImageTags` (`md5(path + ticks)`) and the resized-image
/// cache key. A rescan of an unchanged file has to reproduce the same timestamp,
/// or every scan changes every poster URL (busting the clients' year-long
/// immutable image caches) and invalidates the entire server-side resize cache.
#[must_use]
pub fn file_date_modified(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_or_else(|_| Utc::now(), DateTime::<Utc>::from)
}

/// An item's own art folders under the metadata `root`: Ferrofin's
/// `{root}/{ID}` ([`own_art_dir`]), the [`legacy_art_dir`] older scans wrote
/// some art to, and the `{root}/{xx}/{id:N}` folder Jellyfin's
/// `BaseItem.GetInternalMetadataPath` uses (`BaseItem.cs:927-937`), which an
/// adopted database's images keep.
#[must_use]
pub fn internal_metadata_dirs(root: &Path, item_id: Uuid) -> [PathBuf; 3] {
    let simple = item_id.simple().to_string();
    [
        own_art_dir(root, item_id),
        legacy_art_dir(root, item_id),
        root.join(&simple[..2]).join(&simple),
    ]
}

/// The folder Ferrofin draws an item's art into: `{root}/{ID}`.
#[must_use]
pub fn own_art_dir(root: &Path, item_id: Uuid) -> PathBuf {
    root.join(guid_to_db(item_id))
}

/// `{root}/{id}`, the item's id lower-cased: where older scans wrote music,
/// person, studio and embedded-cover art, beside [`own_art_dir`]. Nothing is
/// written there any more. It is read and cleaned so that art already there
/// is found, pruned and removed with the item, and its stored paths keep
/// working.
#[must_use]
pub fn legacy_art_dir(root: &Path, item_id: Uuid) -> PathBuf {
    root.join(item_id.hyphenated().to_string())
}

/// Port of `ItemImageProvider.RemoveImages` (`ItemImageProvider.cs:74-103`,
/// run by `MetadataService.cs:112-118` for `RemoveOldMetadata` with
/// `ReplaceAllImages`): the item's images that live in its own metadata
/// `folders` ([`internal_metadata_dirs`]) — the first image of each singular
/// type (`_singularImages`, `:42-53`) and every backdrop — are taken out of
/// `images` and their files deleted (`PruneImages`), so the refresh supplies
/// them afresh. Screenshots, chapter images, a second image of a singular
/// type and anything stored outside those folders stay. Returns whether any
/// image was removed.
///
/// TODO(gl-1741): open work item: upstream also removes them wherever they
/// are stored when `IsSaveLocalMetadataEnabled()` (art saved beside the item
/// or its media); that arm is not ported yet.
pub fn remove_images(images: &mut Vec<ItemImageInfo>, folders: &[PathBuf], item_id: &str) -> bool {
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
        if !removable || !folders.iter().any(|folder| path.starts_with(folder)) {
            return true;
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
