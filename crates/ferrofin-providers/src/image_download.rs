//! Downloading a remote image URL into bytes plus its *resolved* media type.
//!
//! Every artwork write — the "Choose Image" download
//! (`POST /Items/{itemId}/RemoteImages/Download`), the scan's TMDB/fanart/TVDB
//! poster fetch, a studio thumb — goes through here, so they all resolve the
//! media type the same way and all refuse a non-image.
//!
//! Port of `ProviderManager.SaveImage(BaseItem, string url, …)` (v10.11.8
//! `MediaBrowser.Providers/Manager/ProviderManager.cs:191-220`). The URL's
//! suffix is the LAST resort, not the first: a provider that serves a PNG from
//! an extensionless URL is stored as a PNG, and a URL that answers JSON is
//! refused instead of being written into the library as artwork.

use crate::rate_limit::{LimitedRequest as _, RateLimiter, RequestError};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Image/CDN quotas are independent of metadata API quotas. Share a gate by
/// origin across download callers, never one gate across unrelated hosts.
///
/// `label` is only the provider the caller's downloads are counted under
/// (`ferrofin_metadata_provider_requests_total{provider}`): `image` for
/// artwork, `opensubtitles` for a subtitle file. It never picks the gate:
/// every caller of one origin shares its pacing and cooldowns whatever it is
/// counted as (METRICS.md rule 6).
pub(crate) fn limiter_for(url: &str, label: &'static str) -> RateLimiter {
    static LIMITERS: OnceLock<Mutex<HashMap<String, RateLimiter>>> = OnceLock::new();
    let origin = reqwest::Url::parse(url)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_default();
    LIMITERS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(origin)
        .or_insert_with_key(|origin| RateLimiter::new(origin.clone()))
        .counted_as(label)
}

pub(crate) async fn send(
    http: &reqwest::Client,
    url: &str,
) -> Result<reqwest::Response, RequestError> {
    http.get(url)
        .send_limited(&limiter_for(url, crate::metrics::IMAGE))
        .await
}

use ferrofin_model::net::mime_types;

/// Largest remote image body accepted, in bytes. A 4K PNG backdrop is well
/// under 20 MiB. Without a cap the whole body was buffered (twice) whatever
/// its size.
pub(crate) const MAX_REMOTE_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// Longest a remote image body may take to arrive once the response headers
/// are in. The request itself is bounded by the limiter's per-attempt timeout;
/// this is a hard ceiling on the body read whatever that is configured to.
pub(crate) const REMOTE_IMAGE_BODY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);

/// GETs `url` and returns its bytes plus the media type resolved the way the
/// C# does:
///
/// 1. the response's own `Content-Type` (parameters stripped);
/// 2. `image/png` for a tvheadend `/imagecache/` URL that reported no type —
///    the upstream workaround, ported verbatim;
/// 3. otherwise, when the type is missing or the useless
///    `application/octet-stream`, the type implied by the URL's PATH (query
///    string stripped, as `Uri.GetLeftPart(UriPartial.Path)` does).
///
/// `None` when the request fails, the response is not a success, or the body
/// is over [`MAX_REMOTE_IMAGE_BYTES`] (by `Content-Length`, or once the
/// streamed count passes it, aborting the read) or takes longer than
/// [`REMOTE_IMAGE_BODY_TIMEOUT`] — the caller turns that into its own error.
///
/// A response whose resolved type is not an image comes back with an EMPTY
/// body: its body is never read, as upstream checks the type before copying
/// the stream. The caller still rejects it through [`non_image_reason`].
pub(crate) async fn download_image(http: &reqwest::Client, url: &str) -> Option<(Vec<u8>, String)> {
    download_image_capped(http, url, MAX_REMOTE_IMAGE_BYTES).await
}

