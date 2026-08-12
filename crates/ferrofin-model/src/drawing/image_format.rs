//! `ImageFormat` — port of `MediaBrowser.Model.Drawing.ImageFormat` and its
//! `ImageFormatExtensions`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Enum `ImageOutputFormat` — the image encodings the server can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ImageFormat {
    /// BMP format.
    Bmp,
    /// GIF format.
    Gif,
    /// JPG format.
    Jpg,
    /// PNG format.
    Png,
    /// WEBP format.
    Webp,
    /// SVG format.
    Svg,
}

/// Error returned when an integer does not correspond to a valid
/// [`ImageFormat`] discriminant.
///
/// Mirrors the C# `InvalidEnumArgumentException` thrown by
/// `ImageFormatExtensions` for out-of-range enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid ImageFormat enum value: {0}")]
pub struct UnknownImageFormat(pub i32);

impl ImageFormat {
    /// Returns the correct mime type for this image format.
    #[must_use]
    pub fn mime_type(self) -> &'static str {
        match self {
            // MediaTypeNames.Image.* constants from System.Net.Mime.
            Self::Bmp => "image/bmp",
            Self::Gif => "image/gif",
            Self::Jpg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Svg => "image/svg+xml",
        }
    }

    /// Returns the correct file extension (with leading dot) for this image
    /// format.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Bmp => ".bmp",
            Self::Gif => ".gif",
            Self::Jpg => ".jpg",
            Self::Png => ".png",
            Self::Webp => ".webp",
            Self::Svg => ".svg",
        }
    }
}

impl TryFrom<i32> for ImageFormat {
    type Error = UnknownImageFormat;

    /// Converts the C# integer discriminant into an [`ImageFormat`].
    ///
    /// # Errors
    ///
    /// Returns [`UnknownImageFormat`] when `value` is not a valid discriminant
    /// (mirrors the C# `InvalidEnumArgumentException`).
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Bmp),
            1 => Ok(Self::Gif),
            2 => Ok(Self::Jpg),
            3 => Ok(Self::Png),
            4 => Ok(Self::Webp),
            5 => Ok(Self::Svg),
            other => Err(UnknownImageFormat(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Ported from `Drawing/ImageFormatExtensionsTests.cs`. In C# the enum is
    //! backed by `int`, so out-of-range integers are cast to `ImageFormat` and
    //! `GetMimeType`/`GetExtension` throw `InvalidEnumArgumentException`. Here
    //! the out-of-range path is modeled by [`ImageFormat::try_from`] returning
    //! [`UnknownImageFormat`]; valid formats always yield a mime/extension.
    use super::*;
    use rstest::rstest;

    const ALL_FORMATS: [ImageFormat; 6] = [
        ImageFormat::Bmp,
        ImageFormat::Gif,
        ImageFormat::Jpg,
        ImageFormat::Png,
        ImageFormat::Webp,
        ImageFormat::Svg,
    ];

    #[test]
    fn get_mime_type_valid_valid() {
        for format in ALL_FORMATS {
            assert!(!format.mime_type().is_empty());
        }
    }

    #[test]
    fn get_extension_valid_valid() {
        for format in ALL_FORMATS {
            assert!(!format.extension().is_empty());
        }
    }

    #[rstest]
    #[case(i32::MIN)]
    #[case(i32::MAX)]
    #[case(-1)]
    #[case(6)]
    fn try_from_invalid_returns_error(#[case] value: i32) {
        assert!(ImageFormat::try_from(value).is_err());
    }
}
