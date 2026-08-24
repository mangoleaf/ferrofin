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

/// Smallest response worth compressing, in bytes — one TCP segment.
///
/// Below one MTU the whole response already travels in a single segment, so
/// compressing it cannot save a round trip. All it can do is spend CPU, and on
/// a small body that cost is most of the request:
///
/// | body | identity | brotli | cost |
/// |---|---|---|---|
/// | `/System/Info` (832 B) | 0.061 ms | 0.089 ms | +0.028 ms |
/// | `/Sessions` (<1400 B) | 0.142 ms | 0.177 ms | +0.035 ms |
/// | `/Users/Me` (2054 B) | 0.064 ms | 0.099 ms | +0.035 ms |
///
/// The cost is roughly fixed per response, so it barely registers on a 49 KB
/// page (+6%) and nearly doubles a 200-byte one. **70% of the benchmark's GET
/// endpoints return under 1400 bytes**, which is why adding compression halved
/// the measured median speedup against Jellyfin while the bandwidth win it
/// bought on those rows was exactly zero.
///
/// Raising the floor to one MTU removes that: measured at 1400, `/System/Info`
/// goes +0.028 ms -> 0.000 ms and `/Sessions` +0.035 -> +0.002, while
/// `/Users/Me` at 2054 bytes keeps compressing and keeps its 7x size win.
///
/// This is a deliberate divergence: ASP.NET's `ResponseCompressionMiddleware`
/// has no minimum size and Jellyfin therefore compresses sub-MTU bodies too.
/// It is a cost upstream pays for nothing, not a contract we owe clients — the
/// decoded bytes are identical either way, and `Content-Encoding` is negotiated
/// per response. Revert to `32` (tower_http's default) to match upstream
/// exactly.
const MIN_COMPRESSIBLE_BYTES: u16 = 1400;

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
        // ASP.NET's gzip and brotli providers both default to
        // `CompressionLevel.Fastest`, and `tower_http`'s default is not the
        // same thing — it is the codec's own default, which is far more
        // aggressive. Measured against a live Jellyfin 10.11.8 on the same
        // ~29.7 KB `/Localization/Cultures` body:
        //
        //   leg                 gzip     br
        //   Jellyfin           5,890  5,533
        //   Ferrofin Fastest   5,860  5,321
        //   Ferrofin Default   4,115  3,548
        //
        // Ferrofin was compressing HARDER than upstream — spending materially
        // more CPU to send a smaller body than the server we are matching.
        // `Fastest` is the parity-correct level, not a shortcut.
        .quality(tower_http::CompressionLevel::Fastest)
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
            "application/vnd.apple.mpegurl",
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

    /// A sub-MTU body is passed through, and one just over it is compressed.
    ///
    /// The boundary is the point of the constant: below one TCP segment
    /// compression cannot save a round trip, so it is pure CPU.
    #[test]
    fn a_sub_mtu_body_is_not_compressed_but_one_over_it_is() {
        let p = JellyfinCompressible::new();
        assert!(
            !p.should_compress(&resp("application/json", 832)),
            "832 bytes fits one segment — compressing it buys nothing"
        );
        assert!(
            !p.should_compress(&resp(
                "application/json",
                MIN_COMPRESSIBLE_BYTES as usize - 1
            )),
            "just under the floor must pass through"
        );
        assert!(
            p.should_compress(&resp("application/json", 2054)),
            "a 2 KB body spans segments — it still compresses"
        );
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
