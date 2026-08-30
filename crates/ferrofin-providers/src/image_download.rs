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

use ferrofin_model::net::mime_types;

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
/// `None` when the request fails or the response is not a success — the caller
/// turns that into its own error. The media type is NOT validated here; see
/// [`reject_non_image`].
pub(crate) async fn download_image(http: &reqwest::Client, url: &str) -> Option<(Vec<u8>, String)> {
    let resp = http.get(url).send().await.ok()?;
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
    let bytes = resp.bytes().await.ok()?.to_vec();
    Some((bytes, content_type))
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
