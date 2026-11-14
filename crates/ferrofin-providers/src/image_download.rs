//! Downloading a remote image URL into bytes plus its *resolved* media type.
//!
//! Every artwork write — the "Choose Image" download
//! (`POST /Items/{itemId}/RemoteImages/Download`), the scan's TMDB/fanart/TVDB
//! poster fetch, a studio thumb — goes through here, so they all resolve the
//! media type the same way and all refuse a non-image.
//!
//! Port of `ProviderManager.SaveImage(BaseItem, string url, …)` (v12.4
//! `MediaBrowser.Providers/Manager/ProviderManager.cs:191-258`). The URL's
//! suffix is the LAST resort, not the first: a provider that serves a PNG from
//! an extensionless URL is stored as a PNG, and a URL that answers JSON is
//! refused instead of being written into the library as artwork.
//!
//! Two things upstream does not do are added (owner decision 2026-10-31,
//! gl-1095). A body is read under the `image_download_max_mb` limit
//! ([`RequestLimits`](crate::rate_limit::RequestLimits)), so that a source
//! cannot decide how much memory a download takes, and the first bytes must be
//! an image whatever the response declared, because the first file stored for
//! a URL or an item stays there.
//!
//! A third (owner decision 2026-10-31, gl-1096): a download never reaches a
//! private address, on its first request or on any redirect, unless the
//! server's [`ImageDestinations`](crate::destination::ImageDestinations) let
//! it, and the URL's suffix names the type only when no redirect happened.
//! Upstream's client follows redirects anywhere, so a playlist's logo URL
//! could read an internal service through the server. The follow-and-vet loop
//! is [`GuardedHttp`], which subtitle links go through too (gl-1160).

use crate::destination::ConfiguredImageDestinations;
use crate::guarded_http::{FetchKind, Fetched, GuardedHttp, SendError};
use crate::rate_limit::{BodyError, CappedBody, CountedBody as _, request_limits};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

/// Image downloads, for their limiter and log lines.
pub(crate) static IMAGE_FETCH: FetchKind = FetchKind::new(
    crate::metrics::IMAGE,
    "remote image",
    "image downloads",
    "image_private_http_allow (FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW) lists it or it is a Live TV \
     tuner's host",
    None,
);

/// The process's image-download clients: the server's
/// [`set_image_destinations`](crate::destination::set_image_destinations), read at
/// each check, and the environment's proxy.
fn image_http() -> &'static GuardedHttp {
    static SHARED: OnceLock<GuardedHttp> = OnceLock::new();
    SHARED.get_or_init(|| {
        GuardedHttp::with_environment_proxy(Arc::new(ConfiguredImageDestinations), &IMAGE_FETCH)
    })
}

/// GETs `url` for an image download: [`GuardedHttp::send`] on the process's
/// clients.
pub(crate) async fn send(url: &str) -> Result<Fetched, SendError> {
    image_http().send(url).await
}

use ferrofin_model::net::mime_types;

/// How many of a body's first bytes [`is_image`] is given: the magic numbers
/// fit in a few, and this leaves room for what an SVG writes before `<svg`.
const SNIFF_BYTES: usize = 4096;

/// Why a URL gave no image.
#[derive(Debug)]
pub(crate) enum ImageFetchError {
    /// `404` or `403`: upstream's "this URL has no image, try the next".
    Missing,
    /// The response's type, or its first bytes, are not an image. Carries the
    /// message for the caller.
    NotAnImage(String),
    /// The body is over the download limit.
    TooLarge,
    /// The URL, or a redirect from it, is at a private address the server's
    /// [`ImageDestinations`](crate::destination::ImageDestinations) do not
    /// let downloads reach.
    Refused,
    /// No answer to go on: a transport failure, a timeout, a provider
    /// cooldown, or a status that says nothing about the image.
    Unavailable,
    /// The image could not be written here.
    Storage(std::io::Error),
}

impl ImageFetchError {
    /// Whether the source answered and asking again would get the same
    /// answer. Anything else may be gone in a moment, so a good image must not
    /// be hidden because of it. A refusal is the same until the server
    /// restarts with another allow-list, or a tuner on that host is added.
    pub(crate) fn is_answer(&self) -> bool {
        matches!(
            self,
            Self::Missing | Self::NotAnImage(_) | Self::TooLarge | Self::Refused
        )
    }
}

impl From<SendError> for ImageFetchError {
    fn from(error: SendError) -> Self {
        match error {
            SendError::Refused => Self::Refused,
            SendError::Request(_) | SendError::Failed(_) => Self::Unavailable,
        }
    }
}

impl From<BodyError> for ImageFetchError {
    fn from(error: BodyError) -> Self {
        match error {
            // A body read as bytes is never parsed as JSON.
            BodyError::Read(_) | BodyError::Decode(_) => Self::Unavailable,
            BodyError::TooLarge => Self::TooLarge,
        }
    }
}

/// A response that says it is an image, and whose first bytes are one, with
/// the rest of its body not read yet.
pub(crate) struct RemoteImage {
    body: CappedBody,
    /// The bytes read so far, which the rest follows.
    head: Vec<u8>,
    /// The media type resolved by [`resolve_content_type`].
    content_type: String,
    /// The length the response declared.
    content_length: Option<u64>,
}

impl RemoteImage {
    /// The media type: the response's own, then the C# workarounds
    /// ([`resolve_content_type`]). Bytes never change it: upstream types the
    /// file from the response, and a tvheadend icon reports nothing at all.
    pub(crate) fn content_type(&self) -> &str {
        &self.content_type
    }

    /// The whole body in memory.
    async fn into_bytes(self) -> Result<Vec<u8>, ImageFetchError> {
        Ok(self.body.read_to_end(self.head).await?)
    }

