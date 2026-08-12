//! `ImageInfo` — port of `MediaBrowser.Model.Dto.ImageInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entities::ImageType;

/// Class `ImageInfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ImageInfo {
    /// Gets or sets the type of the image.
    pub image_type: ImageType,

    /// Gets or sets the index of the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_index: Option<i32>,

    /// Gets or sets the image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,

    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Gets or sets the blurhash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blur_hash: Option<String>,

    /// Gets or sets the height.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,

    /// Gets or sets the width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,

    /// Gets or sets the size.
    pub size: i64,
}
