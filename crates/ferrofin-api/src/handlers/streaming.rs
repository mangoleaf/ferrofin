//! Shared direct-play (static) file-serving helpers for the streaming
//! controllers (`Videos`, `Audio`, `UniversalAudio`).
//!
//! Every one of those controllers, on its `Static=true` path, resolves the
//! requested [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo) and
//! serves it without ffmpeg: a file with HTTP `Range` support, a remote target
//! by relaying it, an open live stream from its buffer. It is factored here so
//! `videos`/`audio` don't each duplicate it; everything else is transcoded by
//! [`transcode_stream_fallback`].

use std::io::Read as _;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use chrono::{DateTime, Utc};
use ferrofin_model::entities::VideoType;
use ferrofin_traits::filesystem::OpenedFile;
use ferrofin_traits::media_encoding::HlsStreamRequest;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::error::ApiError;
use crate::handlers::hls::transcode_stream_fallback;
use crate::state::AppState;

/// What a static (direct-play) request is served from: the requested media
/// source's path — a file on disk, a remote URL the item stands for (a `.strm`
/// shortcut's target), or an open live stream.
pub(crate) struct StaticSource {
    /// The file path or remote URL the source is read from: its encoder path
    /// when it names one, else its path.
    pub path: String,
    /// The protocol the path is read over.
    pub protocol: ferrofin_model::media_info::MediaProtocol,
    /// The source's video type: a disc folder is never served as a file.
    pub video_type: Option<VideoType>,
    /// Whether the source is an opened live stream, whose shared buffer is
    /// read directly (upstream's `state.DirectStreamProvider`).
    pub is_live_stream: bool,
    /// Whether the source is still being written (`IsInfiniteStream`): a file
    /// that keeps growing while it is read.
    pub is_infinite_stream: bool,
    /// The `User-Agent` the source requires (an IPTV tuner's), sent when a
    /// remote target is relayed.
    pub user_agent: Option<String>,
}

/// Resolves the direct-play source a stream request names.
///
/// The source is chosen by
/// [`get_stream_media_source`](ferrofin_traits::library::MediaSourceManager::get_stream_media_source),
/// as for a transcode: the open live stream, else the version
/// `media_source_id` names, else the item's first. It is read from its encoder
/// path when it names one. Returns `404` when there is no such source, or it
/// has neither an encoder path nor a path (nothing to direct-stream).
pub(crate) async fn static_source(
    state: &AppState,
    item_id: Uuid,
    media_source_id: Option<&str>,
    live_stream_id: Option<&str>,
) -> Result<StaticSource, ApiError> {
    let source = state
        .media_sources
        .get_stream_media_source(item_id, media_source_id, live_stream_id)
        .await?;
    // `EncodingHelper.AttachMediaSourceInfo`: a source that names how it is read
    // (an in-progress recording's `LiveRecordings` URL) is read that way.
    let (path, protocol) = match (source.encoder_path, source.encoder_protocol) {
        (Some(path), Some(protocol)) if !path.is_empty() => (path, protocol),
        _ => (
            source.path.ok_or_else(|| {
                ApiError::NotFound(format!("no direct stream for item {item_id}"))
            })?,
            source.protocol,
        ),
    };
    let user_agent = source
        .required_http_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
        .map(|(_, value)| value.clone());
    Ok(StaticSource {
        path,
        protocol,
        video_type: source.video_type,
        is_live_stream: source.live_stream_id.is_some(),
        is_infinite_stream: source.is_infinite_stream,
        user_agent,
    })
}

/// The shared body of `VideosController.GetVideoStream` and
/// `AudioHelper.GetAudioStream`: a `Static=true` request is served from the
/// source it names; anything else, and a static video request for a disc
/// folder (which only ffmpeg reads), is transcoded.
pub(crate) async fn static_or_transcode(
    state: &AppState,
    item_id: Uuid,
    is_audio: bool,
    req: HlsStreamRequest,
    request: Request,
) -> Result<Response, ApiError> {
    if req.is_static {
        let source = static_source(
            state,
            item_id,
            req.media_source_id.as_deref(),
            req.live_stream_id.as_deref(),
        )
        .await?;
        if is_audio || !matches!(source.video_type, Some(VideoType::BluRay | VideoType::Dvd)) {
            return serve_static_source(state, source, req.output_container.as_deref(), request)
                .await;
        }
    }
    transcode_stream_fallback(state, item_id, is_audio, req, request).await
}

