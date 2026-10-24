//! The source-fetch seam.
//!
//! `refresh_guide` needs to read the configured M3U and XMLTV URLs. That network
//! I/O sits behind [`SourceFetcher`] so the manager's channel/guide-population
//! logic can be unit-tested with an in-memory fake instead of a live server.

use std::time::Duration;

use async_trait::async_trait;

use ferrofin_traits::error::ServiceError;
use ferrofin_util::http_client::HTTP_CLIENT_DEFAULT_TIMEOUT;

use crate::error::LiveTvError;

/// Fetches the body of a tuner/guide/listings source URL (or local file path).
#[async_trait]
pub trait SourceFetcher: Send + Sync {
    /// Fetches `url` and returns its body as text.
    async fn fetch(&self, url: &str) -> Result<String, ServiceError>;

    /// Fetches `url` presenting `user_agent` when one is given
    /// (`M3uParser.GetListingsStream`: `requestMessage.Headers.UserAgent.
    /// TryParseAdd(info.UserAgent)` for a non-empty configured value, so a
    /// playlist host that only serves a particular client still answers).
    ///
    /// The default ignores the agent and delegates to [`fetch`](Self::fetch),
    /// so a text-only fake keeps working; the real fetcher sets the header.
    async fn fetch_as(&self, url: &str, user_agent: Option<&str>) -> Result<String, ServiceError> {
        let _ = user_agent;
        self.fetch(url).await
    }

    /// Fetches `url` and returns its body as raw bytes — for documents the
    /// server passes through untouched (the Schedules Direct country list).
    ///
    /// The default goes through [`fetch`](Self::fetch), so a text-only fake
    /// serves both; the real fetcher reads the response bytes directly.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
        self.fetch(url).await.map(String::into_bytes)
    }

    /// Fetches `url` and returns `(status, body)` **without** failing on a
    /// non-success status.
    ///
    /// The HDHomeRun host needs the code itself, not just success-or-not:
    /// `HdHomerunHost.GetModelInfo` treats a **404** as "this is an HDHR4,
    /// which has no `discover.json`" and answers with a synthetic model rather
    /// than an error (v10.11.8 HdHomerunHost.cs:135-157), while every other
    /// failure propagates. Flattening that to one error variant would turn a
    /// perfectly good tuner into a broken one.
    ///
    /// The default reports `200` for whatever [`fetch`](Self::fetch) returns,
    /// so a text-only fake keeps working; a fake that wants to exercise the
    /// 404 branch overrides this.
    async fn fetch_with_status(&self, url: &str) -> Result<(u16, String), ServiceError> {
        self.fetch(url).await.map(|body| (200, body))
    }

    /// Fetches an XMLTV listings document: [`fetch`](Self::fetch), except that
    /// a gzipped source is decompressed. Port of
    /// `XmlTvListingsProvider.UnzipIfNeededAndCopy`, which decides on the URL
    /// after redirects, or the path, with any query stripped: `.gz` or
    /// `.gzip`.
    ///
    /// The default is plain [`fetch`](Self::fetch), so a text-only fake keeps
    /// working; the real fetcher decompresses.
    async fn fetch_listings(&self, url: &str) -> Result<String, ServiceError> {
        self.fetch(url).await
    }

    /// Sends `request` and answers its status and body, whatever the status.
    ///
    /// The Schedules Direct API needs more than a `GET`: a `POST`/`PUT` with a
    /// JSON body and a `token` header, and the error code a failed response
    /// carries in its body (`SchedulesDirect.Request<T>`), which a fetch that
    /// fails on a non-success status would throw away.
    ///
    /// The default serves a header-less `GET` through
    /// [`fetch_with_status`](Self::fetch_with_status), so a text-only fake keeps
    /// working, and refuses anything else; the real fetcher sends it all.
    async fn send(&self, request: SourceRequest) -> Result<SourceResponse, ServiceError> {
        if request.method != RequestMethod::Get
            || !request.headers.is_empty()
            || request.json.is_some()
        {
            return Err(ServiceError::backend(format!(
                "this fetcher cannot send a {:?} request to {}",
                request.method, request.url
            )));
        }
        let (status, body) = self.fetch_with_status(&request.url).await?;
        Ok(SourceResponse {
            status,
            body: body.into_bytes(),
        })
    }
}

