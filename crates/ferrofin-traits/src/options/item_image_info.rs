//! Port of `MediaBrowser.Controller.Entities.ItemImageInfo`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ferrofin_model::entities::ImageType;
use serde::{Deserialize, Serialize};

/// A single image attached to an item — its path, kind, dimensions and hash.
///
/// Mirrors C# `ItemImageInfo`. `Path` is `required` in C#; here it is a plain
/// `String` (constructed, not deserialized-from-partial), so [`Default`] gives
/// an empty path. The C# computed `IsLocalFile` property becomes
/// [`ItemImageInfo::is_local_file`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemImageInfo {
    /// The path to the image (a local file path or an `http(s)` URL).
    pub path: String,

    /// The kind of image this is (primary, backdrop, logo, …).
    #[serde(rename = "Type")]
    pub image_type: ImageType,

    /// When the image was last modified.
    pub date_modified: DateTime<Utc>,

    /// The image width in pixels (0 when unknown).
    pub width: i32,

    /// The image height in pixels (0 when unknown).
    pub height: i32,

    /// The blurhash placeholder string, if computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blur_hash: Option<String>,
}

/// An item's image rows with what their owner decides about serving them, read
/// together so that serving an image costs one query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemImagesToServe {
    /// The image rows, in stored order.
    pub images: Vec<ItemImageInfo>,

    /// `ImageProcessor.ProcessImage`'s `autoOrient`: true for a `Photo` whose
    /// stored orientation is unknown or not `TopLeft`.
    pub auto_orient: bool,

    /// Whether the owner is a Live TV channel or programme, whose rows hold
    /// what a tuner or a guide supplied rather than what a scan found.
    pub live_tv: bool,
}

impl ItemImageInfo {
    /// Returns `true` when [`path`](Self::path) is a local file rather than a
    /// remote `http(s)` URL.
    ///
    /// Matches the C# `IsLocalFile` computed property (a case-insensitive
    /// `!Path.StartsWith("http")` test).
    #[must_use]
    pub fn is_local_file(&self) -> bool {
        Self::is_local_path(&self.path)
    }

    /// [`is_local_file`](Self::is_local_file) for a path without its row.
    #[must_use]
    pub fn is_local_path(path: &str) -> bool {
        !path.to_ascii_lowercase().starts_with("http")
    }
}

/// Whether a Live TV channel's or programme's image `path` is in the internal
/// metadata directory `metadata_root`, the only place its file may be read
/// from, deleted or exchanged with another.
///
/// Those rows hold what a tuner or a guide supplied, which is kept only as a URL
/// (`ferrofin_livetv::projection::is_artwork_url`), but a row saved before that
/// rule, or by the previous version during a rolling update, can hold a value
/// that is not one. The metadata directory is where the server itself writes (a
/// logo set in the metadata editor, or the logo an adopted Jellyfin database
/// downloaded). The paths are resolved before they are compared. A row whose
/// file is gone goes by the nearest ancestor that exists, so one in the metadata
/// directory can still be listed and deleted. Every other item's images may
/// legitimately be anywhere in its library.
pub async fn live_tv_image_is_internal(path: &str, metadata_root: &Path) -> bool {
    let path = PathBuf::from(path);
    let root = metadata_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Ok(root) = std::fs::canonicalize(root) else {
            return false;
        };
        path.ancestors()
            .find_map(|ancestor| std::fs::canonicalize(ancestor).ok())
            .is_some_and(|resolved| resolved.starts_with(root))
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{ImageType, ItemImageInfo, live_tv_image_is_internal};

    /// No metadata directory on disk means no file is accepted, and neither
    /// is a path nothing resolves, however it is spelled.
    #[tokio::test]
    async fn live_tv_image_needs_a_root_and_a_path_that_resolve() {
        let dir = std::env::temp_dir();
        let inside = dir.join("art.png");
        let inside = inside.to_string_lossy();
        assert!(live_tv_image_is_internal(&inside, &dir).await);
        assert!(!live_tv_image_is_internal(&inside, &dir.join("no-such-root")).await);
        assert!(!live_tv_image_is_internal("", &dir).await);
        assert!(!live_tv_image_is_internal("https://art.invalid/a.png", &dir).await);
    }

    #[test]
    fn default_is_empty_primary() {
        let info = ItemImageInfo::default();
        assert_eq!(info.path, "");
        assert_eq!(info.image_type, ImageType::Primary);
        assert_eq!(info.width, 0);
        assert!(info.blur_hash.is_none());
    }

    #[test]
    fn is_local_file_detects_urls() {
        let mut info = ItemImageInfo {
            path: "/library/poster.jpg".into(),
            ..Default::default()
        };
        assert!(info.is_local_file());

        info.path = "https://cdn.example/poster.jpg".into();
        assert!(!info.is_local_file());

        info.path = "HTTP://cdn.example/poster.jpg".into();
        assert!(!info.is_local_file());
    }

    #[test]
    fn serde_round_trips() {
        let info = ItemImageInfo {
            path: "/p.jpg".into(),
            image_type: ImageType::Backdrop,
            width: 1920,
            height: 1080,
            blur_hash: Some("LKO2".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let back: ItemImageInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(info, back);
    }
}