    /// Writes the whole body to `file`, holding one chunk at a time, however
    /// large the image and however many are on their way.
    pub(crate) async fn write_to(
        mut self,
        file: &mut tokio::fs::File,
    ) -> Result<(), ImageFetchError> {
        let mut buffer = self.head;
        loop {
            file.write_all(&buffer)
                .await
                .map_err(ImageFetchError::Storage)?;
            buffer.clear();
            if !self.body.read_into(&mut buffer).await? {
                break;
            }
        }
        file.flush().await.map_err(ImageFetchError::Storage)
    }
}

/// GETs `url` and checks everything that can be checked before the body:
/// the status, the media type resolved the way the C# does
/// ([`resolve_content_type`]), the declared length, and then the first bytes.
pub(crate) async fn open_image(url: &str) -> Result<RemoteImage, ImageFetchError> {
    open_image_capped(image_http(), url, request_limits().download_max_bytes).await
}

async fn open_image_capped(
    http: &GuardedHttp,
    url: &str,
    limit: u64,
) -> Result<RemoteImage, ImageFetchError> {
    let Fetched {
        response,
        redirected,
    } = http.send(url).await?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN => {
            return Err(ImageFetchError::Missing);
        }
        status if !status.is_success() => return Err(ImageFetchError::Unavailable),
        _ => {}
    }
    let declared = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    let content_type = resolve_content_type(declared.as_deref(), url, redirected);
    if let Some(reason) = non_image_reason(&content_type) {
        return Err(ImageFetchError::NotAnImage(reason));
    }
    let content_length = response.content_length();
    let mut body = CappedBody::new(response, limit)?;
    let mut head = Vec::new();
    while head.len() < SNIFF_BYTES && body.read_into(&mut head).await? {}
    if !is_image(&head) {
        return Err(ImageFetchError::NotAnImage(format!(
            "Request returned '{content_type}' but its content is not an image"
        )));
    }
    Ok(RemoteImage {
        body,
        head,
        content_type,
        content_length,
    })
}

/// GETs `url` and returns its bytes plus their media type.
pub(crate) async fn download_image(url: &str) -> Result<(Vec<u8>, String), ImageFetchError> {
    let image = open_image(url).await?;
    let content_type = image.content_type.clone();
    Ok((image.into_bytes().await?, content_type))
}

/// GETs `url` for a caller that names the file itself, so it needs no media
/// type and only the bytes have to be an image. `None` on any failure.
pub(crate) async fn download_bytes(url: &str) -> Option<Vec<u8>> {
    fetch_image_bytes(url).await.ok()
}

/// [`download_bytes`] with the reason it found no image: `404`/`403` is
/// [`Missing`](ImageFetchError::Missing), another status that is not a
/// success [`Unavailable`](ImageFetchError::Unavailable), and a body whose
/// first bytes are no image [`NotAnImage`](ImageFetchError::NotAnImage).
pub(crate) async fn fetch_image_bytes(url: &str) -> Result<Vec<u8>, ImageFetchError> {
    let response = send(url).await?.response;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN => {
            return Err(ImageFetchError::Missing);
        }
        status if !status.is_success() => return Err(ImageFetchError::Unavailable),
        _ => {}
    }
    let bytes = response.counted_bytes().await?;
    if !is_image(&bytes) {
        return Err(ImageFetchError::NotAnImage(
            "the response's content is not an image".to_owned(),
        ));
    }
    Ok(bytes)
}

