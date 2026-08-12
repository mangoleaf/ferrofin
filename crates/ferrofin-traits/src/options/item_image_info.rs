//! Port of `MediaBrowser.Controller.Entities.ItemImageInfo`.

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

impl ItemImageInfo {
    /// Returns `true` when [`path`](Self::path) is a local file rather than a
    /// remote `http(s)` URL.
    ///
    /// Matches the C# `IsLocalFile` computed property (a case-insensitive
    /// `!Path.StartsWith("http")` test).
    #[must_use]
    pub fn is_local_file(&self) -> bool {
        !self.path.to_ascii_lowercase().starts_with("http")
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageType, ItemImageInfo};

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
