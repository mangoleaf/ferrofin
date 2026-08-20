//! Response compression — port of Jellyfin's `AddResponseCompression()` /
//! `UseResponseCompression()` (`Jellyfin.Server/Startup.cs`).
//!
//! Jellyfin registers ASP.NET Core's response-compression middleware with its
//! defaults, which means, verified against a live Jellyfin 10.11.8:
//!
//! | request `Accept-Encoding` | response |
//! |---|---|
//! | absent | no `Content-Encoding`, no `Vary` |
//! | `br` (or `gzip, br`) | `Content-Encoding: br` |
//! | `gzip` | `Content-Encoding: gzip` |
//! | `deflate` / `zstd` / `identity` | no `Content-Encoding` |
//!
//! and only for the media types in [`COMPRESSIBLE_MIME_TYPES`] — ASP.NET's
//! `ResponseCompressionDefaults.MimeTypes`. Everything else (images, audio,
//! video, HLS playlists, subtitles, `application/octet-stream`) is passed
//! through untouched, which is what keeps the streaming paths off the
//! compressor.
//!
//! The compressed bytes are a transport concern: a client that decodes the body
//! sees exactly the JSON [`serde_json`](https://docs.rs/serde_json) produced,
//! byte for byte.

use axum::body::HttpBody;
use axum::http;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{Predicate, SizeAbove};

/// The media types Jellyfin compresses — ASP.NET Core's
/// `ResponseCompressionDefaults.MimeTypes`, verbatim and in the same order.
///
/// Matching is on the media type only; a `; charset=utf-8` parameter still
/// matches, exactly as ASP.NET's `MediaTypeHeaderValue` comparison does.
pub const COMPRESSIBLE_MIME_TYPES: &[&str] = &[
    // General
    "text/plain",
    // Static files
    "text/css",
    "application/javascript",
    "text/javascript",
    // MVC
    "text/html",
    "application/xml",
    "text/xml",
    "application/json",
    "text/json",
    // WebAssembly
    "application/wasm",
];

/// Smallest response worth compressing, in bytes.
///
/// `tower_http`'s own default. A body below this is dominated by the gzip/brotli
/// framing, so compressing it costs CPU to make the response bigger.
const MIN_COMPRESSIBLE_BYTES: u16 = 32;

/// Splits a `Content-Type` header value into its bare media type, lowercased.
///
/// `application/json; charset=utf-8` → `application/json`.
fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Whether a `Content-Type` names one of [`COMPRESSIBLE_MIME_TYPES`].
#[must_use]
pub fn is_compressible_mime(content_type: &str) -> bool {
    let mt = media_type(content_type);
    COMPRESSIBLE_MIME_TYPES.contains(&mt.as_str())
}

/// [`Predicate`] that compresses only Jellyfin's allow-listed media types.
///
/// `tower_http`'s `DefaultPredicate` is a *deny*-list (it only skips gRPC,
/// images and SSE), which would put `video/*` and `audio/*` responses through
/// the compressor. Jellyfin's middleware is an *allow*-list, so this is too.
#[derive(Clone, Copy, Debug, Default)]
pub struct JellyfinCompressible {
    /// Minimum body size, applied exactly as `tower_http` applies its own.
    size: SizeAbove,
}

impl JellyfinCompressible {
    /// Creates the predicate with Jellyfin's media-type allow-list.
    #[must_use]
    pub fn new() -> Self {
        Self {
            size: SizeAbove::new(MIN_COMPRESSIBLE_BYTES),
        }
    }
}

impl Predicate for JellyfinCompressible {
    fn should_compress<B>(&self, response: &http::Response<B>) -> bool
    where
        B: HttpBody,
    {
        let compressible = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(is_compressible_mime);
        compressible && self.size.should_compress(response)
    }
}

/// The response-compression layer Ferrofin applies, matching Jellyfin's
/// `UseResponseCompression()`.
///
/// `gzip` and `br` only — the two providers ASP.NET Core registers by default —
/// gated on [`JellyfinCompressible`], so only Jellyfin's allow-listed media
/// types are ever encoded.
///
/// The router applies this to the API surface; the composition root applies it
/// to the static `/web` bundle, which is served by a separate service. A
/// response that already carries a `Content-Encoding` is never re-encoded, so
/// the two never compound.
#[must_use]
pub fn compression_layer() -> CompressionLayer<JellyfinCompressible> {
    CompressionLayer::new()
        .gzip(true)
        .br(true)
        .compress_when(JellyfinCompressible::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Response, header};

    fn resp(content_type: &str, len: usize) -> Response<Body> {
        let mut b = Response::builder();
        if !content_type.is_empty() {
            b = b.header(header::CONTENT_TYPE, content_type);
        }
        b.header(header::CONTENT_LENGTH, len.to_string())
            .body(Body::from(vec![b'x'; len]))
            .expect("static response builds")
    }

    #[test]
    fn compresses_json_with_and_without_charset() {
        let p = JellyfinCompressible::new();
        assert!(p.should_compress(&resp("application/json", 4096)));
        assert!(p.should_compress(&resp("application/json; charset=utf-8", 4096)));
        assert!(p.should_compress(&resp("APPLICATION/JSON", 4096)));
    }

    #[test]
    fn compresses_the_other_jellyfin_mime_types() {
        let p = JellyfinCompressible::new();
        for mime in COMPRESSIBLE_MIME_TYPES {
            assert!(
                p.should_compress(&resp(mime, 4096)),
                "{mime} must be compressed — Jellyfin compresses it"
            );
        }
    }

    #[test]
    fn never_compresses_media_or_unknown_types() {
        let p = JellyfinCompressible::new();
        for mime in [
            "video/mp4",
            "audio/mpeg",
            "image/jpeg",
            "image/svg+xml",
            "application/octet-stream",
            "application/x-mpegURL",
            "text/vtt",
            "text/event-stream",
            "font/woff2",
        ] {
            assert!(
                !p.should_compress(&resp(mime, 4096)),
                "{mime} must pass through — Jellyfin does not compress it"
            );
        }
    }

    #[test]
    fn never_compresses_a_response_with_no_content_type() {
        let p = JellyfinCompressible::new();
        assert!(!p.should_compress(&resp("", 4096)));
    }

    #[test]
    fn skips_bodies_below_the_minimum() {
        let p = JellyfinCompressible::new();
        assert!(!p.should_compress(&resp("application/json", 8)));
        assert!(p.should_compress(&resp("application/json", MIN_COMPRESSIBLE_BYTES as usize)));
    }

    #[test]
    fn media_type_strips_parameters_and_case() {
        assert_eq!(
            media_type("Application/JSON; charset=UTF-8"),
            "application/json"
        );
        assert_eq!(media_type("text/html"), "text/html");
        assert_eq!(media_type(""), "");
    }
}
