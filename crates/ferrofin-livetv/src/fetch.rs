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

    /// Opens `url` and answers its status and its body, unread, whatever the
    /// status: what `HttpCompletionOption.ResponseHeadersRead` and
    /// `ReadAsStreamAsync` (or `AsyncFile.OpenRead` for a local path) hand
    /// upstream's M3U and HDHomeRun readers, which parse the body as it
    /// arrives instead of holding it whole (v12.2 M3uParser.cs:48-70,
    /// HdHomerunHost.cs:77-78,124-128).
    ///
    /// `user_agent`, when given, is presented with the request
    /// (`M3uParser.GetListingsStream`: `requestMessage.Headers.UserAgent.
    /// TryParseAdd(info.UserAgent)` for a non-empty configured value, so a
    /// playlist host that only serves a particular client still answers).
    ///
    /// The caller reads the status: the HDHomeRun host needs the code itself,
    /// not just success-or-not, because `HdHomerunHost.GetModelInfo` treats a
    /// **404** as "this is an HDHR4, which has no `discover.json`" and answers
    /// with a synthetic model rather than an error (v12.2
    /// HdHomerunHost.cs:135-157), while every other failure propagates.
    ///
    /// The default ignores the agent and serves what [`fetch`](Self::fetch)
    /// returns as one chunk with status `200`, so a text-only fake keeps
    /// working; the real fetcher streams the response or the file.
    async fn open(&self, url: &str, user_agent: Option<&str>) -> Result<SourceBody, ServiceError> {
        let _ = user_agent;
        let body = self.fetch(url).await?;
        Ok(SourceBody::from_bytes(200, body.into_bytes()))
    }

    /// Fetches `url` and returns its body as raw bytes — for documents the
    /// server passes through untouched (the Schedules Direct country list).
    ///
    /// The default goes through [`fetch`](Self::fetch), so a text-only fake
    /// serves both; the real fetcher reads the response bytes directly.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
        self.fetch(url).await.map(String::into_bytes)
    }

    /// Opens an XMLTV listings document and answers its bytes as they arrive,
    /// decompressed when the source is gzipped. Port of `GetXml`'s download
    /// and `UnzipIfNeededAndCopy` (v12.2 XmlTvListingsProvider.cs:100-123,
    /// 175-202), which stream the response or the file into the cache; a
    /// source is gzipped when the URL after redirects, or the path, with any
    /// query stripped ends in `.gz` or `.gzip`. A failure status is an error
    /// (`EnsureSuccessStatusCode`).
    ///
    /// The default is [`open`](Self::open) with that status check, so a
    /// text-only fake keeps working; the real fetcher decompresses.
    async fn fetch_listings(&self, url: &str) -> Result<Box<dyn BodyChunks>, ServiceError> {
        let opened = self.open(url, None).await?;
        if !opened.is_success() {
            return Err(ServiceError::backend(format!(
                "fetch {url} answered {}",
                opened.status
            )));
        }
        Ok(opened.body)
    }

    /// Sends `request` and answers its status and body, whatever the status.
    ///
    /// The Schedules Direct API needs more than a `GET`: a `POST`/`PUT` with a
    /// JSON body and a `token` header, and the error code a failed response
    /// carries in its body (`SchedulesDirect.Request<T>`), which a fetch that
    /// fails on a non-success status would throw away.
    ///
    /// The default serves a header-less `GET` through [`open`](Self::open),
    /// so a text-only fake keeps working, and refuses anything else; the real
    /// fetcher sends it all.
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
        let opened = self.open(&request.url, None).await?;
        let status = opened.status;
        Ok(SourceResponse {
            status,
            body: opened.into_bytes().await?,
        })
    }
}

/// A source's body, read a chunk at a time as it arrives.
#[async_trait]
pub trait BodyChunks: Send {
    /// The body's next bytes, or `None` at its end.
    ///
    /// # Errors
    ///
    /// Fails when the transport or the file read fails.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError>;
}

/// A source [`SourceFetcher::open`] opened: its status, and its body, unread.
pub struct SourceBody {
    /// The HTTP status code; `200` for a local file.
    pub status: u16,
    /// The body, unread.
    pub body: Box<dyn BodyChunks>,
}