/// [`download_image`] with the body cap as a parameter, for tests.
async fn download_image_capped(
    http: &reqwest::Client,
    url: &str,
    cap: u64,
) -> Option<(Vec<u8>, String)> {
    let resp = send(http, url).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let declared = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    let content_type = resolve_content_type(declared.as_deref(), url);
    if non_image_reason(&content_type).is_some() {
        return Some((Vec::new(), content_type));
    }
    let bytes = read_capped(resp, cap).await?;
    Some((bytes, content_type))
}

/// Reads `resp`'s body into one buffer, giving up (and dropping the
/// connection) as soon as it is known to exceed `cap` bytes or the read
/// outlasts [`REMOTE_IMAGE_BODY_TIMEOUT`]. A failed read counts as a request
/// failure, as the rate limiter's `CountedBody` reads do.
async fn read_capped(mut resp: reqwest::Response, cap: u64) -> Option<Vec<u8>> {
    let over = |len: u64| {
        tracing::debug!(cap, len, "remote image is over the size limit");
        crate::rate_limit::note_request_failure();
    };
    if let Some(len) = resp.content_length().filter(|len| *len > cap) {
        over(len);
        return None;
    }
    let initial = resp.content_length().unwrap_or(0).min(cap);
    let read = async move {
        let mut bytes = Vec::with_capacity(usize::try_from(initial).unwrap_or(0));
        while let Some(chunk) = resp.chunk().await.transpose() {
            let chunk = chunk.ok()?;
            let len = (bytes.len() + chunk.len()) as u64;
            if len > cap {
                over(len);
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(bytes)
    };
    let result = tokio::time::timeout(REMOTE_IMAGE_BODY_TIMEOUT, read)
        .await
        .ok()
        .flatten();
    if result.is_none() {
        crate::rate_limit::note_request_failure();
    }
    result
}

/// How long a remote image URL that could not be downloaded as an image is
/// skipped by [`recently_failed`].
const FAILED_URL_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Most failed URLs remembered at once; past it the oldest half is dropped.
const FAILED_URL_CAPACITY: usize = 1024;

fn failed_urls() -> &'static Mutex<HashMap<String, std::time::Instant>> {
    static FAILED: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    FAILED.get_or_init(Mutex::default)
}

/// Whether `url` failed to download as an image within [`FAILED_URL_TTL`].
///
/// A URL that answered a non-image (or failed) was not cached, so every
/// request fetched it again. This negative cache keeps a request from
/// re-triggering that download more than once per TTL.
pub(crate) fn recently_failed(url: &str) -> bool {
    let mut failed = failed_urls()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match failed.get(url) {
        Some(at) if at.elapsed() < FAILED_URL_TTL => true,
        Some(_) => {
            failed.remove(url);
            false
        }
        None => false,
    }
}

/// Records that `url` could not be downloaded as an image (see
/// [`recently_failed`]).
pub(crate) fn note_failed(url: &str) {
    let mut failed = failed_urls()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failed.len() >= FAILED_URL_CAPACITY {
        let mut ages: Vec<std::time::Instant> = failed.values().copied().collect();
        ages.sort_unstable();
        let cutoff = ages[ages.len() / 2];
        failed.retain(|_, at| *at > cutoff);
    }
    failed.insert(url.to_owned(), std::time::Instant::now());
}

/// The media-type resolution above, without the I/O.
pub(crate) fn resolve_content_type(declared: Option<&str>, url: &str) -> String {
    // Workaround for tvheadend channel icons, which report no type at all.
    if declared.is_none() && url.to_ascii_lowercase().contains("/imagecache/") {
        return "image/png".to_owned();
    }
    // Some providers do not report a usable media type; fall back to the one
    // implied by the URL path with its query string stripped.
    if declared.is_none_or(|t| t == "application/octet-stream") {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        return mime_types::get_mime_type(path).to_ascii_lowercase();
    }
    declared.unwrap_or_default().to_owned()
}

/// The C# `HttpRequestException($"Request returned '{contentType}' instead of
/// an image type")` as a message. `None` when `content_type` IS an image.
pub(crate) fn non_image_reason(content_type: &str) -> Option<String> {
    (!mime_types::is_image(content_type))
        .then(|| format!("Request returned '{content_type}' instead of an image type"))
}

#[cfg(test)]
mod tests {
    use super::{non_image_reason, resolve_content_type};
    use rstest::rstest;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Serves one fixed response head, then `body_len` zero bytes in 16 KiB
    /// writes, to every connection; returns the base URL. With `length`, the
    /// head declares `Content-Length: body_len`; without, the body is
    /// delimited by the connection closing, so only a streamed count can
    /// catch an oversized one.
    async fn serve(content_type: &'static str, body_len: usize, length: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let _ = sock.read(&mut buf).await;
                    let length = if length {
                        format!("Content-Length: {body_len}\r\n")
                    } else {
                        String::new()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{length}Connection: close\r\n\r\n"
                    );
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let chunk = vec![0u8; 16 * 1024];
                    let mut left = body_len;
                    while left > 0 {
                        let n = left.min(chunk.len());
                        if sock.write_all(&chunk[..n]).await.is_err() {
                            return;
                        }
                        left -= n;
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// An image body over the cap is refused, whether its `Content-Length`
    /// says so up front or only the streamed count finds out; one under it
    /// downloads whole.
    #[rstest]
    #[case::declared_over(true, 4096, None)]
    #[case::streamed_over(false, 4096, None)]
    #[case::declared_under(true, 512, Some(512))]
    #[case::streamed_under(false, 512, Some(512))]
    #[tokio::test]
    async fn image_bodies_are_capped(
        #[case] length: bool,
        #[case] body_len: usize,
        #[case] expected: Option<usize>,
    ) {
        let base = serve("image/png", body_len, length).await;
        let http = reqwest::Client::new();
        let got = super::download_image_capped(&http, &format!("{base}/a.png"), 1024).await;
        assert_eq!(got.map(|(bytes, _)| bytes.len()), expected);
    }

    /// A non-image response is answered with its type and no body: the body,
    /// however large, is never read.
    #[tokio::test]
    async fn non_image_bodies_are_not_read() {
        let base = serve("application/json", 8 * 1024 * 1024, true).await;
        let http = reqwest::Client::new();
        let (bytes, mime) = super::download_image_capped(&http, &format!("{base}/x"), 1024)
            .await
            .expect("response");
        assert!(bytes.is_empty());
        assert!(non_image_reason(&mime).is_some());
    }

    /// A failed URL is skipped until its entry ages out, and only that URL.
    #[test]
    fn failed_urls_are_remembered() {
        let url = "https://neg-cache.example/logo.png";
        assert!(!super::recently_failed(url));
        super::note_failed(url);
        assert!(super::recently_failed(url));
        assert!(!super::recently_failed(
            "https://neg-cache.example/other.png"
        ));
    }

    /// The label never picks the gate: an artwork download and a subtitle
    /// download from one origin share its pacing and cooldowns, each counted
    /// under its own provider; another origin has a gate of its own.
    #[test]
    fn two_labels_on_one_origin_share_one_gate() {
        let artwork = super::limiter_for("https://cdn.example/poster.jpg", crate::metrics::IMAGE);
        let subtitle = super::limiter_for("https://cdn.example/file.srt", "opensubtitles");
        let other = super::limiter_for("https://elsewhere.example/poster.jpg", "image");
        assert!(artwork.shares_gate_with(&subtitle));
        assert!(!artwork.shares_gate_with(&other));
        assert_eq!(artwork.metric_label(), "image");
        assert_eq!(subtitle.metric_label(), "opensubtitles");
    }

    #[rstest]
    // The response's own type wins, parameters stripped by the caller.
    #[case(Some("image/png"), "https://x/poster.jpg", "image/png")]
    // No type + an `/imagecache/` URL: the ported tvheadend workaround.
    #[case(None, "http://tvh/imagecache/42", "image/png")]
    // No type elsewhere: the URL PATH decides, query string stripped.
    #[case(None, "https://x/poster.png?size=big", "image/png")]
    #[case(None, "https://x/poster.jpg?v=2", "image/jpeg")]
    // `application/octet-stream` is treated as "not reported".
    #[case(Some("application/octet-stream"), "https://x/a.webp", "image/webp")]
    // An extensionless URL with no usable type falls to the default, which is
    // NOT an image — so the caller refuses it rather than guessing jpeg.
    #[case(None, "https://x/image", "application/octet-stream")]
    fn content_type_resolution(
        #[case] declared: Option<&str>,
        #[case] url: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(resolve_content_type(declared, url), expected);
    }

    #[test]
    fn only_image_types_are_accepted() {
        assert!(non_image_reason("image/jpeg").is_none());
        assert!(non_image_reason("IMAGE/PNG").is_none());
        assert_eq!(
            non_image_reason("application/json").as_deref(),
            Some("Request returned 'application/json' instead of an image type")
        );
        // The bug this guards: a URL that answers JSON used to be stored as the
        // item's artwork and served back as `image/jpeg`.
        assert!(non_image_reason("text/html").is_some());
    }
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use crate::mock_http::MockServer;

    #[tokio::test]
    async fn downloads_share_origin_cooldowns_but_not_other_hosts() {
        let blocked = MockServer::with_headers(
            vec![("/", "image".to_owned())],
            "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-In: 120\r\n",
        )
        .await;
        let other = MockServer::always("image").await;
        let http = reqwest::Client::new();
        assert!(send(&http, &blocked.base_url).await.is_ok());
        assert!(matches!(
            send(&http, &format!("{}/another-cover", blocked.base_url)).await,
            Err(RequestError::Cooldown)
        ));
        assert!(send(&http, &other.base_url).await.is_ok());
    }
}

/// Recognizes JPEG, PNG, GIF and WebP magic before persisting plugin artwork.
/// This identifies the file format; image processing validates its dimensions.
#[must_use]
pub fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Successful automatic image download, with enough information to avoid
/// duplicates and preserve the response's actual image format.
#[derive(Debug)]
pub struct ArtworkDownload {
    /// Complete response body.
    pub bytes: Vec<u8>,
    /// Resolved image media type.
    pub content_type: String,
    /// Reported body length, for Jellyfin's backdrop duplicate check.
    pub content_length: Option<u64>,
}

/// How automatic acquisition handles a failed candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkDownloadFailure {
    /// A stale/forbidden URL: try the next candidate from this provider.
    Skip,
    /// Transport or another HTTP error: stop this provider's image-type pass.
    Stop,
}

pub(crate) async fn download_artwork(
    http: &reqwest::Client,
    url: &str,
) -> Result<ArtworkDownload, ArtworkDownloadFailure> {
    let response = send(http, url)
        .await
        .map_err(|_| ArtworkDownloadFailure::Stop)?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN => {
            return Err(ArtworkDownloadFailure::Skip);
        }
        status if !status.is_success() => return Err(ArtworkDownloadFailure::Stop),
        _ => {}
    }
    let declared = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or(value)
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|value| !value.is_empty());
    let content_type = resolve_content_type(declared.as_deref(), url);
    if let Some(reason) = non_image_reason(&content_type) {
        crate::rate_limit::note_request_failure();
        tracing::warn!(%reason, "artwork provider returned a non-image response");
        return Err(ArtworkDownloadFailure::Stop);
    }
    let content_length = response.content_length();
    // The same cap and body deadline as `download_image`: a provider (or a
    // CDN it points at) answering a huge body must not be buffered whole.
    let bytes = read_capped(response, MAX_REMOTE_IMAGE_BYTES)
        .await
        .ok_or(ArtworkDownloadFailure::Stop)?;
    Ok(ArtworkDownload {
        bytes,
        content_type,
        content_length,
    })
}