/// The HTTP method of a [`SourceRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
}

/// A request for [`SourceFetcher::send`].
///
/// Its `Debug` prints header names but not their values, and the body's
/// length but not its bytes: they carry credentials (Schedules Direct's
/// `token` header, its `/token` body).
#[derive(Clone, PartialEq, Eq)]
pub struct SourceRequest {
    /// The method.
    pub method: RequestMethod,
    /// The absolute URL.
    pub url: String,
    /// Extra request headers, as `(name, value)`.
    pub headers: Vec<(String, String)>,
    /// A JSON body, sent as `application/json; charset=utf-8`
    /// (`JsonContent.Create` upstream).
    pub json: Option<Vec<u8>>,
    /// Whether the request ends at the response headers
    /// (`HttpCompletionOption.ResponseHeadersRead`): the real fetcher then
    /// answers a success with an empty body, unread, and reads only a
    /// failure's body.
    pub headers_only: bool,
}

impl std::fmt::Debug for SourceRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("json_bytes", &self.json.as_ref().map(Vec::len))
            .field("headers_only", &self.headers_only)
            .finish()
    }
}

impl SourceRequest {
    /// A request with no headers and no body.
    #[must_use]
    pub fn new(method: RequestMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            json: None,
            headers_only: false,
        }
    }

    /// Adds the header `name: value`.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Sets the JSON body.
    #[must_use]
    pub fn json(mut self, body: Vec<u8>) -> Self {
        self.json = Some(body);
        self
    }

    /// Ends the request at the response headers: sets `headers_only`.
    #[must_use]
    pub fn headers_only(mut self) -> Self {
        self.headers_only = true;
        self
    }
}

/// What [`SourceFetcher::send`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response body.
    pub body: Vec<u8>,
}

impl SourceResponse {
    /// Whether the status is `2xx` (`HttpResponseMessage.IsSuccessStatusCode`).
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The `User-Agent` the real fetcher presents, mirroring the product header
/// Jellyfin's default `HttpClient` sends (`Jellyfin-Server/<version>`,
/// `Jellyfin.Server/Startup.cs`). A generic product token only — never
/// anything identifying the user or the install.
const USER_AGENT: &str = concat!("Ferrofin/", env!("CARGO_PKG_VERSION"));

/// The real fetcher: `reqwest` for `http(s)://`, a filesystem read otherwise.
///
/// Upstream sends these requests through `NamedClient.Default`, so each one
/// gives up after its `HttpClient.Timeout` ([`HTTP_CLIENT_DEFAULT_TIMEOUT`]).
/// The M3U and HDHomeRun requests are `ResponseHeadersRead`, so the timeout
/// stops at their headers; the Schedules Direct ones read the whole response,
/// so it covers their bodies, except a `headers_only` one (adding a lineup),
/// which ends at a success's headers. An XMLTV download is bounded by its
/// caller (`XmlTvCache`'s `DOWNLOAD_TIMEOUT`, upstream's `_downloadTimeout`)
/// instead.
#[derive(Debug, Clone)]
pub struct ReqwestFetcher {
    client: reqwest::Client,
    /// The `HttpClient.Timeout` (a test shortens it).
    timeout: Duration,
}

/// How far the fetcher's timeout reaches into one request.
#[derive(Debug, Clone, Copy)]
enum Deadline {
    /// Up to the response headers; the body is read with no limit
    /// (`HttpCompletionOption.ResponseHeadersRead`).
    Headers,
    /// The whole exchange, body included (`ResponseContentRead`, the default).
    Total,
    /// None: the caller bounds the request (the XMLTV download).
    Caller,
}

/// The error a request to `url` that ran out of time answers.
fn timed_out(url: &str) -> ServiceError {
    ServiceError::Backend(format!("Timed out fetching {url}"))
}

/// Maps a `reqwest` failure of `operation` on `url`: a timeout reads as one,
/// anything else keeps its cause.
fn http_failure(operation: &str, url: &str, error: reqwest::Error) -> ServiceError {
    if error.is_timeout() {
        timed_out(url)
    } else {
        LiveTvError::http(format!("{operation} {url}"), error).into()
    }
}

impl Default for ReqwestFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestFetcher {
    /// Creates a fetcher over a fresh `reqwest` client presenting the
    /// [`USER_AGENT`] product header.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            // A builder failure here means the TLS backend could not
            // initialise; the plain client surfaces the same condition on
            // first use, where it is reported per fetch rather than at wiring.
            .unwrap_or_default();
        Self {
            client,
            timeout: HTTP_CLIENT_DEFAULT_TIMEOUT,
        }
    }