impl std::fmt::Debug for SourceBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceBody")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// A body already in memory, served as one chunk.
struct InMemoryBody(Option<Vec<u8>>);

#[async_trait]
impl BodyChunks for InMemoryBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(self.0.take().filter(|bytes| !bytes.is_empty()))
    }
}

impl SourceBody {
    /// A source answering `status` with `bytes`, already in memory.
    #[must_use]
    pub fn from_bytes(status: u16, bytes: Vec<u8>) -> Self {
        Self {
            status,
            body: Box::new(InMemoryBody(Some(bytes))),
        }
    }

    /// Whether the status is `2xx` (`HttpResponseMessage.IsSuccessStatusCode`).
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Reads the whole body into memory. Only for a body that is small by
    /// contract; a document that can be large is read as it arrives.
    ///
    /// # Errors
    ///
    /// Fails when reading the body fails.
    pub async fn into_bytes(mut self) -> Result<Vec<u8>, ServiceError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = self.body.next_chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// Reads the body as one JSON value while it arrives, as
    /// `HttpContentJsonExtensions.ReadFromJsonAsync` does upstream: a UTF-8
    /// byte-order mark is skipped, the parse fails at the first byte that
    /// cannot continue the value (so a URL serving a binary stream fails at
    /// once instead of being buffered for ever), and only the parsed value is
    /// held.
    ///
    /// The parser reads on the blocking pool; if the caller stops waiting,
    /// the read ends at the next chunk.
    ///
    /// # Errors
    ///
    /// Fails when the body is not one JSON value of type `T`, or reading it
    /// fails (an I/O error then).
    pub async fn read_json<T>(self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        // Dropped with this future, which ends a read nobody waits for.
        let (_waiting, abandoned) = tokio::sync::oneshot::channel::<()>();
        let reader = BlockingBody {
            body: self.body,
            runtime: tokio::runtime::Handle::current(),
            abandoned,
            chunk: Vec::new(),
            at: 0,
            started: false,
        };
        tokio::task::spawn_blocking(move || serde_json::from_reader(reader))
            .await
            .map_err(|e| serde_json::Error::io(std::io::Error::other(e)))?
    }
}

/// A body read synchronously on the blocking pool, for `serde_json`'s
/// `Read`-based parser: each chunk is awaited on the runtime.
struct BlockingBody {
    /// The body.
    body: Box<dyn BodyChunks>,
    /// The runtime the body's I/O runs on.
    runtime: tokio::runtime::Handle,
    /// Resolves once the caller stopped waiting for the parse.
    abandoned: tokio::sync::oneshot::Receiver<()>,
    /// The chunk being read.
    chunk: Vec<u8>,
    /// How much of `chunk` has been read.
    at: usize,
    /// Whether the start of the body (and a byte-order mark) has been read.
    started: bool,
}

/// The UTF-8 byte-order mark.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

impl BlockingBody {
    /// Waits for the next chunk into `chunk`; `false` at the end of the body.
    fn next(&mut self) -> std::io::Result<bool> {
        let Self {
            body,
            runtime,
            abandoned,
            ..
        } = self;
        let next = runtime.block_on(async {
            tokio::select! {
                chunk = body.next_chunk() => Some(chunk),
                _ = abandoned => None,
            }
        });
        match next {
            None => Err(std::io::Error::other("nobody waits for this body any more")),
            Some(Err(e)) => Err(std::io::Error::other(e)),
            Some(Ok(None)) => Ok(false),
            Some(Ok(Some(chunk))) => {
                self.chunk = chunk;
                self.at = 0;
                Ok(true)
            }
        }
    }

    /// Reads enough of the body's start to see a UTF-8 byte-order mark, and
    /// skips one.
    fn start(&mut self) -> std::io::Result<()> {
        self.started = true;
        let mut start = Vec::new();
        while start.len() < UTF8_BOM.len() && self.next()? {
            start.extend_from_slice(&self.chunk);
        }
        self.at = if start.starts_with(UTF8_BOM) {
            UTF8_BOM.len()
        } else {
            0
        };
        self.chunk = start;
        Ok(())
    }
}

