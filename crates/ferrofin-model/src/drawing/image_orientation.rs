//! `ImageOrientation` — port of `MediaBrowser.Model.Drawing.ImageOrientation`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// EXIF image orientation (the eight standard orientation values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ImageOrientation {
    /// Row 0 top, column 0 left.
    TopLeft = 1,
    /// Row 0 top, column 0 right.
    TopRight = 2,
    /// Row 0 bottom, column 0 right.
    BottomRight = 3,
    /// Row 0 bottom, column 0 left.
    BottomLeft = 4,
    /// Row 0 left, column 0 top.
    LeftTop = 5,
    /// Row 0 right, column 0 top.
    RightTop = 6,
    /// Row 0 right, column 0 bottom.
    RightBottom = 7,
    /// Row 0 left, column 0 bottom.
    LeftBottom = 8,
}