    /// Sends `request` to `url` within `deadline`.
    async fn send_within(
        &self,
        request: reqwest::RequestBuilder,
        url: &str,
        deadline: Deadline,
    ) -> Result<reqwest::Response, ServiceError> {
        let sent = match deadline {
            Deadline::Headers => tokio::time::timeout(self.timeout, request.send())
                .await
                .map_err(|_| timed_out(url))?,
            Deadline::Total => request.timeout(self.timeout).send().await,
            Deadline::Caller => request.send().await,
        };
        sent.map_err(|e| http_failure("fetch", url, e))
    }

    /// Performs the `GET` within `deadline`, with a per-request `User-Agent`
    /// when it is a valid header value (an invalid one is dropped, as
    /// `TryParseAdd` drops it), failing on transport errors and non-success
    /// statuses.
    async fn get(
        &self,
        url: &str,
        user_agent: Option<&str>,
        deadline: Deadline,
    ) -> Result<reqwest::Response, ServiceError> {
        let mut request = self.client.get(url);
        if let Some(agent) = user_agent.map(str::trim).filter(|agent| !agent.is_empty())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(agent)
        {
            request = request.header(reqwest::header::USER_AGENT, value);
        }
        let resp = self.send_within(request, url, deadline).await?;
        Ok(resp
            .error_for_status()
            .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?)
    }
}

/// Whether `url` names a remote `http(s)://` source (vs. a local path).
fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Whether a listings source is gzipped, as `UnzipIfNeededAndCopy` decides:
/// the extension of `name` (a URL after redirects, or a path) with any query
/// stripped is `.gz` or `.gzip`, ignoring case.
fn is_gzip_name(name: &str) -> bool {
    let name = name.split('?').next().unwrap_or(name);
    std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("gzip")
        })
}