/// The media-type resolution above, without the I/O. `redirected` is
/// whether a redirect led from `url` to the response.
pub(crate) fn resolve_content_type(declared: Option<&str>, url: &str, redirected: bool) -> String {
    // Workaround for tvheadend channel icons, which report no type at all.
    // It names PNG whatever redirect led to the answer, as upstream's does:
    // it is the URL's shape, not its suffix, and the bytes must be an image
    // anyway (`is_image`), so a tvheadend behind an http-to-https redirect
    // keeps its logos.
    if declared.is_none() && url.to_ascii_lowercase().contains("/imagecache/") {
        return "image/png".to_owned();
    }
    // Some providers do not report a usable media type; fall back to the one
    // implied by the URL path with its query string stripped. Only when the
    // URL itself answered (gl-1096): a redirect may lead anywhere, so the name
    // of the URL asked for says nothing about what answered.
    if declared.is_none_or(|t| t == "application/octet-stream") {
        if redirected {
            return declared.unwrap_or("application/octet-stream").to_owned();
        }
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

    #[rstest]
    // The response's own type wins, parameters stripped by the caller.
    #[case(Some("image/png"), "https://x/poster.jpg", false, "image/png")]
    #[case(Some("image/png"), "https://x/poster.jpg", true, "image/png")]
    // No type + an `/imagecache/` URL: the ported tvheadend workaround.
    #[case(None, "http://tvh/imagecache/42", false, "image/png")]
    #[case(None, "http://tvh/imagecache/42", true, "image/png")]
    // No type elsewhere: the URL PATH decides, query string stripped.
    #[case(None, "https://x/poster.png?size=big", false, "image/png")]
    #[case(None, "https://x/poster.jpg?v=2", false, "image/jpeg")]
    // `application/octet-stream` is treated as "not reported".
    #[case(
        Some("application/octet-stream"),
        "https://x/a.webp",
        false,
        "image/webp"
    )]
    // An extensionless URL with no usable type falls to the default, which is
    // NOT an image — so the caller refuses it rather than guessing jpeg.
    #[case(None, "https://x/image", false, "application/octet-stream")]
    // After a redirect the URL asked for names nothing (gl-1096): whatever
    // answered was not it.
    #[case(None, "https://x/poster.png", true, "application/octet-stream")]
    #[case(
        Some("application/octet-stream"),
        "https://x/a.webp",
        true,
        "application/octet-stream"
    )]
    fn content_type_resolution(
        #[case] declared: Option<&str>,
        #[case] url: &str,
        #[case] redirected: bool,
        #[case] expected: &str,
    ) {
        assert_eq!(resolve_content_type(declared, url, redirected), expected);
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
mod body_tests {
    use super::*;
    use crate::mock_http::MockServer;
    use rstest::rstest;

    const PNG_TYPE: &str = "Content-Type: image/png\r\n";
    const LIMIT: usize = 16 * 1024;

    /// `length` bytes that start like a PNG.
    fn png_of(length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes
    }

    fn chunked(body: &[u8]) -> Vec<u8> {
        let mut encoded = format!("{:x}\r\n", body.len()).into_bytes();
        encoded.extend_from_slice(body);
        encoded.extend_from_slice(b"\r\n0\r\n\r\n");
        encoded
    }

    async fn fetch(response: Vec<u8>, path: &str) -> Result<Vec<u8>, ImageFetchError> {
        let server = MockServer::start_raw(vec![response]).await;
        let url = format!("{}{path}", server.base_url);
        open_image_capped(&GuardedHttp::open(&IMAGE_FETCH), &url, LIMIT as u64)
            .await?
            .into_bytes()
            .await
    }

    /// gl-76: a download names the server and offers compression, as
    /// upstream's `NamedClient.Default` does.
    #[tokio::test]
    async fn a_download_names_the_server() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok(PNG_TYPE, &png_of(8))]).await;
        let url = format!("{}/a.png", server.base_url);

        open_image_capped(&GuardedHttp::open(&IMAGE_FETCH), &url, LIMIT as u64)
            .await
            .expect("an image");

        let head = &server.heads()[0];
        let agent = format!("user-agent: {}\r\n", ferrofin_health::USER_AGENT);
        assert!(head.contains(&agent.to_ascii_lowercase()), "{head}");
        assert!(head.contains("accept-encoding: "), "{head}");
    }

    /// A body past the limit stops there whether or not it says so up front.
    /// The declared case has no body at all: it was never read.
    #[rstest]
    #[case::declared(MockServer::raw_ok(
        &format!("{PNG_TYPE}Content-Length: {}\r\n", LIMIT + 1),
        b"",
    ))]
    #[case::undeclared(MockServer::raw_ok(PNG_TYPE, &png_of(LIMIT + 1)))]
    #[case::chunked(MockServer::raw_ok(
        &format!("{PNG_TYPE}Transfer-Encoding: chunked\r\n"),
        &chunked(&png_of(LIMIT + 1)),
    ))]
    #[tokio::test]
    async fn a_body_past_the_limit_is_refused(#[case] response: Vec<u8>) {
        let err = fetch(response, "/a.png").await.expect_err("over the limit");
        assert!(matches!(err, ImageFetchError::TooLarge), "{err:?}");
    }

    /// The limit is the largest body kept, not the first one refused.
    #[rstest]
    #[case::declared(MockServer::raw_ok(
        &format!("{PNG_TYPE}Content-Length: {LIMIT}\r\n"),
        &png_of(LIMIT),
    ))]
    #[case::undeclared(MockServer::raw_ok(PNG_TYPE, &png_of(LIMIT)))]
    #[tokio::test]
    async fn a_body_at_the_limit_is_kept_whole(#[case] response: Vec<u8>) {
        let bytes = fetch(response, "/a.png").await.expect("at the limit");
        assert_eq!(bytes, png_of(LIMIT));
    }

    /// The type is settled before the length and before the body: this one
    /// declares a body over the limit and sends none.
    #[tokio::test]
    async fn a_non_image_type_is_refused_unread() {
        let headers = format!(
            "Content-Type: text/html\r\nContent-Length: {}\r\n",
            LIMIT + 1
        );
        let err = fetch(MockServer::raw_ok(&headers, b""), "/a.png")
            .await
            .expect_err("not an image");
        assert!(
            matches!(&err, ImageFetchError::NotAnImage(reason)
                if reason == "Request returned 'text/html' instead of an image type"),
            "{err:?}"
        );
    }

    /// Declaring `image/*` is not enough, and a body too short to hold the
    /// magic is no image either.
    #[rstest]
    #[case::a_page(b"<!doctype html><html></html>".as_slice())]
    #[case::json(b"{\"error\":\"nope\"}".as_slice())]
    #[case::nothing(b"".as_slice())]
    #[tokio::test]
    async fn content_that_is_not_an_image_is_refused_whatever_it_declares(#[case] body: &[u8]) {
        let err = fetch(MockServer::raw_ok(PNG_TYPE, body), "/a.png")
            .await
            .expect_err("not an image");
        assert!(matches!(err, ImageFetchError::NotAnImage(_)), "{err:?}");
    }

    /// The tvheadend icons report no type, so the `/imagecache/` workaround
    /// names one, and the bytes need only be an image: any image.
    #[rstest]
    #[case::jpeg(b"\xFF\xD8\xFF\xE0rest of a jpeg".as_slice())]
    #[case::svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_slice())]
    #[tokio::test]
    async fn a_tvheadend_icon_with_no_type_is_kept(#[case] body: &[u8]) {
        let server = MockServer::start_raw(vec![MockServer::raw_ok("", body)]).await;
        let url = format!("{}/imagecache/42", server.base_url);
        let (bytes, content_type) = download_image(&url).await.expect("an image");
        assert_eq!(bytes, body);
        assert_eq!(content_type, "image/png");
    }

    #[tokio::test]
    async fn an_svg_is_kept_with_its_own_type() {
        let body = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        let server = MockServer::start_raw(vec![MockServer::raw_ok(
            "Content-Type: image/svg+xml; charset=utf-8\r\n",
            body,
        )])
        .await;
        let (bytes, content_type) = download_image(&server.base_url).await.expect("an image");
        assert_eq!(bytes, body);
        assert_eq!(content_type, "image/svg+xml");
    }

    /// Whether the next attempt could differ decides what the negative cache
    /// may keep.
    #[rstest]
    #[case::not_found("404 Not Found", true)]
    #[case::forbidden("403 Forbidden", true)]
    #[case::other_status("401 Unauthorized", false)]
    #[tokio::test]
    async fn a_status_is_an_answer_only_when_it_is_about_the_image(
        #[case] status: &str,
        #[case] answer: bool,
    ) {
        let err = fetch(MockServer::raw_status(status), "/a.png")
            .await
            .expect_err("no image");
        assert_eq!(err.is_answer(), answer, "{err:?}");
    }

    /// A body cut off is a failure of the connection, not an answer.
    #[tokio::test]
    async fn a_body_cut_off_is_not_an_answer() {
        let headers = format!("{PNG_TYPE}Content-Length: 4096\r\n");
        let err = fetch(MockServer::raw_ok(&headers, &png_of(100)), "/a.png")
            .await
            .expect_err("cut off");
        assert!(matches!(err, ImageFetchError::Unavailable), "{err:?}");
        assert!(!err.is_answer());
    }

    /// Larger than the window the content is checked in, so the file is
    /// what was read for that check and what came after it.
    #[tokio::test]
    async fn an_image_is_written_whole() {
        let mut body: Vec<u8> = (0..=u8::MAX).cycle().take(SNIFF_BYTES * 8).collect();
        body[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let server = MockServer::start_raw(vec![MockServer::raw_ok(PNG_TYPE, &body)]).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.png");
        let mut file = tokio::fs::File::create(&path).await.expect("file");
        open_image(&server.base_url)
            .await
            .expect("an image")
            .write_to(&mut file)
            .await
            .expect("written");
        assert_eq!(std::fs::read(&path).expect("file"), body);
    }

    /// Large fanart comes through the scan's path under the default limit.
    #[tokio::test]
    async fn large_artwork_fits_under_the_default_limit() {
        let body = png_of(40 * 1024 * 1024);
        let headers = format!("{PNG_TYPE}Content-Length: {}\r\n", body.len());
        let server = MockServer::start_raw(vec![MockServer::raw_ok(&headers, &body)]).await;
        let download = download_artwork(&server.base_url).await.expect("artwork");
        assert_eq!(download.bytes.len(), body.len());
        assert_eq!(download.content_length, Some(body.len() as u64));
        assert_eq!(download.content_type, "image/png");
    }

    /// Over the limit moves on to the provider's next candidate and is not a
    /// failure of the refresh; content that is no image is, as a non-image
    /// type is.
    #[tokio::test]
    async fn the_scan_skips_a_candidate_over_the_limit() {
        let declared = crate::rate_limit::request_limits().download_max_bytes + 1;
        let headers = format!("{PNG_TYPE}Content-Length: {declared}\r\n");
        let server = MockServer::start_raw(vec![MockServer::raw_ok(&headers, b"")]).await;
        let (outcome, failures) =
            crate::rate_limit::count_request_failures(download_artwork(&server.base_url)).await;
        assert!(matches!(outcome, Err(ArtworkDownloadFailure::Skip)));
        assert_eq!(failures, 0);

        let server =
            MockServer::start_raw(vec![MockServer::raw_ok(PNG_TYPE, b"<html></html>")]).await;
        let (outcome, failures) =
            crate::rate_limit::count_request_failures(download_artwork(&server.base_url)).await;
        assert!(matches!(outcome, Err(ArtworkDownloadFailure::Stop)));
        assert_eq!(failures, 1);
    }

    /// A scan's direct download keeps an image and drops anything else, with
    /// no failure counted, so a studio's thumb is not asked for forever.
    #[tokio::test]
    async fn a_direct_download_keeps_only_an_image() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok("", &png_of(16))]).await;
        assert_eq!(download_bytes(&server.base_url).await, Some(png_of(16)));

        let server = MockServer::start_raw(vec![MockServer::raw_ok("", b"<html></html>")]).await;
        let (bytes, failures) =
            crate::rate_limit::count_request_failures(download_bytes(&server.base_url)).await;
        assert_eq!(bytes, None);
        assert_eq!(failures, 0);
    }

    #[rstest]
    #[case::jpeg(b"\xFF\xD8\xFF\xE0".as_slice())]
    #[case::png(b"\x89PNG\r\n\x1a\n".as_slice())]
    #[case::gif(b"GIF89a".as_slice())]
    #[case::webp(b"RIFF\0\0\0\0WEBPVP8 ".as_slice())]
    #[case::bmp(b"BM\x36\0\0\0\0\0\0\0\x36\0\0\0\x28\0\0\0".as_slice())]
    #[case::tiff_little_endian(b"II*\0".as_slice())]
    #[case::tiff_big_endian(b"MM\0*".as_slice())]
    #[case::ico(b"\0\0\x01\0\x01\0".as_slice())]
    #[case::cur(b"\0\0\x02\0\x01\0".as_slice())]
    #[case::avif(b"\0\0\0\x20ftypavif\0\0\0\0".as_slice())]
    #[case::heic(b"\0\0\0\x18ftypheic\0\0\0\0".as_slice())]
    #[case::jpeg_xl(b"\xFF\x0A\0".as_slice())]
    #[case::jpeg_xl_container(b"\0\0\0\x0CJXL \r\n\x87\n".as_slice())]
    #[case::jpeg_2000(b"\xFF\x4F\xFF\x51".as_slice())]
    #[case::jpeg_2000_container(b"\0\0\0\x0CjP  \r\n\x87\n".as_slice())]
    #[case::svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".as_slice())]
    #[case::svg_without_attributes(b"<svg>".as_slice())]
    #[case::svg_self_closed(b"<svg/>".as_slice())]
    #[case::svg_after_a_byte_order_mark(b"\xEF\xBB\xBF\n  <svg width=\"1\"/>".as_slice())]
    #[case::svg_after_a_declaration(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg/>".as_slice())]
    #[case::svg_after_a_comment(b"<?xml version=\"1.0\"?><!-- Generator: x -> y --><svg/>".as_slice())]
    #[case::svg_after_a_doctype(
        b"<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"x.dtd\">\n<svg/>".as_slice()
    )]
    #[case::svg_after_an_internal_subset(
        b"<!DOCTYPE svg [ <!ENTITY ns \"http://x\"> ]>\n<svg/>".as_slice()
    )]
    fn images_are_recognized(#[case] head: &[u8]) {
        assert!(is_image(head), "{}", String::from_utf8_lossy(head));
    }

    #[rstest]
    #[case::nothing(b"".as_slice())]
    #[case::text(b"not an image".as_slice())]
    #[case::json(b"{\"a\":1}".as_slice())]
    #[case::a_page(b"<!DOCTYPE html><html><body></body></html>".as_slice())]
    #[case::a_page_with_an_svg_in_it(b"<html><body><svg></svg></body></html>".as_slice())]
    #[case::a_page_after_a_doctype_with_an_svg(b"<!doctype html><svg></svg>".as_slice())]
    #[case::another_tag_starting_alike(b"<svgfoo/>".as_slice())]
    #[case::xml(b"<?xml version=\"1.0\"?><rss></rss>".as_slice())]
    #[case::a_declaration_never_closed(b"<?xml version=\"1.0\"".as_slice())]
    #[case::a_comment_never_closed(b"<!-- <svg/>".as_slice())]
    #[case::a_movie(b"\0\0\0\x20ftypisom\0\0\0\0".as_slice())]
    #[case::text_starting_like_a_bmp(b"BMW logos, a list of them".as_slice())]
    fn other_content_is_not_an_image(#[case] head: &[u8]) {
        assert!(!is_image(head), "{}", String::from_utf8_lossy(head));
    }
}

