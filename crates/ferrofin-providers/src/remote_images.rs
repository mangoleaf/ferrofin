//! [`RemoteImageFetcher`] over this crate's image downloads, for the parts of
//! the server that cannot depend on it (an extension, through its
//! `ExtensionContext`).
//!
//! A fetch is `image_download::fetch_image_bytes`:
//! the destination policy on the request and every redirect
//! (`image_private_http_allow`, gl-1096), the origin's rate limiter, the
//! `image_download_max_mb` body limit (gl-1095), and a body that must start
//! like an image.

use async_trait::async_trait;
use ferrofin_traits::remote_images::{RemoteImageError, RemoteImageFetcher};

use crate::image_download::{ImageFetchError, fetch_image_bytes};

/// The server's [`RemoteImageFetcher`]: every fetch goes through the same
/// vetted path as artwork downloads.
#[derive(Debug, Default, Clone, Copy)]
pub struct LimitedImageFetcher;

#[async_trait]
impl RemoteImageFetcher for LimitedImageFetcher {
    async fn fetch_image(&self, url: &str) -> Result<Vec<u8>, RemoteImageError> {
        fetch_image_bytes(url)
            .await
            .map_err(|error| remote_error(&error))
    }
}

/// The seam's error for a failed download.
fn remote_error(error: &ImageFetchError) -> RemoteImageError {
    match error {
        ImageFetchError::Refused => RemoteImageError::Refused,
        ImageFetchError::TooLarge => RemoteImageError::TooLarge,
        ImageFetchError::NotAnImage(_) => RemoteImageError::NotAnImage,
        ImageFetchError::Missing => RemoteImageError::Missing,
        ImageFetchError::Unavailable | ImageFetchError::Storage(_) => RemoteImageError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination;
    use crate::mock_http::MockServer;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n rest of a png";

    /// The process's policy in tests refuses every private address but
    /// loopback, where the mock servers listen.
    fn fetcher() -> LimitedImageFetcher {
        destination::test_support::allow_loopback_images();
        LimitedImageFetcher
    }

    #[tokio::test]
    async fn an_image_comes_back_whole_whatever_type_it_declares() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok(
            "Content-Type: application/octet-stream\r\n",
            PNG,
        )])
        .await;
        let bytes = fetcher()
            .fetch_image(&format!("{}/avatar", server.base_url))
            .await
            .expect("an image");
        assert_eq!(bytes, PNG);
    }

    #[tokio::test]
    async fn a_private_address_is_refused() {
        let err = fetcher()
            .fetch_image("http://10.255.255.1/avatar.jpg")
            .await
            .expect_err("refused");
        assert_eq!(err, RemoteImageError::Refused);
    }

    #[tokio::test]
    async fn a_body_over_the_download_limit_is_refused() {
        let declared = crate::rate_limit::request_limits().download_max_bytes + 1;
        let headers = format!("Content-Type: image/png\r\nContent-Length: {declared}\r\n");
        let server = MockServer::start_raw(vec![MockServer::raw_ok(&headers, b"")]).await;
        let err = fetcher()
            .fetch_image(&server.base_url)
            .await
            .expect_err("too large");
        assert_eq!(err, RemoteImageError::TooLarge);
    }

    #[tokio::test]
    async fn a_body_that_is_no_image_is_refused() {
        let server = MockServer::start_raw(vec![MockServer::raw_ok(
            "Content-Type: image/png\r\n",
            b"<html></html>",
        )])
        .await;
        let err = fetcher()
            .fetch_image(&server.base_url)
            .await
            .expect_err("not an image");
        assert_eq!(err, RemoteImageError::NotAnImage);
    }

    #[tokio::test]
    async fn a_missing_image_and_a_failing_server_have_their_own_reasons() {
        let server = MockServer::start_raw(vec![
            MockServer::raw_status("404 Not Found"),
            // Not one the limiter retries (a 5xx is), so no backoff is waited out.
            MockServer::raw_status("410 Gone"),
        ])
        .await;
        let fetcher = fetcher();
        assert_eq!(
            fetcher.fetch_image(&server.base_url).await,
            Err(RemoteImageError::Missing)
        );
        assert_eq!(
            fetcher.fetch_image(&server.base_url).await,
            Err(RemoteImageError::Unavailable)
        );
    }

    #[test]
    fn every_download_failure_has_its_own_reason() {
        assert_eq!(
            remote_error(&ImageFetchError::Refused),
            RemoteImageError::Refused
        );
        assert_eq!(
            remote_error(&ImageFetchError::TooLarge),
            RemoteImageError::TooLarge
        );
        assert_eq!(
            remote_error(&ImageFetchError::NotAnImage("text/html".to_owned())),
            RemoteImageError::NotAnImage
        );
        assert_eq!(
            remote_error(&ImageFetchError::Missing),
            RemoteImageError::Missing
        );
        assert_eq!(
            remote_error(&ImageFetchError::Unavailable),
            RemoteImageError::Unavailable
        );
        assert_eq!(
            remote_error(&ImageFetchError::Storage(std::io::Error::other("disk"))),
            RemoteImageError::Unavailable
        );
    }
}
