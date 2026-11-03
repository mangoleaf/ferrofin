//! Fetching an image from a URL the server was handed — the seam a part of
//! the server that cannot depend on `ferrofin-providers` (an extension) uses,
//! so its downloads keep the server's image download limits.
//!
//! The implementation (`ferrofin_providers::remote_images::LimitedImageFetcher`)
//! goes through the providers' vetted download path: it never reaches a
//! private address, on the first request or on any redirect, unless
//! `image_private_http_allow` lets it (gl-1096), and it reads at most
//! `image_download_max_mb` of the body (gl-1095). LDAP-Auth's profile images
//! given as a URL (`LdapProfileImageFormat` `Url`) are fetched through it;
//! upstream's plugin uses the default `HttpClient` with neither check.
//!
//! The trait is object-safe and carries an `_assert_object_safe` assertion.

use async_trait::async_trait;

/// Why a URL gave no image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RemoteImageError {
    /// The URL, or a redirect from it, is at a private address that the
    /// server's `image_private_http_allow` does not list.
    #[error(
        "the URL, or a redirect from it, is at a private address, which image downloads may not \
         reach unless image_private_http_allow lists it"
    )]
    Refused,
    /// The body is over the `image_download_max_mb` limit.
    #[error("the image is over the image_download_max_mb limit")]
    TooLarge,
    /// The server answered, but its first bytes are not an image.
    #[error("the response is not an image")]
    NotAnImage,
    /// `404` or `403`: the server has no image there.
    #[error("the server has no image at the URL")]
    Missing,
    /// No usable answer: a URL that cannot be requested, a transport failure,
    /// a timeout, a cooldown, or another error status.
    #[error("the image could not be downloaded")]
    Unavailable,
}

/// Downloads images under the server's image download limits.
#[async_trait]
pub trait RemoteImageFetcher: Send + Sync {
    /// GETs `url` and returns its body, which must start like an image. The
    /// caller names the stored file itself, so no media type is resolved.
    ///
    /// # Errors
    /// [`RemoteImageError`] says why there is no image.
    async fn fetch_image(&self, url: &str) -> Result<Vec<u8>, RemoteImageError>;
}

fn _assert_object_safe(_: &dyn RemoteImageFetcher) {}