#[cfg(test)]
mod negative_cache_tests {
    use super::*;

    #[test]
    fn a_url_is_remembered_until_its_time_is_up() {
        let (url, other) = (Uuid::new_v4(), Uuid::new_v4());
        let cache = NegativeImageCache::new(Duration::from_hours(1));
        cache.insert(url);
        assert!(cache.contains(url));
        assert!(!cache.contains(other));

        let cache = NegativeImageCache::new(Duration::ZERO);
        cache.insert(url);
        assert!(!cache.contains(url), "expired");
    }

    /// The cache never outgrows its capacity, whatever is asked of it.
    #[test]
    fn the_entry_closest_to_expiring_makes_room() {
        let cache = NegativeImageCache::with_capacity(Duration::from_hours(1), 3);
        let urls: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for url in &urls {
            cache.insert(*url);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(cache.until.lock().expect("lock").len(), 3);
        assert!(!cache.contains(urls[0]));
        assert!(!cache.contains(urls[1]));
        assert!(cache.contains(urls[2]));
        assert!(cache.contains(urls[4]));

        // A URL it already holds takes no one else's place.
        cache.insert(urls[2]);
        assert!(cache.contains(urls[3]));
    }

    #[test]
    fn an_entry_knows_when_it_went_in() {
        let url = Uuid::new_v4();
        for ttl in [Duration::ZERO, Duration::from_hours(1)] {
            let cache = NegativeImageCache::new(ttl);
            let before = Instant::now();
            assert!(!cache.inserted_since(url, before), "nothing in it yet");
            std::thread::sleep(Duration::from_millis(1));
            cache.insert(url);
            assert!(cache.inserted_since(url, before));
            std::thread::sleep(Duration::from_millis(1));
            assert!(!cache.inserted_since(url, Instant::now()));
        }
    }

    #[test]
    fn expired_entries_make_room_before_a_live_one_goes() {
        let cache = NegativeImageCache::with_capacity(Duration::from_hours(1), 2);
        let (expired, live, new) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        cache.insert(live);
        cache
            .until
            .lock()
            .expect("lock")
            .insert(expired, Instant::now());
        cache.insert(new);
        assert!(cache.contains(live));
        assert!(cache.contains(new));
        assert!(!cache.until.lock().expect("lock").contains_key(&expired));
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
        let http = GuardedHttp::open(&IMAGE_FETCH);
        assert!(http.send(&blocked.base_url).await.is_ok());
        assert!(matches!(
            http.send(&format!("{}/another-cover?probe=1", blocked.base_url))
                .await,
            Err(SendError::Request(_))
        ));
        assert!(
            blocked.query_values("/another-cover", "probe").is_empty(),
            "the cooldown kept it unsent"
        );
        assert!(http.send(&other.base_url).await.is_ok());
    }
}

/// gl-1096: a download refuses a private destination on its first request and
/// on every redirect hop, unless the policy lets it through.
#[cfg(test)]
mod destination_tests {
    use super::*;
    use crate::destination::{self, DestinationPolicy};
    use crate::guarded_http::MAX_REDIRECTS;
    use crate::mock_http::MockServer;
    use rstest::rstest;
    use std::net::IpAddr;
    use std::sync::atomic::Ordering;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n rest of a png";
    const PNG_HEADER: &str = "Content-Type: image/png\r\n";

