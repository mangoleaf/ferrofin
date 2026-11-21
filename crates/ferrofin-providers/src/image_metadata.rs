//! What `LibraryManager.UpdateImagesAsync` records for an image the server
//! saved: its size and blurhash, read from the file, and which images need
//! that (`ImageNeedsRefresh`). The measuring is shared by the library scan and
//! the provider manager's saves, so both record an image the same way; the
//! scan judges which images need it against its own pre-read of the rows.

use std::path::Path;

use ferrofin_model::drawing::ImageDimensions;
use ferrofin_traits::drawing::ImageProcessor;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{ItemImageInfo, file_date_modified};
use uuid::Uuid;

/// Dimensions + blurhash for a written image file, the way the scanner records
/// every other image (so the DTO carries `Width`/`Height`/`ImageBlurHashes`):
/// what `LibraryManager.UpdateImagesAsync` fills for a saved image. `path` is
/// the file to read: `image.path` itself, or its expansion when the row stores
/// a virtual path.
///
/// As upstream: a missing file leaves the image as it is; a size that cannot
/// be read is stored 0x0, and a blurhash that cannot be computed empty (a 0x0
/// image's blurhash is empty too, `ImageProcessor.GetImageBlurHash`). Each
/// failure is logged with the item, the image type and the path, the events
/// `UpdateImagesAsync` and its encoder log, at levels set by LOGGING.md rule 1:
/// a missing file, an error reading the size and a failed blurhash at `warn!`
/// (upstream: Warning, Error, Error), a file measured 0x0 at `debug!`
/// (upstream: Debug when empty or unsupported, Warning when undecodable, which
/// this encoder cannot tell apart).
pub async fn fill_image_metadata(
    processor: &dyn ImageProcessor,
    item_id: Uuid,
    path: &str,
    image: &mut ItemImageInfo,
) {
    let image_type = image.image_type;
    let dims = match processor.get_image_dimensions(path).await {
        Ok(dims) => dims,
        Err(ServiceError::NotFound(_)) => {
            tracing::warn!(item_id = %item_id, ?image_type, path = %path, "image not found");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, item_id = %item_id, ?image_type, path = %path, "cannot get image dimensions");
            ImageDimensions {
                width: 0,
                height: 0,
            }
        }
    };
    if dims.width <= 0 || dims.height <= 0 {
        // The encoder answers an empty, unsupported or undecodable file 0x0,
        // and cannot say which: debug, as an SVG logo (no decoder, gl-802) is
        // one per movie, measured again on every scan.
        tracing::debug!(item_id = %item_id, ?image_type, path = %path, "unable to determine image dimensions");
    }
    image.width = dims.width;
    image.height = dims.height;
    image.blur_hash = Some(
        match processor.get_image_blur_hash_sized(path, dims).await {
            Ok(hash) => hash,
            Err(error) => {
                tracing::warn!(%error, item_id = %item_id, ?image_type, path = %path, "cannot compute blurhash");
                String::new()
            }
        },
    );
}

/// `LibraryManager.ImageNeedsRefresh` for a local image: its size or blurhash
/// is missing, or its recorded date is more than a second from its file's. A
/// file that is gone needs it too: upstream reads its time as a date far from
/// any stored one (`ManagedFileSystem.GetLastWriteTimeUtc`), and the save then
/// drops its row (`ValidateImages`).
#[must_use]
pub fn image_needs_refresh(image: &ItemImageInfo) -> bool {
    if image.width == 0 || image.height == 0 || image.blur_hash.as_deref().is_none_or(str::is_empty)
    {
        return true;
    }
    let path = Path::new(&image.path);
    if std::fs::metadata(path).is_err() {
        return true;
    }
    (image.date_modified - file_date_modified(path))
        .num_milliseconds()
        .abs()
        > 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::entities::ImageType;

    /// `ImageNeedsRefresh` for a local file: a missing width, height or
    /// blurhash, a date more than a second from the file's, or a file that is
    /// gone.
    #[test]
    fn an_image_needs_a_refresh_as_upstream_says() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("poster.png");
        std::fs::write(&path, b"art").unwrap();
        let file_time = file_date_modified(&path);
        let image = |width, height, hash: Option<&str>, offset_ms: i64| ItemImageInfo {
            path: path.to_string_lossy().into_owned(),
            image_type: ImageType::Primary,
            date_modified: file_time + chrono::TimeDelta::milliseconds(offset_ms),
            width,
            height,
            blur_hash: hash.map(str::to_owned),
        };
        assert!(!image_needs_refresh(&image(10, 20, Some("LKO2"), 0)));
        assert!(!image_needs_refresh(&image(10, 20, Some("LKO2"), 1000)));
        assert!(image_needs_refresh(&image(10, 20, Some("LKO2"), 1001)));
        assert!(image_needs_refresh(&image(0, 20, Some("LKO2"), 0)));
        assert!(image_needs_refresh(&image(10, 0, Some("LKO2"), 0)));
        assert!(image_needs_refresh(&image(10, 20, Some(""), 0)));
        assert!(image_needs_refresh(&image(10, 20, None, 0)));
        let mut gone = image(10, 20, Some("LKO2"), 0);
        gone.path = tmp.path().join("gone.png").to_string_lossy().into_owned();
        assert!(image_needs_refresh(&gone), "a file that is gone");
    }
}
