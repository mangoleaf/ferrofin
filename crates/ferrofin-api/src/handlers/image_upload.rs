//! Shared helpers for the base64 image-upload routes.
//!
//! Jellyfin's `ImageController` accepts an uploaded image as the **base64-encoded
//! image bytes** in the request body, with the real MIME type carried in the
//! `Content-Type` header (`[AcceptsImageFile]` + `GetFromBase64Stream` +
//! `TryGetImageExtensionFromContentType`). The item-image `SetItemImage`, the
//! user-profile `PostUserImage`, and the branding `UploadCustomSplashscreen`
//! routes all share that shape, so this module hosts the two shared primitives —
//! decoding the body and validating the image `Content-Type` — rather than each
//! handler re-deriving them.

use ferrofin_model::net::mime_types;

/// Decodes a standard (RFC 4648) base64 string into bytes, returning `None` on
/// any invalid character.
///
/// Port of the `GetFromBase64Stream` decode the image/user/splashscreen upload
/// handlers apply to the request body (the workspace has no base64 dependency).
/// Whitespace is ignored and `=` padding is accepted.
#[must_use]
pub(crate) fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in input.as_bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            // Mask to the low 8 bits — the shift leaves exactly one byte.
            out.push(u8::try_from((acc >> bits) & 0xFF).expect("masked to a byte"));
        }
    }
    Some(out)
}

/// An image upload's validated `Content-Type`: the media type the bytes are
/// saved as, and the file extension that type is stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageContentType {
    /// The bare media type, lowercased and without parameters (`image/png`).
    pub(crate) mime_type: String,
    /// The file extension, with its leading dot (`.png`).
    pub(crate) extension: &'static str,
}

/// Validates an upload's `Content-Type` and resolves its file extension.
///
/// Port of `ImageController.TryGetImageExtensionFromContentType`: strips any
/// `;charset=…`/parameters, and succeeds only for an `image/…` type that
/// `MimeTypes.ToExtension` maps to an extension. `None` for a missing, empty,
/// non-image or unknown image type, which every upload route (`PostUserImage`,
/// `SetItemImage`, `SetItemImageByIndex`, `UploadCustomSplashscreen`) answers
/// with `400 Incorrect ContentType.`.
#[must_use]
pub(crate) fn try_get_image_extension_from_content_type(
    content_type: Option<&str>,
) -> Option<ImageContentType> {
    let mime_type = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    if !mime_types::is_image(&mime_type) {
        return None;
    }
    let extension = mime_types::to_extension(&mime_type)?;
    Some(ImageContentType {
        mime_type,
        extension,
    })
}

#[cfg(test)]
mod tests {
    use super::{ImageContentType, decode_base64, try_get_image_extension_from_content_type};
    use rstest::rstest;

    #[test]
    fn decodes_padded_base64_with_whitespace() {
        // "hi" == aGk=
        assert_eq!(decode_base64("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(decode_base64("aG k=\n").as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn rejects_non_base64() {
        assert!(decode_base64("not*base64").is_none());
    }

    /// Upstream `ImageControllerTests.TryGetImageExtensionFromContentType_Valid_True`,
    /// plus `image/jpg` (Jellyfin 12's `MimeTypes._extensionLookup` entry).
    #[rstest]
    #[case("image/apng", ".apng")]
    #[case("image/avif", ".avif")]
    #[case("image/bmp", ".bmp")]
    #[case("image/gif", ".gif")]
    #[case("image/x-icon", ".ico")]
    #[case("image/jpeg", ".jpg")]
    #[case("image/png", ".png")]
    #[case("image/png; charset=utf-8", ".png")]
    #[case("image/svg+xml", ".svg")]
    #[case("image/tiff", ".tiff")]
    #[case("image/webp", ".webp")]
    #[case("image/jpg", ".jpg")]
    fn try_get_image_extension_from_content_type_valid_true(
        #[case] content_type: &str,
        #[case] extension: &str,
    ) {
        let parsed = try_get_image_extension_from_content_type(Some(content_type))
            .expect("a known image type");
        assert_eq!(parsed.extension, extension);
    }

    /// Upstream `ImageControllerTests.TryGetImageExtensionFromContentType_InValid_False`,
    /// plus image types `ToExtension` knows no extension for.
    #[rstest]
    #[case(None)]
    #[case(Some(""))]
    #[case(Some("text/html"))]
    #[case(Some("image/x-unknown"))]
    #[case(Some("image/"))]
    fn try_get_image_extension_from_content_type_invalid_false(#[case] content_type: Option<&str>) {
        assert_eq!(
            try_get_image_extension_from_content_type(content_type),
            None
        );
    }

    #[test]
    fn image_content_type_is_the_bare_lowercased_media_type() {
        assert_eq!(
            try_get_image_extension_from_content_type(Some(" IMAGE/JPEG ; charset=binary")),
            Some(ImageContentType {
                mime_type: "image/jpeg".to_owned(),
                extension: ".jpg",
            })
        );
    }
}