/// Serves a static source: an open shared live stream from its buffer, a file
/// with `Range` support or, while it is still being written, as it grows, an
/// HTTP(S) target by proxying it; anything else is refused.
///
/// Port of the static branches of `VideosController.GetVideoStream` and
/// `AudioHelper.GetAudioStream`: the `DirectStreamProvider` is read as
/// `file.ts`, an HTTP input goes through `GetStaticRemoteStreamResult`, any
/// other non-file protocol is `400 Input protocol … cannot be streamed
/// statically`, and a file with `IsInfiniteStream` is a `ProgressiveFileStream`
/// typed by `output_container` (an in-progress recording names an encoder
/// path, so it is relayed instead).
pub(crate) async fn serve_static_source(
    state: &AppState,
    source: StaticSource,
    output_container: Option<&str>,
    request: Request,
) -> Result<Response, ApiError> {
    use ferrofin_model::media_info::MediaProtocol;
    if source.is_live_stream
        && let Some(unique_id) = live_buffer_id(&source.path)
    {
        return crate::handlers::live_tv::live_stream_buffer(state, unique_id, "file.ts").await;
    }
    match source.protocol {
        MediaProtocol::File if source.is_infinite_stream => {
            Ok(crate::handlers::live_tv::progressive_response(
                std::path::PathBuf::from(&source.path),
                0,
                infinite_stream_content_type(output_container, &source.path),
            ))
        }
        MediaProtocol::File => serve_static_file(&source.path, request).await,
        MediaProtocol::Http => {
            serve_remote_static(
                &source.path,
                source.user_agent.as_deref(),
                request,
                ferrofin_util::http_client::HTTP_CLIENT_DEFAULT_TIMEOUT,
            )
            .await
        }
        other => Err(ApiError::BadRequest(format!(
            "Input protocol {other:?} cannot be streamed statically"
        ))),
    }
}

/// The type of a file stream still being written: `state.GetMimeType("." +
/// state.OutputContainer, false) ?? state.GetMimeType(state.MediaPath)`, where
/// the container of a request naming none is `ts`, as
/// `StreamingHelpers.GetStreamingState` sets it for an infinite source.
fn infinite_stream_content_type(output_container: Option<&str>, path: &str) -> &'static str {
    use ferrofin_model::net::mime_types::{get_mime_type, get_mime_type_or};
    let container = output_container.filter(|c| !c.is_empty()).unwrap_or("ts");
    get_mime_type_or(&format!(".{container}"), None).unwrap_or_else(|| get_mime_type(path))
}

/// The unique id of the shared buffer a live source reads, from the
/// `…/LiveTv/LiveStreamFiles/{uniqueId}/stream.ts` path the Live TV manager
/// gives a shared stream. A pass-through stream keeps its tuner URL: `None`.
fn live_buffer_id(path: &str) -> Option<&str> {
    path.split_once("/LiveTv/LiveStreamFiles/")?
        .1
        .split('/')
        .next()
        .filter(|id| !id.is_empty())
}