    /// Refuses every private address but at the one host it names.
    struct PrivateBut(&'static str);

    #[async_trait::async_trait]
    impl DestinationPolicy for PrivateBut {
        async fn refuses(&self, host: &str, address: IpAddr) -> bool {
            destination::is_private_address(address) && host != self.0
        }
    }

    fn guarded(allowed_host: &'static str) -> GuardedHttp {
        GuardedHttp::new(Arc::new(PrivateBut(allowed_host)), false, &IMAGE_FETCH)
    }

    fn redirect(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
        .into_bytes()
    }

    fn port(server: &MockServer) -> &str {
        server.base_url.rsplit(':').next().expect("port")
    }

    /// The server's own default: before the server sets anything, every
    /// private address is refused, and nothing is sent to it.
    #[tokio::test]
    async fn a_private_address_is_refused_unsent() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok(PNG_HEADER, PNG)]).await;
        let http = GuardedHttp::new(
            Arc::new(destination::ImageDestinations::default()),
            false,
            &IMAGE_FETCH,
        );
        let err = open_image_capped(&http, &format!("{}/a.png", server.base_url), 1024)
            .await
            .err()
            .expect("refused");
        assert!(matches!(err, ImageFetchError::Refused), "{err:?}");
        assert!(err.is_answer());
        assert_eq!(server.requests(), 0);
    }

    /// The finding: a URL on an allowed host answers with a redirect to an
    /// internal service. Whether the hop names the service's host or its
    /// address, it is refused, and nothing reaches the service.
    #[rstest]
    #[case::to_a_name("127.0.0.1", "localhost")]
    #[case::to_an_address("localhost", "127.0.0.1")]
    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_refused(
        #[case] allowed: &'static str,
        #[case] internal: &'static str,
    ) {
        let service = MockServer::start_raw(vec![MockServer::raw_ok(PNG_HEADER, PNG)]).await;
        let target = format!("http://{internal}:{}/secret", port(&service));
        let source = MockServer::start_raw(vec![redirect(&target)]).await;
        let first = format!("http://{allowed}:{}/a.png", port(&source));
        let err = open_image_capped(&guarded(allowed), &first, 1024)
            .await
            .err()
            .expect("refused");
        assert!(matches!(err, ImageFetchError::Refused), "{err:?}");
        assert_eq!(source.requests(), 1);
        assert_eq!(service.requests(), 0, "the hop was never sent");
    }

    /// A redirect the policy allows is followed, but then the URL asked for
    /// no longer names the type: an internal answer typed
    /// `application/octet-stream` is not stored as the `.png` it was asked as.
    #[tokio::test]
    async fn after_a_redirect_the_url_names_no_type() {
        let untyped = MockServer::raw_ok("Content-Type: application/octet-stream\r\n", PNG);
        let direct = MockServer::start_raw(vec![untyped.clone()]).await;
        let image = open_image_capped(
            &GuardedHttp::open(&IMAGE_FETCH),
            &format!("{}/a.png", direct.base_url),
            1024,
        )
        .await
        .expect("typed by its URL");
        assert_eq!(image.content_type(), "image/png");

        let server = MockServer::start_raw(vec![redirect("/blob"), untyped]).await;
        let err = open_image_capped(
            &GuardedHttp::open(&IMAGE_FETCH),
            &format!("{}/a.png", server.base_url),
            1024,
        )
        .await
        .err()
        .expect("untyped");
        assert!(
            matches!(&err, ImageFetchError::NotAnImage(reason)
                if reason == "Request returned 'application/octet-stream' instead of an image type"),
            "{err:?}"
        );
        assert_eq!(server.requests(), 2, "the redirect was followed");
    }

    /// A redirect with the response's own image type is kept.
    #[tokio::test]
    async fn an_allowed_redirect_is_followed() {
        let server = MockServer::start_raw(vec![
            redirect("/elsewhere.jpg"),
            MockServer::raw_ok(PNG_HEADER, PNG),
        ])
        .await;
        let image = open_image_capped(&GuardedHttp::open(&IMAGE_FETCH), &server.base_url, 1024)
            .await
            .expect("an image");
        assert_eq!(image.content_type(), "image/png");
        assert_eq!(image.into_bytes().await.expect("bytes"), PNG);
    }

    /// A `Location` in raw UTF-8 is followed, as reqwest's own policy does.
    #[tokio::test]
    async fn a_utf8_location_is_followed() {
        let mut moved = b"HTTP/1.1 302 Found\r\nLocation: /caf\xc3\xa9.png\r\n".to_vec();
        moved.extend_from_slice(b"Content-Length: 0\r\nConnection: close\r\n\r\n");
        let server = MockServer::start_raw(vec![moved, MockServer::raw_ok(PNG_HEADER, PNG)]).await;
        let image = open_image_capped(&GuardedHttp::open(&IMAGE_FETCH), &server.base_url, 1024)
            .await
            .expect("followed");
        assert_eq!(image.content_type(), "image/png");
        assert_eq!(server.requests(), 2);
    }

    /// Past reqwest's own limit, or to a scheme a download does not speak, a
    /// redirect is no answer, and a failure of the provider.
    #[rstest]
    #[case::endless("/again", MAX_REDIRECTS + 1)]
    #[case::another_scheme("ftp://127.0.0.1/a.png", 1)]
    #[tokio::test]
    async fn a_redirect_that_cannot_be_followed_is_unavailable(
        #[case] location: &str,
        #[case] sent: usize,
    ) {
        let server = MockServer::start_raw(vec![redirect(location)]).await;
        let (outcome, failures) = crate::rate_limit::count_request_failures(open_image_capped(
            &GuardedHttp::open(&IMAGE_FETCH),
            &server.base_url,
            1024,
        ))
        .await;
        let err = outcome.err().expect("no image");
        assert!(matches!(err, ImageFetchError::Unavailable), "{err:?}");
        assert!(!err.is_answer());
        assert_eq!(failures, 1);
        assert_eq!(server.requests(), sent);
    }

    /// With a proxy configured, which would resolve a name out of the
    /// resolver's sight, a name is checked on the server's own answer before
    /// anything is sent, then reached at the addresses checked (the hosts the
    /// proxy settings exclude go there directly; this test sets no proxy, so
    /// all do). An address literal is checked as it is, and a name the server
    /// cannot resolve is not sent to the proxy unchecked.
    #[tokio::test]
    async fn through_a_proxy_a_destination_is_checked_before_it_is_sent() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok(PNG_HEADER, PNG)]).await;
        let at = |host: &str| format!("http://{host}:{}/a.png", port(&server));
        let proxied = |allowed| GuardedHttp::new(Arc::new(PrivateBut(allowed)), true, &IMAGE_FETCH);

        let image = open_image_capped(&proxied("localhost"), &at("localhost"), 1024)
            .await
            .expect("an allowed name");
        assert_eq!(image.content_type(), "image/png");
        assert!(
            open_image_capped(&proxied("127.0.0.1"), &at("127.0.0.1"), 1024)
                .await
                .is_ok(),
            "an allowed address"
        );
        assert_eq!(server.requests(), 2);

        for (allowed, host) in [("127.0.0.1", "localhost"), ("localhost", "127.0.0.1")] {
            let err = open_image_capped(&proxied(allowed), &at(host), 1024)
                .await
                .err()
                .expect("refused");
            assert!(matches!(err, ImageFetchError::Refused), "{host}: {err:?}");
        }
        let err = open_image_capped(&proxied("x"), "http://name.invalid/a.png", 1024)
            .await
            .err()
            .expect("unresolvable");
        assert!(matches!(err, ImageFetchError::Unavailable), "{err:?}");
        assert_eq!(server.requests(), 2, "nothing refused was sent");
    }

    /// A refused candidate is skipped like a missing one, and is not a
    /// failure of the refresh; a direct download of one has no bytes. (The
    /// process's policy refuses every private address but loopback, where
    /// tests serve.)
    #[tokio::test]
    async fn the_scan_skips_a_refused_candidate() {
        destination::test_support::allow_loopback_images();
        let private = "http://10.255.255.1/a.png";
        let (outcome, failures) =
            crate::rate_limit::count_request_failures(download_artwork(private)).await;
        assert!(matches!(outcome, Err(ArtworkDownloadFailure::Skip)));
        assert_eq!(failures, 0);
        assert_eq!(download_bytes(private).await, None);
    }

    /// The resolver refuses at connect time too: a name whose answer turns
    /// private after the check before sending (DNS rebinding) never reaches
    /// that address.
    #[tokio::test]
    async fn a_name_that_turns_private_after_the_check_is_refused_at_connect() {
        /// Allows the first lookup's addresses, then refuses every private one.
        struct Rebinding {
            allowed: usize,
            checks: std::sync::atomic::AtomicUsize,
        }
        #[async_trait::async_trait]
        impl DestinationPolicy for Rebinding {
            async fn refuses(&self, _host: &str, address: IpAddr) -> bool {
                let check = self.checks.fetch_add(1, Ordering::SeqCst);
                check >= self.allowed && destination::is_private_address(address)
            }
        }
        let server = MockServer::start_raw(vec![MockServer::raw_ok(PNG_HEADER, PNG)]).await;
        let allowed = destination::lookup("localhost", 80)
            .await
            .expect("localhost")
            .len();
        let http = GuardedHttp::new(
            Arc::new(Rebinding {
                allowed,
                checks: std::sync::atomic::AtomicUsize::new(0),
            }),
            false,
            &IMAGE_FETCH,
        );
        let url = format!("http://localhost:{}/a.png", port(&server));
        let err = open_image_capped(&http, &url, 1024)
            .await
            .err()
            .expect("refused at connect");
        assert!(matches!(err, ImageFetchError::Unavailable), "{err:?}");
        assert_eq!(server.requests(), 0, "nothing reached the address");
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

/// The brands of an ISO media file that hold an image: AVIF and HEIF. A movie
/// has the same `ftyp` box with other brands.
const IMAGE_FTYP_BRANDS: [&[u8; 4]; 10] = [
    b"avif", b"avis", b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1",
];

/// Whether a body starting with `head` is an image. Wider than
/// [`sniff_image_ext`], which picks a file extension for plugin artwork: a
/// channel logo or a scan's image is any format a client may be able to show,
/// SVG included. A format not listed here is refused, as a response of an
/// unknown type is.
pub(crate) fn is_image(head: &[u8]) -> bool {
    sniff_image_ext(head).is_some()
        || is_bmp(head)
        || head.starts_with(b"II*\0")
        || head.starts_with(b"MM\0*")
        // ICO and CUR.
        || head.starts_with(b"\0\0\x01\0")
        || head.starts_with(b"\0\0\x02\0")
        // JPEG XL, bare and in its container.
        || head.starts_with(b"\xFF\x0A")
        || head.starts_with(b"\0\0\0\x0CJXL \r\n\x87\n")
        // JPEG 2000, bare and in its container.
        || head.starts_with(b"\xFF\x4F\xFF\x51")
        || head.starts_with(b"\0\0\0\x0CjP  \r\n\x87\n")
        || head.get(4..8) == Some(b"ftyp".as_slice())
            && head
                .get(8..12)
                .is_some_and(|brand| IMAGE_FTYP_BRANDS.iter().any(|known| brand == *known))
        || is_svg(head)
}

/// Whether `head` is the start of a BMP: `BM`, then the size of one of the
/// bitmap header formats after the 14-byte file header, since two letters
/// alone would pass plenty of text.
fn is_bmp(head: &[u8]) -> bool {
    head.starts_with(b"BM")
        && head
            .get(14..18)
            .and_then(|size| <[u8; 4]>::try_from(size).ok())
            .is_some_and(|size| {
                matches!(u32::from_le_bytes(size), 12 | 40 | 52 | 56 | 64 | 108 | 124)
            })
}

/// Whether `head` is the start of an SVG document: `<svg`, after only what XML
/// allows before its root (a byte-order mark, white space, the declaration,
/// comments, processing instructions and an SVG doctype). A page that has an
/// SVG inside it does not count.
fn is_svg(head: &[u8]) -> bool {
    /// `text` after the first `end`, or nothing when it has none.
    fn after<'a>(text: &'a [u8], end: &[u8]) -> &'a [u8] {
        text.windows(end.len())
            .position(|window| window == end)
            .map_or(&[], |at| &text[at + end.len()..])
    }

    let mut rest = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    loop {
        rest = rest.trim_ascii_start();
        if rest.starts_with(b"<?") {
            rest = after(rest, b"?>");
        } else if rest.starts_with(b"<!--") {
            rest = after(rest, b"-->");
        } else if rest
            .get(..9)
            .is_some_and(|tag| tag.eq_ignore_ascii_case(b"<!doctype"))
        {
            let name = rest[9..].trim_ascii_start();
            if !name
                .get(..3)
                .is_some_and(|n| n.eq_ignore_ascii_case(b"svg"))
            {
                return false;
            }
            // An internal subset holds `>`s of its own.
            let subset = rest.iter().position(|&b| b == b'[');
            let close = rest.iter().position(|&b| b == b'>');
            rest = match (subset, close) {
                (Some(subset), Some(close)) if subset < close => {
                    after(after(&rest[subset..], b"]"), b">")
                }
                _ => after(rest, b">"),
            };
        } else {
            break;
        }
    }
    rest.strip_prefix(b"<svg")
        .and_then(<[u8]>::first)
        .is_some_and(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/')
}

