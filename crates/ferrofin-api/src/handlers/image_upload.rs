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

/// Extracts the bare image MIME type from a request `Content-Type` header.
///
/// Port of `ImageController.TryGetImageExtensionFromContentType`'s validation
/// half: strips any `;charset=…`/parameters, and returns the media type only when
/// it is a known image type. `None` for a missing, non-image, or unknown type
/// (the C# `BadRequest("Incorrect ContentType.")` outcome).
#[must_use]
pub(crate) fn image_mime_from_content_type(content_type: Option<&str>) -> Option<String> {
    let raw = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    if raw.is_empty() || !mime_types::is_image(&raw) {
        return None;
    }
    Some(raw)
}

/// The filename extension for an image `Content-Type`, e.g. `image/png` → `.png`.
///
/// Port of the extension half of `TryGetImageExtensionFromContentType`
/// (`MimeTypes.ToExtension`). `None` when the type is not a known image type.
#[must_use]
pub(crate) fn image_extension_from_content_type(content_type: Option<&str>) -> Option<String> {
    let mime = image_mime_from_content_type(content_type)?;
    mime_types::to_extension(&mime).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{decode_base64, image_extension_from_content_type, image_mime_from_content_type};

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

    #[test]
    fn image_mime_strips_parameters_and_validates() {
        assert_eq!(
            image_mime_from_content_type(Some("image/png; charset=binary")).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            image_mime_from_content_type(Some("IMAGE/JPEG")).as_deref(),
            Some("image/jpeg")
        );
        assert!(image_mime_from_content_type(Some("application/json")).is_none());
        assert!(image_mime_from_content_type(None).is_none());
    }

    #[test]
    fn extension_maps_image_type() {
        assert_eq!(
            image_extension_from_content_type(Some("image/png")).as_deref(),
            Some(".png")
        );
        assert!(image_extension_from_content_type(Some("text/plain")).is_none());
    }
}