impl std::io::Read for BlockingBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.started {
            self.start()?;
        }
        while self.at == self.chunk.len() {
            if !self.next()? {
                return Ok(0);
            }
        }
        let read = buf.len().min(self.chunk.len() - self.at);
        buf[..read].copy_from_slice(&self.chunk[self.at..self.at + read]);
        self.at += read;
        Ok(read)
    }
}

/// A response body, read as it arrives.
struct HttpBody {
    /// The response, headers read.
    response: reqwest::Response,
    /// The URL, for errors.
    url: String,
}

#[async_trait]
impl BodyChunks for HttpBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        self.response
            .chunk()
            .await
            .map(|chunk| chunk.map(Vec::from))
            .map_err(|e| http_failure("read", &self.url, e))
    }
}

/// A local file, read a buffer at a time.
struct FileBody {
    /// The open file.
    reader: tokio::io::BufReader<tokio::fs::File>,
    /// The path, for errors.
    path: String,
}

#[async_trait]
impl BodyChunks for FileBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        use tokio::io::AsyncBufReadExt as _;
        let chunk = self
            .reader
            .fill_buf()
            .await
            .map_err(|e| LiveTvError::io(format!("read {}", self.path), e))?
            .to_vec();
        self.reader.consume(chunk.len());
        Ok(Some(chunk).filter(|chunk| !chunk.is_empty()))
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
    /// `TryParseAdd` drops it), failing on transport errors but answering
    /// any status.
    async fn request(
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
        self.send_within(request, url, deadline).await
    }

    /// Performs the `GET` within `deadline`, failing on transport errors and
    /// non-success statuses.
    async fn get(&self, url: &str, deadline: Deadline) -> Result<reqwest::Response, ServiceError> {
        let resp = self.request(url, None, deadline).await?;
        Ok(resp
            .error_for_status()
            .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?)
    }
}

/// Whether `url` names a remote source rather than a local path, by upstream's
/// rule everywhere it makes that choice: it starts with `http`, ignoring ASCII
/// case (`StartsWith("http", StringComparison.OrdinalIgnoreCase)`, v12.2
/// M3uParser.cs:52, XmlTvListingsProvider.cs:76,343).
pub(crate) fn is_http(url: &str) -> bool {
    url.get(..4)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
}

/// Whether `url` starts with `scheme://` for one of `schemes`, scheme compared
/// ignoring ASCII case. Stricter than [`is_http`], which only compares the first
/// four characters.
pub(crate) fn has_scheme(url: &str, schemes: &[&str]) -> bool {
    url.split_once("://").is_some_and(|(scheme, _)| {
        schemes
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    })
}

