//! String helpers ported from `MediaBrowser.Common.Extensions.BaseExtensions`.
//!
//! `strip_html` removes HTML tags; `get_md5` hashes a string's UTF-16LE bytes
//! with MD5 and wraps the digest in a `Uuid` using the .NET `Guid(byte[16])`
//! byte layout (first three fields little-endian). Both are untested/behavioral
//! in Jellyfin.

use std::sync::OnceLock;

use regex::Regex;
use uuid::Uuid;

mod md5;

/// The HTML-stripping regex, copied byte-for-byte from `BaseExtensions`.
///
/// Source: `[GeneratedRegex(@"<(.|\n)*?>")]`.
const STRIP_HTML_PATTERN: &str = r"<(.|\n)*?>";

/// Returns the compiled, cached [`STRIP_HTML_PATTERN`] regex.
fn strip_html_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(STRIP_HTML_PATTERN).expect("STRIP_HTML_PATTERN is a valid regex"))
}

/// Strips HTML tags from `html_string` and trims the result.
///
/// Port of `BaseExtensions.StripHtml`.
#[must_use]
pub fn strip_html(html_string: &str) -> String {
    strip_html_regex()
        .replace_all(html_string, "")
        .trim()
        .to_owned()
}

/// Lowercase hex MD5 of raw bytes — the checksum format Jellyfin plugin
/// repository manifests use. Integrity only, NOT authenticity: transport
/// security (HTTPS) is the trust root for downloads.
#[must_use]
pub fn md5_hex(bytes: &[u8]) -> String {
    let digest = md5::compute(bytes);
    let mut out = String::with_capacity(32);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Computes the MD5 of `str`'s UTF-16LE bytes as a [`Uuid`].
///
/// Port of `BaseExtensions.GetMD5`: MD5 over `Encoding.Unicode.GetBytes(str)`
/// (little-endian UTF-16) wrapped in a .NET `Guid`. The 16 digest bytes are
/// laid out the way `new Guid(byte[])` reads them — the first three fields are
/// little-endian, the last eight bytes are in order.
#[must_use]
pub fn get_md5(str: &str) -> Uuid {
    let utf16le: Vec<u8> = str.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let digest = md5::compute(&utf16le);

    // .NET Guid(byte[16]): fields Data1 (u32) and Data2/Data3 (u16) are stored
    // little-endian, so reversing those groups recovers the big-endian order
    // `Uuid::from_bytes` expects.
    let bytes = [
        digest[3], digest[2], digest[1], digest[0], // Data1 (LE -> BE)
        digest[5], digest[4], // Data2 (LE -> BE)
        digest[7], digest[6], // Data3 (LE -> BE)
        digest[8], digest[9], digest[10], digest[11], digest[12], digest[13], digest[14],
        digest[15],
    ];
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod md5_hex_tests {
    #[test]
    fn matches_rfc1321_vectors() {
        assert_eq!(super::md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(super::md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            super::md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
    }
}
