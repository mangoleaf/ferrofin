//! `ImageOrientation` — port of `MediaBrowser.Model.Drawing.ImageOrientation`.

use serde::Serialize;
use utoipa::ToSchema;

/// EXIF image orientation (the eight standard orientation values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    ImageOrientation, None, {
        TopLeft => ("TopLeft", 1),
        TopRight => ("TopRight", 2),
        BottomRight => ("BottomRight", 3),
        BottomLeft => ("BottomLeft", 4),
        LeftTop => ("LeftTop", 5),
        RightTop => ("RightTop", 6),
        RightBottom => ("RightBottom", 7),
        LeftBottom => ("LeftBottom", 8),
    }
}
