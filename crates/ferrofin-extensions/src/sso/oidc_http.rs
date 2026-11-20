//! The HTTP client every call to an OpenID Connect identity provider goes
//! through: `openidconnect`'s [`AsyncHttpClient`] over a reqwest client built
//! for it. Discovery and its key set use it now; the code exchange and
//! userinfo of the next phases are to use it too.
//!
//! - **No redirect is followed.** A `3xx` answer is an error: the URLs come
//!   from the admin's issuer and the document it serves, and a redirect would
//!   take a request elsewhere.
//! - **Limits** ([`CONNECT_TIMEOUT`], [`REQUEST_TIMEOUT`],
//!   [`MAX_RESPONSE_BYTES`]): constants with the milestone's D19 values.
//! - **`https` only**, unless the provider's `AllowHttpIssuer` is on (D11).
//!   That holds for every URL fetched, the ones the discovery document names
//!   included.
//! - **No private-address rule**, unlike the image downloader's: the admin
//!   named this host, and an identity provider is often on a private address.
//!
//! No [`OidcHttpError`] carries a URL (reqwest's are taken off with
//! `without_url`) or anything the provider answered, so one can be logged
//! (`docs/conventions/LOGGING.md`, "Never log secrets"). The innermost cause
//! of a TLS failure may still name the host.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use openidconnect::http::StatusCode;
use openidconnect::{AsyncHttpClient, HttpRequest, HttpResponse};

/// How long connecting to an identity provider may take, the TLS handshake
/// included. Candidate setting (D19): a constant until the owner asks for it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one request to an identity provider may take, from connecting to
/// the last byte of its answer. Candidate setting (D19): a constant until the
/// owner asks for it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest answer read from an identity provider, in bytes once decoded:
/// 1 MiB. Candidate setting (D19): a constant until the owner asks for it.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Why a request to an identity provider failed. No message carries the URL
/// or anything the provider answered.
#[derive(Debug, thiserror::Error)]
pub enum OidcHttpError {
    /// An `http` URL, and the provider's `AllowHttpIssuer` is off.
    #[error(
        "an http address is refused: turn on \"allow an http issuer\" only for an identity \
         provider on a trusted network"
    )]
    HttpRefused,
    /// Neither an `http` nor an `https` URL.
    #[error("only http and https addresses are fetched")]
    Scheme,
    /// The identity provider answered with a redirect.
    #[error("the identity provider answered with a redirect ({0}), which is not followed")]
    Redirect(StatusCode),
    /// The answer is longer than [`MAX_RESPONSE_BYTES`].
    #[error("the identity provider's answer is longer than {MAX_RESPONSE_BYTES} bytes")]
    TooLarge,
    /// No connection, or no whole answer, in time: [`CONNECT_TIMEOUT`] or the
    /// request timeout.
    #[error("the identity provider did not answer within {} seconds", .0.as_secs())]
    Timeout(Duration),
    /// The connection could not be made: the name, the port, or TLS.
    #[error("could not connect to the identity provider: {0}")]
    Connect(String),
    /// Anything else on the way.
    #[error("the request to the identity provider failed: {0}")]
    Request(String),
}

/// The client for one identity provider: cheap to clone, and every clone
/// shares one connection pool. [`OidcHttpClient::new`] makes the shared one,
/// and [`allowing_http`](Self::allowing_http) a copy for a provider whose
/// `AllowHttpIssuer` is on.
#[derive(Debug, Clone)]
pub struct OidcHttpClient {
    /// The reqwest client, or why it could not be built (only when the TLS
    /// backend cannot start), which every request then reports.
    client: Result<reqwest::Client, String>,
    /// Whether `http` URLs are fetched (D11).
    allow_http: bool,
    /// [`REQUEST_TIMEOUT`], shorter in tests.
    request_timeout: Duration,
}

impl Default for OidcHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OidcHttpClient {
    /// The client with the D19 limits, fetching `https` URLs only.
    #[must_use]
    pub fn new() -> Self {
        Self::with_request_timeout(REQUEST_TIMEOUT)
    }

    /// The client with `request_timeout` in place of [`REQUEST_TIMEOUT`], so
    /// a test of the timeout need not wait ten seconds.
    pub(crate) fn with_request_timeout(request_timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(ferrofin_health::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(request_timeout)
            .build()
            .map_err(|e| innermost(&e));
        Self {
            client,
            allow_http: false,
            request_timeout,
        }
    }

    /// This client, fetching `http` URLs too when `allow_http` is on: for a
    /// provider whose `AllowHttpIssuer` is on (D11).
    #[must_use]
    pub fn allowing_http(&self, allow_http: bool) -> Self {
        Self {
            allow_http,
            ..self.clone()
        }
    }

    /// Sends `request` and reads its answer, within the limits above.
    ///
    /// # Errors
    ///
    /// An [`OidcHttpError`] for a refused URL, a redirect, an answer over
    /// [`MAX_RESPONSE_BYTES`], a timeout, or a failed connection.
    pub async fn send(&self, request: HttpRequest) -> Result<HttpResponse, OidcHttpError> {
        let client = self
            .client
            .as_ref()
            .map_err(|why| OidcHttpError::Request(why.clone()))?;
        let request = reqwest::Request::try_from(request)
            .map_err(|e| OidcHttpError::Request(innermost(&e.without_url())))?;
        match request.url().scheme() {
            "https" => {}
            "http" if self.allow_http => {}
            "http" => return Err(OidcHttpError::HttpRefused),
            _ => return Err(OidcHttpError::Scheme),
        }
        let mut response = client.execute(request).await.map_err(|e| self.failure(e))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(OidcHttpError::Redirect(status));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(OidcHttpError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| self.failure(e))? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(OidcHttpError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let mut answer = HttpResponse::new(body);
        *answer.status_mut() = status;
        *answer.headers_mut() = std::mem::take(response.headers_mut());
        Ok(answer)
    }

    /// `e` as an [`OidcHttpError`], without its URL.
    fn failure(&self, e: reqwest::Error) -> OidcHttpError {
        if e.is_timeout() {
            OidcHttpError::Timeout(if e.is_connect() {
                CONNECT_TIMEOUT
            } else {
                self.request_timeout
            })
        } else if e.is_connect() {
            OidcHttpError::Connect(innermost(&e.without_url()))
        } else {
            OidcHttpError::Request(innermost(&e.without_url()))
        }
    }
}

impl<'c> AsyncHttpClient<'c> for OidcHttpClient {
    type Error = OidcHttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, OidcHttpError>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(self.send(request))
    }
}

/// The innermost cause of `e`: what says why (the refused connection, the
/// certificate), where reqwest's own message only says that sending failed.
fn innermost(e: &(dyn std::error::Error + 'static)) -> String {
    std::iter::successors(Some(e), |e| e.source())
        .last()
        .map_or_else(String::new, ToString::to_string)
}

#[cfg(test)]
mod tests;