/// `image_negative_cache_ttl_secs`'s default.
pub const DEFAULT_IMAGE_NEGATIVE_CACHE_TTL_SECS: u64 = 3600;

/// The values `image_negative_cache_ttl_secs` takes.
pub const IMAGE_NEGATIVE_CACHE_TTL_SECS: RangeInclusive<u64> = 300..=86_400;

/// How many URLs the negative cache remembers at most. An entry is a hash and
/// a time, so this is about a MiB, and it is more than a large playlist's
/// channels. Past it the entry closest to expiring goes first.
const IMAGE_NEGATIVE_CACHE_ENTRIES: usize = 16_384;

/// URLs that answered with no image, each until its time runs out, so that
/// asking for the image again does not ask its source again. Upstream stops
/// the retries by removing the image's row, which a later refresh has to bring
/// back; here the row stays and the URL is tried again when its entry expires.
///
/// Holds a URL's MD5, the name of its cache folder, so its size does not
/// depend on the URLs. Only [`ImageFetchError::is_answer`] outcomes belong
/// here.
#[derive(Debug)]
pub(crate) struct NegativeImageCache {
    ttl: Duration,
    capacity: usize,
    until: Mutex<HashMap<Uuid, Instant>>,
}

impl NegativeImageCache {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, IMAGE_NEGATIVE_CACHE_ENTRIES)
    }

    fn with_capacity(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            until: Mutex::default(),
        }
    }

    /// Whether `url`'s answer is still good.
    pub(crate) fn contains(&self, url: Uuid) -> bool {
        self.until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&url)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Whether `url` went in after `moment`.
    pub(crate) fn inserted_since(&self, url: Uuid, moment: Instant) -> bool {
        self.until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&url)
            .and_then(|until| until.checked_sub(self.ttl))
            .is_some_and(|inserted| inserted > moment)
    }

    /// Remembers `url` for the cache's time to live. At capacity, expired
    /// entries make room, then the one with the least time left.
    pub(crate) fn insert(&self, url: Uuid) {
        let now = Instant::now();
        let mut entries = self.until.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= self.capacity && !entries.contains_key(&url) {
            entries.retain(|_, until| *until > now);
            if entries.len() >= self.capacity {
                let oldest = entries.iter().min_by_key(|(_, until)| **until);
                if let Some(oldest) = oldest.map(|(url, _)| *url) {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(url, now + self.ttl);
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

pub(crate) async fn download_artwork(url: &str) -> Result<ArtworkDownload, ArtworkDownloadFailure> {
    let image = open_image(url).await.map_err(artwork_failure)?;
    let content_type = image.content_type.clone();
    let content_length = image.content_length;
    let bytes = image.into_bytes().await.map_err(artwork_failure)?;
    Ok(ArtworkDownload {
        bytes,
        content_type,
        content_length,
    })
}

/// What automatic acquisition does about a failed candidate. One over the
/// limit is like a missing one: the same provider's next candidate may fit,
/// and a failure would keep the item from being marked as refreshed, for a
/// body that will not change. So is a refused one, which was logged when it
/// was refused.
fn artwork_failure(error: ImageFetchError) -> ArtworkDownloadFailure {
    match error {
        ImageFetchError::Missing | ImageFetchError::Refused => ArtworkDownloadFailure::Skip,
        ImageFetchError::TooLarge => {
            tracing::debug!("artwork is over image_download_max_mb; trying the next candidate");
            ArtworkDownloadFailure::Skip
        }
        ImageFetchError::NotAnImage(reason) => {
            crate::rate_limit::note_request_failure();
            tracing::warn!(%reason, "artwork provider returned a non-image response");
            ArtworkDownloadFailure::Stop
        }
        ImageFetchError::Unavailable | ImageFetchError::Storage(_) => ArtworkDownloadFailure::Stop,
    }
}