/// `FileStreamResponseHelpers.GetStaticRemoteStreamResult`: relays the remote
/// target's body to the client, sending the source's required `User-Agent` and
/// forwarding the client's `Range`, and answering with the upstream status,
/// `Accept-Ranges` (`none` when the upstream does not offer ranges),
/// `Content-Range`, `Content-Length` and `Content-Type`
/// (`application/octet-stream` when it names none).
///
/// The request is `ResponseHeadersRead` through `NamedClient.Default`, so its
/// `HttpClient.Timeout` (`headers_timeout`) bounds it only up to the response
/// headers: an origin that never answers is a `500`, and a long body is
/// relayed with no limit.
async fn serve_remote_static(
    url: &str,
    user_agent: Option<&str>,
    request: Request,
    headers_timeout: std::time::Duration,
) -> Result<Response, ApiError> {
    use axum::http::{HeaderValue, StatusCode, header};
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    // `NamedClient.Default` always names the server; a source's required agent
    // replaces it per request (reqwest fills only headers a request leaves unset).
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("Ferrofin/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    });
    let method = if request.method() == axum::http::Method::HEAD {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };
    let mut upstream = client.request(method, url);
    if let Some(user_agent) = user_agent {
        upstream = upstream.header(header::USER_AGENT, user_agent);
    }
    if let Some(range) = request
        .headers()
        .get(header::RANGE)
        .filter(|range| !range.is_empty())
    {
        upstream = upstream.header(header::RANGE, range.as_bytes());
    }
    let response = tokio::time::timeout(headers_timeout, upstream.send())
        .await
        .map_err(|_| {
            ApiError::Service(ferrofin_traits::error::ServiceError::backend(format!(
                "remote stream request timed out: no response headers within {}s",
                headers_timeout.as_secs_f64()
            )))
        })?
        .map_err(|error| {
            ApiError::Service(ferrofin_traits::error::ServiceError::backend(format!(
                "remote stream request to {} failed: {}",
                ferrofin_util::source_url::redact_url(url),
                error.without_url()
            )))
        })?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers();
    let accept_ranges = headers.get(header::ACCEPT_RANGES).cloned();
    let supports_range = status == StatusCode::PARTIAL_CONTENT
        || accept_ranges
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("bytes"));
    let mut builder = Response::builder().status(status).header(
        header::ACCEPT_RANGES,
        accept_ranges.unwrap_or_else(|| {
            HeaderValue::from_static(if supports_range { "bytes" } else { "none" })
        }),
    );
    for name in [header::CONTENT_RANGE, header::CONTENT_LENGTH] {
        if let Some(value) = headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    builder = builder.header(
        header::CONTENT_TYPE,
        headers
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream")),
    );
    let body = Body::from_stream(response.bytes_stream());
    builder
        .body(body)
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}

/// Serves the file at `path` for `request` **with range processing disabled**.
///
/// Mirrors ASP.NET Core's two-argument `PhysicalFile(path, contentType)`, the
/// overload that leaves `FileResult.EnableRangeProcessing` at its `false`
/// default. `FileResultExecutorBase.SetHeadersAndLog` only calls
/// `SetAcceptRangeHeader` inside the `enableRangeProcessing` branch, so such a
/// response advertises no `Accept-Ranges` **and** ignores an inbound `Range`:
/// the client gets `200` with the whole file, never a `206`.
///
/// Reproduced here by stripping `Range`/`If-Range` from the request before
/// [`serve_static_file`] sees them and dropping the `Accept-Ranges` the
/// underlying `ServeFile` adds. Use this only for routes the C# serves through
/// that overload — the trickplay tile is one; direct play and the HLS segments
/// are not, and must keep their range support.
pub(crate) async fn serve_static_file_without_ranges(
    path: &str,
    mut request: Request,
) -> Result<Response, ApiError> {
    request.headers_mut().remove(axum::http::header::RANGE);
    request.headers_mut().remove(axum::http::header::IF_RANGE);
    let mut response = serve_static_file(path, request).await?;
    response
        .headers_mut()
        .remove(axum::http::header::ACCEPT_RANGES);
    Ok(response)
}

/// Serves the file at `path` for `request`, honouring `Range`/`HEAD`.
///
/// Delegates to [`tower_http::services::ServeFile`], which performs the
/// `Range`/`HEAD`/`206 Partial Content`/`404` handling; its infallible response is
/// mapped into an axum body. A resolution/IO failure surfaces as `404`.
pub(crate) async fn serve_static_file(path: &str, request: Request) -> Result<Response, ApiError> {
    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    // A path the database resolved but the filesystem rejects otherwise
    // surfaces as a bare 404 indistinguishable from a missing route — name the
    // fs-level cause (stale NFS handle, permissions, moved file) in the log.
    // Only on the 404 branch: this helper serves every HLS segment and every
    // Range request of a direct play, and an unconditional pre-`stat` doubled
    // the metadata syscalls on that path (two round trips per request on
    // network storage) to produce a diagnostic that never fired. A 206/416 is
    // about the Range header, not the file, so it is not interesting here.
    if response.status() == axum::http::StatusCode::NOT_FOUND {
        let reason = tokio::fs::metadata(path)
            .await
            .err()
            .map_or_else(|| "unreadable".to_owned(), |e| e.to_string());
        tracing::warn!(path, error = reason, "direct-stream file is not accessible");
    }
    Ok(response.map(Body::new))
}

/// How much of a file one blocking-pool hop of [`opened_file_response`] reads.
const FILE_CHUNK_BYTES: u64 = 64 * 1024;

/// The `200` for a file opened through the
/// [`FileSystem`](ferrofin_traits::filesystem::FileSystem) seam, read on the
/// blocking pool a chunk at a time as the client takes it, what
/// `FileStreamResult` and `PhysicalFileResult` send: `Content-Length`, and no
/// `Accept-Ranges` (both leave `EnableRangeProcessing` off).
///
/// The reads take no slot of [`AppState::blocking_file_system`]: the mount
/// answered the open, so a read that hangs is one the mount took down with it,
/// at most one per response under way, and a body must not stall on calls
/// hung on some other mount.
///
/// The body stops at the length it declared. A log still being written is
/// longer by the time the reader gets there, and a body that outran its
/// `Content-Length` would abort the response; upstream's does. One that comes
/// up short (the file was truncated) ends the connection early.
pub(crate) fn opened_file_response(
    file: OpenedFile,
    content_type: &str,
) -> Result<Response, ApiError> {
    let OpenedFile {
        path,
        length,
        reader,
    } = file;
    let unread = (reader.take(length), path);
    let chunks = futures_util::stream::try_unfold(unread, |(mut reader, path)| async move {
        let read = tokio::task::spawn_blocking(move || {
            let mut chunk = Vec::new();
            let read = reader
                .by_ref()
                .take(FILE_CHUNK_BYTES)
                .read_to_end(&mut chunk);
            read.map(|_| (reader, chunk))
        })
        .await;
        let (reader, chunk) = read
            .map_err(std::io::Error::from)
            .and_then(|read| read)
            .inspect_err(|error| {
                tracing::warn!(path, %error, "reading a file for a response failed");
            })?;
        Ok::<_, std::io::Error>((!chunk.is_empty()).then(|| (Bytes::from(chunk), (reader, path))))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from_stream(chunks))
        .map_err(|error| ApiError::BadRequest(error.to_string()))
}

/// `FileResultExecutorBase.GetPreconditionState` for a `PhysicalFile`, which
/// has a last-modified date and no entity tag: the status that answers instead
/// of the file, if any. `last_modified` counts in whole seconds, as the executor
/// rounds it down to, and a date in the future is ignored.
///
/// `If-Unmodified-Since` failing beats everything. An `If-Modified-Since` that
/// says "not modified" holds only when nothing says to go on, and a passing
/// `If-Unmodified-Since` does (`PreconditionState` takes the highest of the
/// two).
pub(crate) fn physical_file_precondition(
    headers: &HeaderMap,
    last_modified: DateTime<Utc>,
) -> Option<StatusCode> {
    let now = Utc::now();
    let date = |name| {
        let value = headers.get(name)?.to_str().ok()?;
        let date = DateTime::<Utc>::from(httpdate::parse_http_date(value).ok()?);
        (date <= now).then_some(date.timestamp())
    };
    let last_modified = last_modified.timestamp();
    match (
        date(header::IF_MODIFIED_SINCE),
        date(header::IF_UNMODIFIED_SINCE),
    ) {
        (_, Some(since)) if since < last_modified => Some(StatusCode::PRECONDITION_FAILED),
        (Some(since), None) if since >= last_modified => Some(StatusCode::NOT_MODIFIED),
        _ => None,
    }
}

/// Runs the response-completion callback when the body terminates, including
/// cancellation. ASP.NET Core v10.0.0 Kestrel HttpProtocol.ProcessRequests calls
/// FireOnCompleted after both normal and aborted responses.
struct ResponseCompletion(
    Option<std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>>,
);
impl Drop for ResponseCompletion {
    fn drop(&mut self) {
        if let Some(observer) = &self.0 {
            observer.complete();
        }
    }
}

/// Counts reads and advances the dynamic segment when its response terminates.
pub(crate) fn track_transcode_body(
    body: Body,
    observer: std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>,
) -> Body {
    use futures_util::StreamExt as _;
    Body::from_stream(futures_util::stream::unfold(
        (
            body.into_data_stream(),
            ResponseCompletion(Some(observer)),
            false,
        ),
        |(mut stream, completion, failed)| async move {
            if failed {
                return None;
            }
            match stream.next().await {
                Some(Ok(bytes)) => {
                    if let Some(observer) = &completion.0 {
                        observer.add_bytes(bytes.len());
                    }
                    Some((Ok::<_, axum::Error>(bytes), (stream, completion, false)))
                }
                Some(Err(error)) => Some((Err(error), (stream, completion, true))),
                None => None,
            }
        },
    ))
}

/// Reads a progressive output while ffmpeg is still extending it.
/// An empty read waits 50ms while the producing process lives, as upstream's
/// ProgressiveFileStream does. A body termination releases its request mark.
pub(crate) async fn serve_growing_transcode(
    path: &str,
    request: Request,
    observer: Option<std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>>,
) -> Result<Response, ApiError> {
    use tokio::io::AsyncReadExt as _;
    let completion = ResponseCompletion(observer);
    if request.method() == axum::http::Method::HEAD {
        let mut response = Response::new(Body::empty());
        response.headers_mut().insert(
            axum::http::header::ACCEPT_RANGES,
            axum::http::HeaderValue::from_static("none"),
        );
        return Ok(response);
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ApiError::NotFound(error.to_string()))?;
    let body = Body::from_stream(futures_util::stream::unfold(
        (file, completion, false),
        |(mut file, completion, failed)| async move {
            if failed {
                return None;
            }
            let mut buffer = vec![0; 32_768];
            loop {
                match file.read(&mut buffer).await {
                    Ok(0)
                        if completion
                            .0
                            .as_ref()
                            .is_some_and(|observer| !observer.has_exited()) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Ok(0) => return None,
                    Ok(length) => {
                        buffer.truncate(length);
                        if let Some(observer) = &completion.0 {
                            observer.add_bytes(length);
                        }
                        return Some((Ok::<_, std::io::Error>(buffer), (file, completion, false)));
                    }
                    Err(error) => return Some((Err(error), (file, completion, true))),
                }
            }
        },
    ));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        axum::http::header::ACCEPT_RANGES,
        axum::http::HeaderValue::from_static("none"),
    );
    Ok(response)
}

#[cfg(test)]
mod remote_static_tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// The headers deadline these tests use in place of the 100 s.
    const SHORT_TIMEOUT: Duration = Duration::from_millis(200);

    /// Long enough for every relay below to finish, short enough that one
    /// with no deadline fails here instead of hanging the run.
    const GUARD: Duration = Duration::from_secs(5);

    /// An origin that accepts one connection, reads the request, then sends
    /// `head` (nothing when it is empty) and each of `chunks` 100 ms apart,
    /// holding the connection open afterwards.
    async fn origin(head: &'static str, chunks: &'static [&'static str]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/media.mkv", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await;
            let _ = stream.write_all(head.as_bytes()).await;
            for chunk in chunks {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        });
        url
    }

    /// `GetStaticRemoteStreamResult` sends through `NamedClient.Default`: an
    /// origin that never answers runs out its `HttpClient.Timeout`, and the
    /// `TaskCanceledException` is a `500`.
    #[tokio::test]
    async fn an_origin_that_never_answers_is_a_500() {
        let url = origin("", &[]).await;
        let relayed = tokio::time::timeout(
            GUARD,
            serve_remote_static(&url, None, Request::new(Body::empty()), SHORT_TIMEOUT),
        )
        .await
        .expect("the relay gave up on the origin");
        let error = relayed.expect_err("a silent origin is an error");
        assert_eq!(
            error.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// A pass-through channel's URL can carry the provider's account, and a
    /// `5xx`'s text is logged at the boundary.
    #[tokio::test]
    async fn a_failed_relay_names_only_the_origins_host() {
        let error = serve_remote_static(
            "http://acct-user:info-secret@127.0.0.1:0/path-secret/1.ts?password=query-secret",
            None,
            Request::new(Body::empty()),
            SHORT_TIMEOUT,
        )
        .await
        .expect_err("nothing listens there");

        let text = error.to_string();
        assert!(text.contains("http://127.0.0.1:0"), "{text}");
        for secret in ["acct-user", "info-secret", "path-secret", "query-secret"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    /// The timeout stops at the headers (`ResponseHeadersRead`): a body that
    /// takes longer than it is relayed whole, never cut off.
    #[tokio::test]
    async fn a_body_slower_than_the_timeout_is_relayed_whole() {
        let url = origin(
            "HTTP/1.1 200 OK\r\nContent-Type: video/x-matroska\r\nContent-Length: 12\r\nConnection: close\r\n\r\n",
            &["abc", "def", "ghi", "jkl"],
        )
        .await;
        let body = tokio::time::timeout(GUARD, async {
            let response =
                serve_remote_static(&url, None, Request::new(Body::empty()), SHORT_TIMEOUT)
                    .await
                    .expect("relayed");
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body")
        })
        .await
        .expect("the relay finished");
        assert_eq!(body.as_ref(), b"abcdefghijkl");
    }
}

#[cfg(test)]
mod transcode_body_tests {
    use super::*;
    use ferrofin_traits::media_encoding::TranscodeStreamProgress;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Observer {
        bytes: AtomicUsize,
        completed: AtomicBool,
        exited: AtomicBool,
    }

    impl TranscodeStreamProgress for Observer {
        fn add_bytes(&self, bytes: usize) {
            self.bytes.fetch_add(bytes, Ordering::SeqCst);
        }
        fn complete(&self) {
            self.completed.store(true, Ordering::SeqCst);
        }
        fn has_exited(&self) -> bool {
            self.exited.load(Ordering::SeqCst)
        }
        fn is_progressive(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn completion_counts_actual_chunks_and_disconnect_runs_callback() {
        use futures_util::StreamExt as _;
        let observer = Arc::new(Observer::default());
        let body = track_transcode_body(Body::from("0123456789"), observer.clone());
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), b"0123456789");
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 10);
        assert!(observer.completed.load(Ordering::SeqCst));
        let observer = Arc::new(Observer::default());
        let mut body =
            track_transcode_body(Body::from("unfinished"), observer.clone()).into_data_stream();
        assert!(body.next().await.is_some());
        drop(body);
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 10);
        assert!(observer.completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn growing_output_streams_appended_bytes_until_process_exit() {
        use futures_util::StreamExt as _;
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.ts");
        std::fs::write(&path, b"first").unwrap();
        let observer = Arc::new(Observer::default());
        let response = serve_growing_transcode(
            path.to_str().unwrap(),
            Request::new(Body::empty()),
            Some(observer.clone()),
        )
        .await
        .unwrap();
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::CONTENT_LENGTH)
        );
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"first");
        assert!(!observer.completed.load(Ordering::SeqCst));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"second")
            .unwrap();
        assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"second");
        observer.exited.store(true, Ordering::SeqCst);
        assert!(body.next().await.is_none());
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 11);
        assert!(observer.completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn growing_read_cancellation_and_missing_file_run_completion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.ts");
        std::fs::write(&path, b"").unwrap();
        let observer = Arc::new(Observer::default());
        let response = serve_growing_transcode(
            path.to_str().unwrap(),
            Request::new(Body::empty()),
            Some(observer.clone()),
        )
        .await
        .unwrap();
        let read = axum::body::to_bytes(response.into_body(), usize::MAX);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(75), read)
                .await
                .is_err()
        );
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 0);
        assert!(observer.completed.load(Ordering::SeqCst));
        let observer = Arc::new(Observer::default());
        assert!(
            serve_growing_transcode(
                directory.path().join("missing").to_str().unwrap(),
                Request::new(Body::empty()),
                Some(observer.clone())
            )
            .await
            .is_err()
        );
        assert!(observer.completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn failed_segment_body_still_runs_its_completion_callback() {
        let observer = Arc::new(Observer::default());
        let failed = Body::from_stream(futures_util::stream::once(async {
            Err::<Vec<u8>, _>(std::io::Error::other("fixture disconnected"))
        }));
        assert!(
            axum::body::to_bytes(track_transcode_body(failed, observer.clone()), usize::MAX)
                .await
                .is_err()
        );
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 0);
        assert!(observer.completed.load(Ordering::SeqCst));
    }
}