/// `error` as a `404` when it is a local file that does not exist, the way
/// upstream's middleware answers the `FileNotFoundException` or
/// `DirectoryNotFoundException` that `AsyncFile.OpenRead` throws
/// (`ExceptionMiddleware.GetStatusCode`); any other error is returned as it is.
pub(crate) fn missing_file_is_not_found(error: ServiceError) -> ServiceError {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(current) = cause {
        if current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        {
            return ServiceError::not_found(error.to_string());
        }
        cause = current.source();
    }
    error
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

/// How much compressed input [`GunzipBody`] hands its decoder at once. Deflate
/// expands at most about 1032 to 1, so the text one call answers stays near a
/// megabyte however the transport chunks the body.
const GUNZIP_SLICE: usize = 1024;

/// A gzipped body, decompressed as it arrives. Every gzip member is read, as
/// .NET's `GZipStream` reads concatenated members.
struct GunzipBody {
    /// The compressed body.
    compressed: Box<dyn BodyChunks>,
    /// The compressed chunk being decompressed; `pending[at..]` is not fed yet.
    pending: Vec<u8>,
    /// How much of `pending` the decoder has had.
    at: usize,
    /// The decoder, writing what it extracts into its `Vec`; `None` once the
    /// body has ended.
    decoder: Option<flate2::write::MultiGzDecoder<Vec<u8>>>,
    /// The URL after redirects, or the path, for errors.
    name: String,
}

impl GunzipBody {
    fn new(compressed: Box<dyn BodyChunks>, name: String) -> Self {
        Self {
            compressed,
            pending: Vec::new(),
            at: 0,
            decoder: Some(flate2::write::MultiGzDecoder::new(Vec::new())),
            name,
        }
    }
}

#[async_trait]
impl BodyChunks for GunzipBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        use std::io::Write as _;
        loop {
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(None);
            };
            let ended = if self.at < self.pending.len() {
                let end = self.pending.len().min(self.at + GUNZIP_SLICE);
                decoder
                    .write_all(&self.pending[self.at..end])
                    .and_then(|()| decoder.flush())
                    .map_err(|e| LiveTvError::io(format!("extract gz {}", self.name), e))?;
                self.at = end;
                false
            } else if let Some(chunk) = self.compressed.next_chunk().await? {
                self.pending = chunk;
                self.at = 0;
                continue;
            } else {
                decoder
                    .try_finish()
                    .map_err(|e| LiveTvError::io(format!("extract gz {}", self.name), e))?;
                true
            };
            let out = std::mem::take(decoder.get_mut());
            if ended {
                self.decoder = None;
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

#[async_trait]
impl SourceFetcher for ReqwestFetcher {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        if is_http(url) {
            self.get(url, Deadline::Headers)
                .await?
                .text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            tokio::fs::read_to_string(url)
                .await
                .map_err(|e| LiveTvError::io(format!("read {url}"), e).into())
        }
    }

    async fn open(&self, url: &str, user_agent: Option<&str>) -> Result<SourceBody, ServiceError> {
        if is_http(url) {
            // `ResponseHeadersRead`: the timeout stops at the headers, and the
            // body is read as the caller asks for it.
            let response = self.request(url, user_agent, Deadline::Headers).await?;
            Ok(SourceBody {
                status: response.status().as_u16(),
                body: Box::new(HttpBody {
                    response,
                    url: url.to_owned(),
                }),
            })
        } else {
            // `AsyncFile.OpenRead`: a file that cannot be opened is an error,
            // as upstream's `FileNotFoundException` is, not a status. The value
            // is the path as given: `file:///x` is a relative path there too.
            let file = tokio::fs::File::open(url)
                .await
                .map_err(|e| LiveTvError::io(format!("read {url}"), e))?;
            // `AsyncFile.OpenRead` refuses a directory (`UnauthorizedAccessException`),
            // where a Unix `open` succeeds and only the first read fails.
            if file.metadata().await.is_ok_and(|meta| meta.is_dir()) {
                return Err(LiveTvError::io(
                    format!("read {url}"),
                    std::io::Error::new(std::io::ErrorKind::IsADirectory, "is a directory"),
                )
                .into());
            }
            Ok(SourceBody {
                status: 200,
                body: Box::new(FileBody {
                    reader: tokio::io::BufReader::new(file),
                    path: url.to_owned(),
                }),
            })
        }
    }

    async fn fetch_listings(&self, url: &str) -> Result<Box<dyn BodyChunks>, ServiceError> {
        let (name, body): (String, Box<dyn BodyChunks>) = if is_http(url) {
            let response = self.get(url, Deadline::Caller).await?;
            let final_url = response.url().to_string();
            let body = Box::new(HttpBody {
                response,
                url: url.to_owned(),
            });
            (final_url, body)
        } else {
            (url.to_owned(), self.open(url, None).await?.body)
        };
        Ok(if is_gzip_name(&name) {
            Box::new(GunzipBody::new(body, name))
        } else {
            body
        })
    }

    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, ServiceError> {
        if is_http(url) {
            self.get(url, Deadline::Total)
                .await?
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| http_failure("read", url, e))
        } else {
            tokio::fs::read(url)
                .await
                .map_err(|e| LiveTvError::io(format!("read {url}"), e).into())
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

    /// `StartsWith("http", StringComparison.OrdinalIgnoreCase)`: the scheme in
    /// any case is remote; a path, `file://` and anything shorter are not.
    #[rstest::rstest]
    #[case("http://guide/x.xml", true)]
    #[case("HTTP://guide/x.xml", true)]
    #[case("Http://guide/x.xml", true)]
    #[case("https://guide/x.xml", true)]
    #[case("/srv/guide.xml", false)]
    #[case("file:///srv/guide.xml", false)]
    #[case("C:\\guide.xml", false)]
    #[case("htt", false)]
    #[case("", false)]
    #[case("htté://x", false)]
    fn a_source_is_remote_when_it_starts_with_http(#[case] url: &str, #[case] remote: bool) {
        assert_eq!(super::is_http(url), remote, "{url}");
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
            let opened = ReqwestFetcher::new()
                .open(&url, configured)
                .await
                .expect("open");
            assert_eq!(opened.status, 200);
            assert_eq!(opened.into_bytes().await.expect("read"), b"#EXTM3U");
            assert_eq!(received.recv().expect("agent"), expected, "{configured:?}");
        }
    }

    #[tokio::test]
    async fn the_default_open_ignores_the_agent_and_a_file_needs_none() {
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
        let opened = TextOnly.open("x", Some("agent")).await.expect("open");
        assert!(opened.is_success());
        assert_eq!(opened.into_bytes().await.expect("read"), b"body of x");

        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("list.m3u");
        tokio::fs::write(&path, "#EXTM3U\n").await.expect("write");
        let opened = ReqwestFetcher::new()
            .open(path.to_str().unwrap(), Some("agent"))
            .await
            .expect("open");
        assert_eq!(opened.status, 200);
        assert_eq!(opened.into_bytes().await.expect("read"), b"#EXTM3U\n");
    }

    /// `AsyncFile.OpenRead` throws for a file that is not there: an I/O
    /// error naming the path, not a status.
    #[tokio::test]
    async fn opening_a_missing_file_fails() {
        let err = ReqwestFetcher::new()
            .open("/nonexistent/ferrofin/livetv/list.m3u", None)
            .await
            .expect_err("no such file");
        assert!(
            err.to_string()
                .starts_with("read /nonexistent/ferrofin/livetv/list.m3u: "),
            "{err}"
        );
    }

    /// A local file is read a buffer at a time, every byte of it.
    #[tokio::test]
    async fn a_local_file_is_read_in_chunks() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("list.m3u");
        let content: Vec<u8> = (0..100_000_u32).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&path, &content).await.expect("write");
        let mut opened = ReqwestFetcher::new()
            .open(path.to_str().unwrap(), None)
            .await
            .expect("open");
        let mut read = Vec::new();
        let mut chunks = 0;
        while let Some(chunk) = opened.body.next_chunk().await.expect("chunk") {
            assert!(!chunk.is_empty(), "{chunk:?}");
            read.extend_from_slice(&chunk);
            chunks += 1;
        }
        assert_eq!(read, content);
        assert!(chunks > 1, "read in one piece");
    }

    /// The real `open` answers any status with its body: the HDHomeRun host
    /// reads a 404 itself.
    #[tokio::test]
    async fn open_answers_a_failure_status_with_its_body() {
        let base = serve_sequence(vec![
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone"
                .to_vec(),
        ]);
        let opened = ReqwestFetcher::new()
            .open(&format!("{base}/discover.json"), None)
            .await
            .expect("open");
        assert_eq!(opened.status, 404);
        assert!(!opened.is_success());
        assert_eq!(opened.into_bytes().await.expect("read"), b"gone");
    }

    /// A body served in pieces (`stream` cut at each `|`; a `!` piece is a
    /// transport failure), then, when `stall`, never ending; it marks
    /// `dropped` when dropped.
    struct Pieces {
        queue: std::collections::VecDeque<Vec<u8>>,
        stall: bool,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Pieces {
        fn new(stream: &[u8], stall: bool) -> Self {
            Self {
                queue: stream
                    .split(|&b| b == b'|')
                    .filter(|piece| !piece.is_empty())
                    .map(<[u8]>::to_vec)
                    .collect(),
                stall,
                dropped: std::sync::Arc::default(),
            }
        }
    }

    impl Drop for Pieces {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl super::BodyChunks for Pieces {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
            match self.queue.pop_front() {
                Some(piece) if piece == b"!" => Err(ServiceError::backend("connection reset")),
                Some(piece) => Ok(Some(piece)),
                None if self.stall => std::future::pending().await,
                None => Ok(None),
            }
        }
    }

    fn pieces(stream: &[u8]) -> super::SourceBody {
        super::SourceBody {
            status: 200,
            body: Box::new(Pieces::new(stream, false)),
        }
    }

    /// `ReadFromJsonAsync`: the value is parsed across chunks, a UTF-8
    /// byte-order mark (even one cut by a chunk) is skipped, and anything
    /// after the value but whitespace is an error.
    #[rstest::rstest]
    #[case::whole(b"[1,2]", Some(vec![1, 2]))]
    #[case::cut(b"[1|,||2]  ", Some(vec![1, 2]))]
    #[case::bom(b"\xEF\xBB\xBF[1,2]", Some(vec![1, 2]))]
    #[case::bom_cut(b"\xEF|\xBB\xBF[|1,2]", Some(vec![1, 2]))]
    #[case::short(b"[]", Some(vec![]))]
    #[case::bom_alone(b"\xEF\xBB\xBF", None)]
    #[case::empty(b"", None)]
    #[case::not_json(b"G\x00\x00", None)]
    #[case::trailing(b"[1]| x", None)]
    #[case::transport_failure(b"[1,|!", None)]
    #[tokio::test]
    async fn a_json_body_is_read_as_it_arrives(
        #[case] stream: &[u8],
        #[case] expected: Option<Vec<u32>>,
    ) {
        let read = pieces(stream).read_json::<Vec<u32>>().await;
        assert_eq!(read.ok(), expected);
    }

    /// `System.Text.Json` rejects invalid UTF-8 inside a string, so a Latin-1
    /// name in a device's JSON is a parse error, not a U+FFFD.
    #[tokio::test]
    async fn invalid_utf8_in_a_json_string_is_an_error() {
        assert!(
            pieces(b"[\"Caf\xE9\"]")
                .read_json::<Vec<String>>()
                .await
                .is_err()
        );
        assert_eq!(
            pieces(b"[\"Caf\xC3\xA9\"]")
                .read_json::<Vec<String>>()
                .await
                .expect("valid UTF-8"),
            ["Café"]
        );
    }

    /// A transport failure mid-body is an I/O error carrying the cause.
    #[tokio::test]
    async fn a_json_body_that_breaks_is_an_io_error() {
        let err = pieces(b"[1,|!")
            .read_json::<Vec<u32>>()
            .await
            .expect_err("broken");
        assert!(err.is_io(), "{err}");
        assert!(err.to_string().contains("connection reset"), "{err}");
    }

    /// A caller that stops waiting (a refresh cancelled while a device trickles
    /// whitespace) ends the read on the blocking pool too.
    #[tokio::test]
    async fn an_abandoned_json_read_lets_go_of_the_body() {
        let body = Pieces::new(b"[1,", true);
        let dropped = std::sync::Arc::clone(&body.dropped);
        let opened = super::SourceBody {
            status: 200,
            body: Box::new(body),
        };
        let read = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            opened.read_json::<Vec<u32>>(),
        )
        .await;
        assert!(read.is_err(), "the stalled body cannot have parsed");
        let released = tokio::time::timeout(GUARD, async {
            while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(released.is_ok(), "the abandoned read still holds the body");
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

    /// A listings document read whole, for the assertions.
    async fn listings_text(fetcher: &dyn SourceFetcher, url: &str) -> Result<String, ServiceError> {
        let mut body = fetcher.fetch_listings(url).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next_chunk().await? {
            bytes.extend(chunk);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A body served a few bytes at a time.
    struct GzPieces(std::collections::VecDeque<Vec<u8>>);

    #[async_trait::async_trait]
    impl super::BodyChunks for GzPieces {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
            Ok(self.0.pop_front())
        }
    }

    /// `UnzipIfNeededAndCopy` gunzips the stream as it is copied: a gzipped
    /// body that arrives in pieces cut anywhere, members included, becomes
    /// the same document, and nothing is held but the piece in hand.
    #[tokio::test]
    async fn a_gzipped_body_is_decompressed_as_it_arrives() {
        let mut members = gzipped("<tv><channel id=\"a\"/>");
        members.extend(gzipped("</tv>"));
        let pieces = members.chunks(3).map(<[u8]>::to_vec).collect();
        let mut body =
            super::GunzipBody::new(Box::new(GzPieces(pieces)), "guide.xml.gz".to_owned());
        let mut text = Vec::new();
        while let Some(chunk) = super::BodyChunks::next_chunk(&mut body)
            .await
            .expect("gunzip")
        {
            assert!(!chunk.is_empty(), "{chunk:?}");
            text.extend(chunk);
        }
        assert_eq!(text, b"<tv><channel id=\"a\"/></tv>");
        assert!(
            super::BodyChunks::next_chunk(&mut body)
                .await
                .expect("ended")
                .is_none()
        );
    }

    /// A gzip stream cut short is an extraction error, not a shorter guide.
    #[tokio::test]
    async fn a_truncated_gzipped_body_fails() {
        let mut gz = gzipped("<tv><channel id=\"a\"/></tv>");
        gz.truncate(gz.len() - 6);
        let mut body =
            super::GunzipBody::new(Box::new(GzPieces([gz].into())), "guide.xml.gz".to_owned());
        let mut outcome = Ok(Some(Vec::new()));
        while matches!(outcome, Ok(Some(_))) {
            outcome = super::BodyChunks::next_chunk(&mut body).await;
        }
        assert!(outcome.is_err(), "{outcome:?}");
    }

    /// One compressed chunk that expands a thousandfold is handed out a slice
    /// of input at a time, never whole.
    #[tokio::test]
    async fn a_highly_compressed_chunk_is_extracted_in_bounded_pieces() {
        use std::io::Write as _;
        const TEXT_BYTES: usize = 16 * 1024 * 1024;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(&vec![b' '; TEXT_BYTES])
            .expect("compress");
        let gz = encoder.finish().expect("finish");
        let mut body =
            super::GunzipBody::new(Box::new(GzPieces([gz].into())), "guide.xml.gz".to_owned());
        let (mut total, mut largest) = (0, 0);
        while let Some(chunk) = super::BodyChunks::next_chunk(&mut body)
            .await
            .expect("gunzip")
        {
            total += chunk.len();
            largest = largest.max(chunk.len());
        }
        assert_eq!(total, TEXT_BYTES);
        assert!(largest <= 2 * 1024 * 1024, "one piece was {largest} bytes");
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
            listings_text(&ReqwestFetcher::new(), gz.to_str().unwrap())
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
            listings_text(&fetcher, gz.to_str().unwrap())
                .await
                .expect("gz"),
            "<tv></tv>"
        );
        let plain = dir.path().join("guide.xml");
        tokio::fs::write(&plain, "<tv/>").await.expect("write");
        assert_eq!(
            listings_text(&fetcher, plain.to_str().unwrap())
                .await
                .expect("plain"),
            "<tv/>"
        );
        // Not gzip after all: an extraction error, not a garbled guide.
        let fake = dir.path().join("fake.xml.gz");
        tokio::fs::write(&fake, "<tv/>").await.expect("write");
        assert!(
            listings_text(&fetcher, fake.to_str().unwrap())
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
        let text = listings_text(&ReqwestFetcher::new(), &format!("{base}/epg?id=1"))
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
            listings_text(&TextOnly, "x.gz").await.expect("fetch"),
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

    /// A text-only fake serves a plain `GET` through `open` and
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
            assert_timed_out(fetcher.open(&url, None).await, &url);
            assert_timed_out(fetcher.fetch(&url).await, &url);
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
            let opened = fetcher.open(&url, None).await.expect("open");
            assert_eq!(opened.status, 200);
            assert_eq!(
                opened.into_bytes().await.expect("read"),
                b"#EXTM3U\n#EXTINF:-1,A\nhttp://a\n"
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
            tokio::time::timeout(GUARD, listings_text(&fetcher, &format!("{base}/guide.xml")))
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
