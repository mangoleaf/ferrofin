//! Transliteration of `Jellyfin.Model.Tests/Drawing/ImageFormatExtensionsTests.cs`.
//!
//! In C#, an arbitrary `int` can be cast to `ImageFormat`, so the extension
//! methods guard against out-of-range values by throwing
//! `InvalidEnumArgumentException`. In Rust an out-of-range value cannot inhabit
//! the enum at all, so the "throws" cases are transliterated against
//! [`ImageFormat::try_from`] (the boundary that produces the enum from an int),
//! while the "valid" cases assert the total mime-type/extension helpers succeed.

use ferrofin_model::drawing::ImageFormat;
use rstest::rstest;

/// The six valid formats, mirroring `Enum.GetValues<ImageFormat>()`.
const ALL_FORMATS: [ImageFormat; 6] = [
    ImageFormat::Bmp,
    ImageFormat::Gif,
    ImageFormat::Jpg,
    ImageFormat::Png,
    ImageFormat::Webp,
    ImageFormat::Svg,
];

/// `GetMimeType_Valid_Valid` — every valid format yields a non-empty mime type.
#[test]
fn get_mime_type_valid_valid() {
    for format in ALL_FORMATS {
        assert!(!format.mime_type().is_empty());
    }
}

/// `GetExtension_Valid_Valid` — every valid format yields a non-empty extension.
#[test]
fn get_extension_valid_valid() {
    for format in ALL_FORMATS {
        assert!(format.extension().starts_with('.'));
    }
}

/// `GetMimeType_Valid_ThrowsInvalidEnumArgumentException` and
/// `GetExtension_Valid_ThrowsInvalidEnumArgumentException` — out-of-range
/// discriminants (`int.MinValue`, `int.MaxValue`, `-1`, `6`) are rejected at
/// the int→enum boundary.
#[rstest]
#[case(i32::MIN)]
#[case(i32::MAX)]
#[case(-1)]
#[case(6)]
fn out_of_range_discriminant_is_rejected(#[case] value: i32) {
    let result = ImageFormat::try_from(value);
    assert!(result.is_err());
}

/// Exact mime-type strings, pinned to the upstream `MediaTypeNames.Image.*`
/// constants (the C# extension switch is the oracle).
#[rstest]
#[case(ImageFormat::Bmp, "image/bmp")]
#[case(ImageFormat::Gif, "image/gif")]
#[case(ImageFormat::Jpg, "image/jpeg")]
#[case(ImageFormat::Png, "image/png")]
#[case(ImageFormat::Webp, "image/webp")]
#[case(ImageFormat::Svg, "image/svg+xml")]
fn mime_type_matches_csharp(#[case] format: ImageFormat, #[case] expected: &str) {
    assert_eq!(format.mime_type(), expected);
}

/// Exact extension strings, pinned to the C# extension switch.
#[rstest]
#[case(ImageFormat::Bmp, ".bmp")]
#[case(ImageFormat::Gif, ".gif")]
#[case(ImageFormat::Jpg, ".jpg")]
#[case(ImageFormat::Png, ".png")]
#[case(ImageFormat::Webp, ".webp")]
#[case(ImageFormat::Svg, ".svg")]
fn extension_matches_csharp(#[case] format: ImageFormat, #[case] expected: &str) {
    assert_eq!(format.extension(), expected);
}