/// Decompresses a gzipped listings document and decodes it as UTF-8 (lossily,
/// like `.text()` on a response that names no charset). Every gzip member is
/// read, as .NET's `GZipStream` reads concatenated members. It runs on the
/// blocking pool, because an EPG can run to tens of megabytes.
async fn gunzip<B>(name: &str, bytes: B) -> Result<String, ServiceError>
where
    B: AsRef<[u8]> + Send + 'static,
{
    let context = format!("extract gz {name}");
    let text = tokio::task::spawn_blocking(move || {
        let mut text = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::MultiGzDecoder::new(bytes.as_ref()),
            &mut text,
        )
        .map(|_| text)
    })
    .await
    .map_err(|e| LiveTvError::io(context.clone(), std::io::Error::other(e)))?
    .map_err(|e| LiveTvError::io(context, e))?;
    Ok(String::from_utf8(text)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

#[async_trait]
impl SourceFetcher for ReqwestFetcher {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        if is_http(url) {
            self.get(url, None, Deadline::Headers)
                .await?
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            tokio::fs::read_to_string(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e).into())
        }
    }

    async fn fetch_as(&self, url: &str, user_agent: Option<&str>) -> Result<String, ServiceError> {
        if is_http(url) {
            self.get(url, user_agent, Deadline::Headers)
                .await?
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            self.fetch(url).await
        }
    }

    async fn fetch_with_status(&self, url: &str) -> Result<(u16, String), ServiceError> {
        if is_http(url) {
            let resp = self
                .send_within(self.client.get(url), url, Deadline::Headers)
                .await?;
            let status = resp.status().as_u16();
            let body = resp
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e))?;
            Ok((status, body))
        } else {
            // A local path has no status; a missing file is the 404 analogue,
            // which is exactly how the HDHR4 branch reads it.
            match self.fetch(url).await {
                Ok(body) => Ok((200, body)),
                Err(e) => {
                    let path = url.strip_prefix("file://").unwrap_or(url);
                    if tokio::fs::try_exists(path).await.unwrap_or(false) {
                        Err(e)
                    } else {
                        Ok((404, String::new()))
                    }
                }
            }
        }
    }

    async fn fetch_listings(&self, url: &str) -> Result<String, ServiceError> {
        if is_http(url) {
            let resp = self.get(url, None, Deadline::Caller).await?;
            let final_url = resp.url().to_string();
            if !is_gzip_name(&final_url) {
                return resp
                    .text()
                    .await
                    .map_err(|e| LiveTvError::http(format!("read {url}"), e).into());
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e))?;
            gunzip(&final_url, bytes).await
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            if !is_gzip_name(path) {
                return self.fetch(url).await;
            }
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e))?;
            gunzip(path, bytes).await
        }
    }

    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
        if is_http(url) {
            self.get(url, None, Deadline::Total)
                .await?
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| http_failure("read", url, e))
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            tokio::fs::read(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e).into())
        }
    }

    async fn send(&self, request: SourceRequest) -> Result<SourceResponse, ServiceError> {
        let method = match request.method {
            RequestMethod::Get => reqwest::Method::GET,
            RequestMethod::Post => reqwest::Method::POST,
            RequestMethod::Put => reqwest::Method::PUT,
        };
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(json) = request.json {
            builder = builder
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/json; charset=utf-8",
                )
                .body(json);
        }
        let url = &request.url;
        // Bounded in full: a headers-only request still waits for its headers
        // within the timeout, and for a refusal's body within what is left.
        let resp = self.send_within(builder, url, Deadline::Total).await?;
        let status = resp.status().as_u16();
        if request.headers_only && resp.status().is_success() {
            return Ok(SourceResponse {
                status,
                body: Vec::new(),
            });
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| http_failure("read", url, e))?;
        Ok(SourceResponse {
            status,
            body: body.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ReqwestFetcher, SourceFetcher};
    use ferrofin_traits::error::ServiceError;

    #[tokio::test]
    async fn reads_a_local_file_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("ferrofin_livetv_fetch_test.m3u");
        tokio::fs::write(&path, "#EXTM3U\n").await.expect("write");
        let body = ReqwestFetcher::new()
            .fetch(path.to_str().unwrap())
            .await
            .expect("fetch");
        assert_eq!(body, "#EXTM3U\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let err = ReqwestFetcher::new()
            .fetch("/nonexistent/ferrofin/livetv/nope.xml")
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn reads_bytes_from_a_local_file_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("ferrofin_livetv_fetch_bytes_test.json");
        tokio::fs::write(&path, b"[{\"shortName\":\"USA\"}]")
            .await
            .expect("write");
        let body = ReqwestFetcher::new()
            .fetch_bytes(path.to_str().unwrap())
            .await
            .expect("fetch_bytes");
        assert_eq!(body, b"[{\"shortName\":\"USA\"}]");
        let _ = tokio::fs::remove_file(&path).await;

        let err = ReqwestFetcher::new()
            .fetch_bytes("/nonexistent/ferrofin/livetv/nope.json")
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn default_fetch_bytes_delegates_to_fetch() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        let bytes = TextOnly.fetch_bytes("x").await.expect("fetch_bytes");
        assert_eq!(bytes, b"body of x");
    }

    /// One-shot HTTP server that answers with `200 #EXTM3U` and reports the
    /// `User-Agent` header it received.
    fn serve_once() -> (String, std::sync::mpsc::Receiver<Option<String>>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/list.m3u", listener.local_addr().expect("addr"));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0_u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let agent = request.lines().find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                        .map(|(_, value)| value.trim().to_owned())
                });
                let _ = sender.send(agent);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n#EXTM3U",
                );
            }
        });
        (url, receiver)
    }

    /// `M3uParser.GetListingsStream` sends the tuner's configured user agent;
    /// without one (or with a blank one) the client's own product agent goes.
    #[tokio::test]
    async fn a_configured_user_agent_is_sent_with_the_playlist_request() {
        for (configured, expected) in [
            (Some(" VLC/3.0 "), Some("VLC/3.0".to_owned())),
            (None, Some(super::USER_AGENT.to_owned())),
            (Some("   "), Some(super::USER_AGENT.to_owned())),
            // Not a valid header value: dropped, like `TryParseAdd`.
            (Some("bad\nagent"), Some(super::USER_AGENT.to_owned())),
        ] {
            let (url, received) = serve_once();
            let body = ReqwestFetcher::new()
                .fetch_as(&url, configured)
                .await
                .expect("fetch");
            assert_eq!(body, "#EXTM3U");
            assert_eq!(received.recv().expect("agent"), expected, "{configured:?}");
        }
    }

    #[tokio::test]
    async fn the_default_fetch_as_ignores_the_agent_and_a_file_needs_none() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        assert_eq!(
            TextOnly.fetch_as("x", Some("agent")).await.expect("fetch"),
            "body of x"
        );
        let path = std::env::temp_dir().join("ferrofin_livetv_fetch_as_test.m3u");
        tokio::fs::write(&path, "#EXTM3U\n").await.expect("write");
        let body = ReqwestFetcher::new()
            .fetch_as(path.to_str().unwrap(), Some("agent"))
            .await
            .expect("fetch");
        assert_eq!(body, "#EXTM3U\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// `UnzipIfNeededAndCopy`'s test: the extension, query stripped, is `.gz`
    /// or `.gzip` in any case.
    #[rstest::rstest]
    #[case("/epg/guide.xml.gz", true)]
    #[case("GUIDE.XML.GZIP", true)]
    #[case("http://host/epg.gz?token=1", true)]
    #[case("http://host/guide.xml", false)]
    #[case("http://host/guide.xml?name=a.gz", false)]
    #[case("http://host/epg.tgz", false)]
    fn gzip_is_decided_by_the_extension(#[case] name: &str, #[case] gzipped: bool) {
        assert_eq!(super::is_gzip_name(name), gzipped);
    }

    fn gzipped(text: &str) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(text.as_bytes()).expect("compress");
        encoder.finish().expect("finish")
    }

    /// A guide gzipped as several concatenated members (as some EPG builders
    /// write them) is read in full, as `GZipStream` reads it.
    #[tokio::test]
    async fn every_gzip_member_is_read() {
        let dir = tempfile::tempdir().expect("tmp");
        let gz = dir.path().join("guide.xml.gz");
        let mut members = gzipped("<tv>");
        members.extend(gzipped("</tv>"));
        tokio::fs::write(&gz, members).await.expect("write");
        assert_eq!(
            ReqwestFetcher::new()
                .fetch_listings(gz.to_str().unwrap())
                .await
                .expect("gz"),
            "<tv></tv>"
        );
    }

    /// A local `.xml.gz` guide is read decompressed; a plain one as it is.
    #[tokio::test]
    async fn a_local_gzipped_guide_is_decompressed() {
        let dir = tempfile::tempdir().expect("tmp");
        let gz = dir.path().join("guide.xml.gz");
        tokio::fs::write(&gz, gzipped("<tv></tv>"))
            .await
            .expect("write");
        let fetcher = ReqwestFetcher::new();
        assert_eq!(
            fetcher
                .fetch_listings(gz.to_str().unwrap())
                .await
                .expect("gz"),
            "<tv></tv>"
        );
        let plain = dir.path().join("guide.xml");
        tokio::fs::write(&plain, "<tv/>").await.expect("write");
        assert_eq!(
            fetcher
                .fetch_listings(plain.to_str().unwrap())
                .await
                .expect("plain"),
            "<tv/>"
        );
        // Not gzip after all: an extraction error, not a garbled guide.
        let fake = dir.path().join("fake.xml.gz");
        tokio::fs::write(&fake, "<tv/>").await.expect("write");
        assert!(
            fetcher
                .fetch_listings(fake.to_str().unwrap())
                .await
                .is_err()
        );
    }

    /// Answers each connection in turn with the next canned response.
    fn serve_sequence(responses: Vec<Vec<u8>>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for response in responses {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer);
                    let _ = stream.write_all(&response);
                }
            }
        });
        base
    }

    /// `UnzipIfNeededAndCopy` decides on the URL after redirects: a feed URL
    /// without an extension that redirects to `guide.xml.gz` is decompressed.
    #[tokio::test]
    async fn a_redirect_to_a_gzipped_guide_is_decompressed() {
        let body = gzipped("<tv><channel id=\"a\"/></tv>");
        let mut ok = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        ok.extend_from_slice(&body);
        let base = serve_sequence(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /guide.xml.gz\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ok,
        ]);
        let text = ReqwestFetcher::new()
            .fetch_listings(&format!("{base}/epg?id=1"))
            .await
            .expect("fetch");
        assert_eq!(text, "<tv><channel id=\"a\"/></tv>");
    }

    #[tokio::test]
    async fn the_default_fetch_listings_delegates_to_fetch() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        assert_eq!(
            TextOnly.fetch_listings("x.gz").await.expect("fetch"),
            "body of x.gz"
        );
    }

    /// One-shot HTTP server that answers `response` and hands back the raw
    /// request it read (request line, headers and body).
    fn serve_recording(response: &'static [u8]) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                // Read until the declared body has arrived.
                loop {
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        if read == 0 {
                            break;
                        }
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if body.len() >= length || read == 0 {
                        break;
                    }
                }
                let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
                let _ = stream.write_all(response);
            }
        });
        (base, receiver)
    }

    /// `SchedulesDirect.Request<T>` reads the error code out of a failed
    /// response's body, so `send` answers a `4xx` with its body instead of
    /// failing, and sends the method, the headers and the JSON body it was given.
    #[tokio::test]
    async fn send_delivers_method_headers_and_json_and_keeps_an_error_body() {
        let (base, received) = serve_recording(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"code\":4003}",
        );
        let response = ReqwestFetcher::new()
            .send(
                super::SourceRequest::new(super::RequestMethod::Post, format!("{base}/token"))
                    .header("token", "abc")
                    .json(br#"{"username":"u"}"#.to_vec()),
            )
            .await
            .expect("send");
        assert_eq!(response.status, 400);
        assert!(!response.is_success());
        assert_eq!(response.body, br#"{"code":4003}"#);

        let request = received.recv().expect("request");
        assert!(request.starts_with("POST /token HTTP/1.1\r\n"), "{request}");
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("\r\ntoken: abc\r\n"), "{request}");
        assert!(
            lower.contains("\r\ncontent-type: application/json; charset=utf-8\r\n"),
            "{request}"
        );
        assert!(
            request.ends_with("\r\n\r\n{\"username\":\"u\"}"),
            "{request}"
        );
    }

    /// A text-only fake serves a plain `GET` through `fetch_with_status` and
    /// refuses what it cannot send rather than silently dropping the body.
    #[tokio::test]
    async fn the_default_send_serves_only_a_plain_get() {
        struct TextOnly;
        #[async_trait::async_trait]
        impl SourceFetcher for TextOnly {
            async fn fetch(
                &self,
                url: &str,
            ) -> Result<String, ferrofin_traits::error::ServiceError> {
                Ok(format!("body of {url}"))
            }
        }
        let got = TextOnly
            .send(super::SourceRequest::new(super::RequestMethod::Get, "x"))
            .await
            .expect("get");
        assert_eq!((got.status, got.body), (200, b"body of x".to_vec()));
        assert!(got_is_success_for(200) && !got_is_success_for(301));

        for request in [
            super::SourceRequest::new(super::RequestMethod::Post, "x"),
            super::SourceRequest::new(super::RequestMethod::Get, "x").header("token", "t"),
            super::SourceRequest::new(super::RequestMethod::Get, "x").json(b"[]".to_vec()),
        ] {
            assert!(TextOnly.send(request).await.is_err());
        }
    }

    /// Header values and the body carry credentials: `Debug` prints neither.
    #[test]
    fn a_requests_debug_hides_header_values_and_the_body() {
        let request = super::SourceRequest::new(super::RequestMethod::Post, "https://sd/token")
            .header("token", "s3cr3t-token")
            .json(br#"{"password":"hash"}"#.to_vec());
        let printed = format!("{request:?}");
        assert!(
            printed.contains("token") && printed.contains("https://sd/token"),
            "{printed}"
        );
        assert!(
            !printed.contains("s3cr3t-token") && !printed.contains("hash"),
            "{printed}"
        );
    }

    fn got_is_success_for(status: u16) -> bool {
        super::SourceResponse {
            status,
            body: Vec::new(),
        }
        .is_success()
    }

    #[tokio::test]
    async fn http_fetch_bytes_fails_on_an_unreachable_host() {
        let err = ReqwestFetcher::new()
            .fetch_bytes("http://127.0.0.1:0/countries")
            .await
            .expect_err("connect must fail");
        assert!(
            err.to_string()
                .contains("fetch http://127.0.0.1:0/countries")
        );
    }

    /// The deadline these tests give the fetcher in place of the 100 s.
    const SHORT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

    /// Long enough for every fetch below to finish, short enough that a fetch
    /// with no deadline fails here instead of hanging the run.
    const GUARD: std::time::Duration = std::time::Duration::from_secs(5);

    fn short_timeout_fetcher() -> ReqwestFetcher {
        ReqwestFetcher {
            timeout: SHORT_TIMEOUT,
            ..ReqwestFetcher::new()
        }
    }

    /// A peer that accepts every connection and never answers, holding each
    /// one open.
    async fn silent_peer() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        base
    }

    /// A peer that reads each request, waits `head_delay`, sends `status`
    /// headers announcing `chunks` in full, then sends each chunk `gap` after
    /// the last. A `None` chunk stops sending there and holds the connection
    /// open.
    async fn trickling_peer(
        status: &'static str,
        head_delay: std::time::Duration,
        chunks: &'static [Option<&'static str>],
        gap: std::time::Duration,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 4096];
                    let _ = stream.read(&mut buffer).await;
                    tokio::time::sleep(head_delay).await;
                    let length: usize = chunks.iter().flatten().map(|chunk| chunk.len()).sum();
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                    );
                    if stream.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    for chunk in chunks {
                        tokio::time::sleep(gap).await;
                        let Some(chunk) = chunk else {
                            // Stall: keep the connection open, send nothing more.
                            std::future::pending::<()>().await;
                            return;
                        };
                        if stream.write_all(chunk.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        base
    }

    fn assert_timed_out(result: Result<impl std::fmt::Debug, ServiceError>, url: &str) {
        let err = result.expect_err("a silent source must fail");
        assert!(
            matches!(&err, ServiceError::Backend(why) if *why == format!("Timed out fetching {url}")),
            "{err:?}"
        );
    }

    /// `M3uParser.GetListingsStream`, `HdHomerunHost` and `SchedulesDirect`
    /// send through `NamedClient.Default`, whose `HttpClient.Timeout` gives up
    /// on a source that accepts the connection and never answers.
    #[tokio::test]
    async fn a_silent_source_times_out_on_every_bounded_path() {
        let base = silent_peer().await;
        let fetcher = short_timeout_fetcher();
        let url = format!("{base}/list.m3u");
        let outcome = tokio::time::timeout(GUARD, async {
            assert_timed_out(fetcher.fetch_as(&url, None).await, &url);
            assert_timed_out(fetcher.fetch(&url).await, &url);
            assert_timed_out(fetcher.fetch_with_status(&url).await, &url);
            assert_timed_out(fetcher.fetch_bytes(&url).await, &url);
            assert_timed_out(
                fetcher
                    .send(super::SourceRequest::new(
                        super::RequestMethod::Post,
                        url.clone(),
                    ))
                    .await,
                &url,
            );
        })
        .await;
        assert!(outcome.is_ok(), "a fetch to a silent source never gave up");
    }

    /// The M3U and HDHomeRun requests are `ResponseHeadersRead`: the timeout
    /// stops at the headers, so a body slower than it still arrives whole.
    #[tokio::test]
    async fn a_slow_body_after_prompt_headers_is_read_whole() {
        let base = trickling_peer(
            "200 OK",
            std::time::Duration::ZERO,
            &[
                Some("#EXT"),
                Some("M3U\n"),
                Some("#EXTINF:-1,A\n"),
                Some("http://a\n"),
            ],
            std::time::Duration::from_millis(100),
        )
        .await;
        let fetcher = short_timeout_fetcher();
        let url = format!("{base}/list.m3u");
        let outcome = tokio::time::timeout(GUARD, async {
            assert_eq!(
                fetcher.fetch_as(&url, None).await.expect("m3u"),
                "#EXTM3U\n#EXTINF:-1,A\nhttp://a\n"
            );
            assert_eq!(
                fetcher.fetch_with_status(&url).await.expect("hdhomerun"),
                (200, "#EXTM3U\n#EXTINF:-1,A\nhttp://a\n".to_owned())
            );
        })
        .await;
        assert!(outcome.is_ok(), "the slow body was never read");
    }

    /// XMLTV sets its own `HttpClient.Timeout` (`_downloadTimeout`, which
    /// `XmlTvCache` applies), so the fetcher's timeout bounds neither its
    /// headers nor its body.
    #[tokio::test]
    async fn a_listings_download_is_not_bounded_by_the_fetcher_timeout() {
        let base = trickling_peer(
            "200 OK",
            std::time::Duration::from_millis(300),
            &[Some("<tv>"), Some("</tv>")],
            std::time::Duration::from_millis(150),
        )
        .await;
        let fetcher = short_timeout_fetcher();
        let text =
            tokio::time::timeout(GUARD, fetcher.fetch_listings(&format!("{base}/guide.xml")))
                .await
                .expect("the listings download finished");
        assert_eq!(text.expect("listings"), "<tv></tv>");
    }

    /// Schedules Direct reads its responses whole (`ResponseContentRead`, the
    /// default), so its timeout covers the body: one that stalls after the
    /// headers fails instead of pinning the request.
    #[tokio::test]
    async fn a_schedules_direct_body_that_stalls_times_out() {
        let base = trickling_peer(
            "200 OK",
            std::time::Duration::ZERO,
            &[Some("[{"), None, Some("}]")],
            std::time::Duration::from_millis(50),
        )
        .await;
        let fetcher = short_timeout_fetcher();
        let url = format!("{base}/available/countries");
        let outcome = tokio::time::timeout(GUARD, async {
            assert_timed_out(fetcher.fetch_bytes(&url).await, &url);
            assert_timed_out(
                fetcher
                    .send(super::SourceRequest::new(
                        super::RequestMethod::Get,
                        url.clone(),
                    ))
                    .await,
                &url,
            );
        })
        .await;
        assert!(
            outcome.is_ok(),
            "a stalled Schedules Direct body never gave up"
        );
    }

    /// `AddLineupToAccount` sends `ResponseHeadersRead` and reads the body only
    /// of a refusal, to log it: a success whose body stalls answers at its
    /// headers, and a refusal's body is read, within the timeout.
    #[tokio::test]
    async fn a_headers_only_request_reads_only_a_refusals_body() {
        let fetcher = short_timeout_fetcher();
        let put = |base: &str| {
            super::SourceRequest::new(super::RequestMethod::Put, format!("{base}/lineups/X"))
                .headers_only()
        };
        let gap = std::time::Duration::from_millis(50);
        let stalled_success = trickling_peer(
            "200 OK",
            std::time::Duration::ZERO,
            &[Some("{"), None, Some("}")],
            gap,
        )
        .await;
        let refusal = trickling_peer(
            "400 Bad Request",
            std::time::Duration::ZERO,
            &[Some("{\"code\":2100}")],
            gap,
        )
        .await;
        let stalled_refusal = trickling_peer(
            "400 Bad Request",
            std::time::Duration::ZERO,
            &[Some("{"), None, Some("}")],
            gap,
        )
        .await;
        let outcome = tokio::time::timeout(GUARD, async {
            let added = fetcher.send(put(&stalled_success)).await.expect("added");
            assert_eq!((added.status, added.body), (200, Vec::new()));
            let refused = fetcher.send(put(&refusal)).await.expect("refused");
            assert_eq!(
                (refused.status, refused.body),
                (400, br#"{"code":2100}"#.to_vec())
            );
            let url = format!("{stalled_refusal}/lineups/X");
            assert_timed_out(fetcher.send(put(&stalled_refusal)).await, &url);
        })
        .await;
        assert!(outcome.is_ok(), "a headers-only request never answered");
    }
}
