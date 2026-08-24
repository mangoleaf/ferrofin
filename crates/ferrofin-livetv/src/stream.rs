//! The tuner live-stream engine — port of `Jellyfin.LiveTv.TunerHosts`'
//! `LiveStream` / `SharedHttpStream`.
//!
//! Opening a channel means opening the tuner's HTTP stream **once** and copying
//! it into a temp `.ts` file under the transcode directory; every consumer
//! (a client's direct play, the HLS transcoder's ffmpeg input, the DVR
//! recorder) then reads that one file through
//! `GET /LiveTv/LiveStreamFiles/{uniqueId}/stream.ts`. That is what makes a
//! single tuner serve several viewers, and it is why the opened media source's
//! `Path` is a Ferrofin URL rather than the tuner URL.
//!
//! A tuner whose stream cannot be shared (looping enabled, or a container the
//! upstream `_extensionsCanShareHttpStream`/`_mimeTypesCanShareHttpStream`
//! tables reject) falls back to the plain pass-through [`LiveStreamKind::Direct`]
//! stream: nothing is buffered and the media source keeps the tuner URL, exactly
//! as upstream's bare `LiveStream` does.
//!
//! The HTTP itself sits behind [`TunerStreamSource`] so the copy/sharing logic
//! is unit-testable against an in-memory tuner instead of a live server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::MediaStreamType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::live_tv::TunerHostInfo;
use ferrofin_model::media_info::MediaProtocol;
use ferrofin_traits::error::ServiceError;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::LiveTvError;

/// The file extensions whose HTTP stream may be shared between consumers.
///
/// Port of `M3UTunerHost._extensionsCanShareHttpStream`.
const EXTENSIONS_CAN_SHARE_HTTP_STREAM: &[&str] = &["ts", "tsv", "m2t"];

/// The `Content-Type` media types whose HTTP stream may be shared between
/// consumers when the URL carries no extension.
///
/// Port of `M3UTunerHost._mimeTypesCanShareHttpStream`.
const MIME_TYPES_CAN_SHARE_HTTP_STREAM: &[&str] = &["video/mp2t"];

/// The browser-shaped `User-Agent` an M3U tuner host is sent when it configures
/// none of its own.
///
/// Port of the literal in `M3UTunerHost.CreateMediaSourceInfo`.
pub const DEFAULT_TUNER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

/// How long a tuner may go silent — before answering, or between two chunks of
/// an open stream — before Ferrofin gives up on it, in seconds.
///
/// This is `reqwest`'s per-read timeout, so it bounds the endless response body
/// too: a tuner that stops producing for this long is treated as dead and the
/// copy task tears the stream down. That is a deliberate divergence — upstream
/// inherits `HttpClient`'s 100 s and would ride out a longer stall — chosen
/// because a silent tuner otherwise pins a connection and a buffer for ever.
const TUNER_RESPONSE_TIMEOUT_SECONDS: u64 = 30;

/// How long the caller waits for the tuner's first byte before giving up.
///
/// Port of `SharedHttpStream.Open`'s `await taskCompletionSource.Task`, bounded:
/// upstream inherits `HttpClient`'s timeout, Ferrofin says it explicitly.
const FIRST_BYTE_TIMEOUT_SECONDS: u64 = 30;

/// One chunk-at-a-time reader over an open tuner response body.
///
/// The seam that replaces C#'s `HttpResponseMessage.Content.ReadAsStreamAsync`:
/// `None` is end-of-stream, which for a healthy tuner never comes.
#[async_trait]
pub trait TunerStreamBody: Send {
    /// Reads the next chunk of tuner bytes, or `None` at end of stream.
    ///
    /// # Errors
    ///
    /// Propagates the transport failure that ended the stream.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError>;
}

/// Opens a tuner URL for streaming.
///
/// Port of the `IHttpClientFactory` use in `SharedHttpStream.Open` and of the
/// `HEAD` probe in `M3UTunerHost.GetChannelStream`.
#[async_trait]
pub trait TunerStreamSource: Send + Sync {
    /// Opens `url` with `headers` and returns a reader over its body.
    ///
    /// # Errors
    ///
    /// Fails when the tuner is unreachable or answers a non-success status.
    async fn open(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Box<dyn TunerStreamBody>, ServiceError>;

    /// The `Content-Type` a `HEAD` of `url` reports, or `None` when the probe
    /// fails (upstream logs and disables sharing in exactly that case).
    async fn content_type(&self, url: &str, headers: &HashMap<String, String>) -> Option<String>;
}

/// The real tuner source: `reqwest`, streaming the response body.
#[derive(Debug, Clone)]
pub struct ReqwestTunerSource {
    client: reqwest::Client,
}

impl Default for ReqwestTunerSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTunerSource {
    /// Creates a tuner source over a `reqwest` client bounded by
    /// [`TUNER_RESPONSE_TIMEOUT_SECONDS`] — both to connect and between reads
    /// of the stream body.
    #[must_use]
    pub fn new() -> Self {
        let timeout = std::time::Duration::from_secs(TUNER_RESPONSE_TIMEOUT_SECONDS);
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            // A client with default settings is still a working client; the
            // only thing lost is the bound, and playback must not die for it.
            .unwrap_or_default();
        Self { client }
    }

    /// Applies the tuner's `RequiredHttpHeaders` to a request builder.
    fn with_headers(
        mut builder: reqwest::RequestBuilder,
        headers: &HashMap<String, String>,
    ) -> reqwest::RequestBuilder {
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder
    }
}

/// A `reqwest` response body, read one chunk at a time.
struct ReqwestTunerBody {
    response: reqwest::Response,
}

#[async_trait]
impl TunerStreamBody for ReqwestTunerBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
        let chunk = self
            .response
            .chunk()
            .await
            .map_err(|e| LiveTvError::http("read tuner stream", e))?;
        Ok(chunk.map(|b| b.to_vec()))
    }
}

#[async_trait]
impl TunerStreamSource for ReqwestTunerSource {
    async fn open(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Box<dyn TunerStreamBody>, ServiceError> {
        let response = Self::with_headers(self.client.get(url), headers)
            .send()
            .await
            .map_err(|e| LiveTvError::http(format!("open tuner stream {url}"), e))?
            .error_for_status()
            .map_err(|e| LiveTvError::http(format!("open tuner stream {url}"), e))?;
        Ok(Box::new(ReqwestTunerBody { response }))
    }

    async fn content_type(&self, url: &str, headers: &HashMap<String, String>) -> Option<String> {
        let response = Self::with_headers(self.client.head(url), headers)
            .timeout(std::time::Duration::from_secs(
                TUNER_RESPONSE_TIMEOUT_SECONDS,
            ))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    }
}

/// The protocol a tuner path is read over.
///
/// Port of `MediaSourceManager.GetPathProtocol`: the URI scheme names the
/// protocol, and anything without one of the known schemes is a file path.
#[must_use]
pub fn path_protocol(path: &str) -> MediaProtocol {
    let lower = path.to_ascii_lowercase();
    for (scheme, protocol) in [
        ("rtmp://", MediaProtocol::Rtmp),
        ("rtmps://", MediaProtocol::Rtmp),
        ("rtsp://", MediaProtocol::Rtsp),
        ("rtspu://", MediaProtocol::Rtsp),
        ("rtspt://", MediaProtocol::Rtsp),
        ("ftp://", MediaProtocol::Ftp),
        ("rtp://", MediaProtocol::Rtp),
        ("udp://", MediaProtocol::Udp),
        ("http://", MediaProtocol::Http),
        ("https://", MediaProtocol::Http),
    ] {
        if lower.starts_with(scheme) {
            return protocol;
        }
    }
    MediaProtocol::File
}

/// Whether a tuner URL's host is on the local network.
///
/// Port of the state-free subset of `NetworkManager.IsInLocalNetwork`: loopback,
/// the RFC 1918 / RFC 4193 private ranges and link-local addresses are local. A
/// host that is not an IP literal is treated as remote, the way the C# check
/// falls through when the name resolves to nothing on the LAN.
#[must_use]
pub fn is_in_local_network(host: &str) -> bool {
    // An IPv6 literal in a URL is bracketed.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // `fc00::/7` is IPv6's private range; `fe80::/10` is link-local.
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// The host component of a URL, or `None` when it does not parse as one.
fn url_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Strip the port, but not the colons inside a bracketed IPv6 literal.
    let host = if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or(authority)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    (!host.is_empty()).then_some(host)
}

/// Builds the unopened media source for one tuner channel.
///
/// Port of `M3UTunerHost.CreateMediaSourceInfo`: the tuner URL as `Path`, the
/// two placeholder `Index = -1` streams (the container's real layout is unknown
/// until something probes it), `RequiresOpening`/`RequiresClosing`, and the
/// tuner host's own switches (looping, tuner count, DTS, native framerate, fMP4
/// container, fallback bitrate, user agent).
#[must_use]
pub fn create_media_source_info(path: &str, tuner: &TunerHostInfo) -> MediaSourceInfo {
    let supports_direct_play = !tuner.enable_stream_looping && tuner.tuner_count == 0;
    let supports_direct_stream = !tuner.enable_stream_looping;
    let protocol = path_protocol(path);
    let is_remote = url_host(path).is_none_or(|host| !is_in_local_network(host));

    let mut required_http_headers = std::collections::HashMap::new();
    if protocol == MediaProtocol::Http {
        // A user-defined user-agent, else something that looks like a browser.
        let user_agent = tuner
            .user_agent
            .as_deref()
            .map(str::trim)
            .filter(|ua| !ua.is_empty())
            .unwrap_or(DEFAULT_TUNER_USER_AGENT);
        required_http_headers.insert("User-Agent".to_owned(), user_agent.to_owned());
    }

    let mut source = MediaSourceInfo {
        path: Some(path.to_owned()),
        protocol,
        media_streams: vec![
            MediaStream {
                stream_type: MediaStreamType::Video,
                index: -1,
                // True when unknown, to enable deinterlacing.
                is_interlaced: true,
                ..MediaStream::default()
            },
            MediaStream {
                stream_type: MediaStreamType::Audio,
                index: -1,
                ..MediaStream::default()
            },
        ],
        requires_opening: true,
        requires_closing: true,
        requires_looping: tuner.enable_stream_looping,
        read_at_native_framerate: tuner.read_at_native_framerate,
        id: Some(
            ferrofin_common::extensions::get_md5(path)
                .simple()
                .to_string(),
        ),
        is_infinite_stream: true,
        is_remote,
        ignore_dts: tuner.ignore_dts,
        supports_direct_play,
        supports_direct_stream,
        required_http_headers,
        use_most_compatible_transcoding_profile: !tuner.allow_fmp4_transcoding_container,
        fallback_max_streaming_bitrate: Some(tuner.fallback_max_streaming_bitrate),
        ..MediaSourceInfo::default()
    };
    source.infer_total_bitrate(false);
    source
}

/// Cleans a tuner media source the way `LiveTvMediaSourceProvider.Normalize`
/// does before it reaches a client: an infinite stream, no zero-or-negative
/// numeric stream fields, unknown (`-1`) indexes when they collide, and an
/// inferred total bitrate.
pub fn normalize(source: &mut MediaSourceInfo) {
    // Not all of the plugins set this.
    source.is_infinite_stream = true;

    for stream in &mut source.media_streams {
        clear_non_positive(&mut stream.bit_rate);
        clear_non_positive(&mut stream.channels);
        clear_non_positive_f32(&mut stream.average_frame_rate);
        clear_non_positive_f32(&mut stream.real_frame_rate);
        clear_non_positive(&mut stream.width);
        clear_non_positive(&mut stream.height);
        clear_non_positive(&mut stream.sample_rate);
        if stream.level.is_some_and(|l| l <= 0.0) {
            stream.level = None;
        }
    }

    // Duplicate indexes mean the layout is unknown: mark them all unknown.
    let distinct: std::collections::HashSet<i32> =
        source.media_streams.iter().map(|s| s.index).collect();
    if distinct.len() != source.media_streams.len() {
        for stream in &mut source.media_streams {
            stream.index = -1;
        }
    }

    source.infer_total_bitrate(false);
}

/// Nulls a stream field that is zero or negative (C# `stream.X is <= 0`).
fn clear_non_positive(value: &mut Option<i32>) {
    if value.is_some_and(|v| v <= 0) {
        *value = None;
    }
}

/// [`clear_non_positive`] for the float-valued frame-rate fields.
fn clear_non_positive_f32(value: &mut Option<f32>) {
    if value.is_some_and(|v| v <= 0.0) {
        *value = None;
    }
}

/// The share decision `M3UTunerHost.GetChannelStream` makes from the stream
/// URL's file extension: `Some(true)`/`Some(false)` when the URL carries one,
/// `None` when it does not (upstream then falls back to a `HEAD` probe of the
/// `Content-Type`).
#[must_use]
pub fn extension_can_share(url: &str) -> Option<bool> {
    // `Path.GetExtension(new UriBuilder(path).Path)` — only the URL's PATH is
    // examined, so neither the query string, the fragment, nor the host's own
    // dots (`http://tuner.example.com`) ever supply the extension.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let path = match path.split_once("://") {
        // Everything from the first `/` after the authority; a URL with no path
        // at all is `/`, which has no extension.
        Some((_, rest)) => rest.find('/').map_or("/", |at| &rest[at..]),
        None => path,
    };
    let last_segment = path.rsplit('/').next().unwrap_or("");
    let (_, extension) = last_segment.rsplit_once('.')?;
    if extension.is_empty() {
        return None;
    }
    Some(
        EXTENSIONS_CAN_SHARE_HTTP_STREAM
            .iter()
            .any(|e| e.eq_ignore_ascii_case(extension)),
    )
}

/// Whether a `HEAD` `Content-Type` allows sharing the tuner's HTTP stream.
///
/// Port of the `_mimeTypesCanShareHttpStream.Contains(...)` check; the
/// `;charset=…` parameters are not part of the media type.
#[must_use]
pub fn mime_type_can_share(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    MIME_TYPES_CAN_SHARE_HTTP_STREAM.contains(&media_type.as_str())
}

/// How an opened tuner stream is delivered to consumers.
#[derive(Debug)]
pub enum LiveStreamKind {
    /// A shared HTTP stream: the tuner body is copied to `temp_path` and every
    /// consumer reads that file (C# `SharedHttpStream`).
    Shared {
        /// The temp `.ts` file the copy task appends to.
        temp_path: PathBuf,
        /// The copy task; aborting it drops the tuner connection.
        task: JoinHandle<()>,
    },
    /// A pass-through stream: nothing is buffered and the media source keeps
    /// the tuner URL (C# bare `LiveStream`).
    Direct,
}

/// An open tuner stream and its bookkeeping — the Rust shape of
/// `ILiveStream`.
#[derive(Debug)]
pub struct LiveStreamHandle {
    /// C# `ILiveStream.UniqueId`: the id in the `LiveStreamFiles` URL.
    pub unique_id: String,
    /// C# `ILiveStream.OriginalStreamId`: the media-source id the open was
    /// keyed by, which is what a later open matches to join this stream.
    pub original_stream_id: Option<String>,
    /// C# `ILiveStream.TunerHostId`: which tuner host this stream occupies, for
    /// the host's simultaneous-stream limit.
    pub tuner_host_id: Option<String>,
    /// C# `ILiveStream.DateOpened`.
    pub opened_at: DateTime<Utc>,
    /// C# `ILiveStream.ConsumerCount`.
    pub consumer_count: i32,
    /// C# `ILiveStream.EnableStreamSharing`: cleared by the copy task when the
    /// tuner hangs up on its own, because at that moment the buffer is deleted
    /// and there is nothing left to share (`SharedHttpStream.StartStreaming`
    /// sets the same flag false on that exact exit).
    pub enable_stream_sharing: Arc<AtomicBool>,
    /// The opened media source handed back to the caller.
    pub media_source: MediaSourceInfo,
    /// How the stream is delivered.
    pub kind: LiveStreamKind,
}

impl LiveStreamHandle {
    /// Whether the stream is still live and joinable.
    #[must_use]
    pub fn is_sharing(&self) -> bool {
        self.enable_stream_sharing.load(Ordering::SeqCst)
    }

    /// The buffered temp file, when this stream is a shared one.
    #[must_use]
    pub fn temp_path(&self) -> Option<&Path> {
        match &self.kind {
            LiveStreamKind::Shared { temp_path, .. } => Some(temp_path.as_path()),
            LiveStreamKind::Direct => None,
        }
    }

    /// Closes the stream: drops the tuner connection and deletes the buffer.
    ///
    /// Port of `LiveStream.Close()` (cancel the copy) plus the
    /// `DeleteTempFiles` its copy task runs on the way out — done here because
    /// aborting the task skips its own cleanup.
    pub async fn close(self) {
        self.enable_stream_sharing.store(false, Ordering::SeqCst);
        if let LiveStreamKind::Shared { temp_path, task } = self.kind {
            task.abort();
            // The abort lands at the task's next await point; the unlink is
            // safe either way (a still-open write handle keeps working on the
            // now-unlinked inode, and the reader side is read-only).
            if let Err(error) = tokio::fs::remove_file(&temp_path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %temp_path.display(), %error, "live tv: deleting the live-stream buffer failed");
            }
        }
    }
}

/// A freshly-opened shared tuner stream.
#[derive(Debug)]
pub struct OpenedSharedStream {
    /// The id the `LiveStreamFiles` URL carries.
    pub unique_id: String,
    /// The temp file the copy task is filling.
    pub temp_path: PathBuf,
    /// When the first byte landed (C# `DateOpened`).
    pub opened_at: DateTime<Utc>,
    /// Set while the copy task is running; cleared when the tuner hangs up.
    pub alive: Arc<AtomicBool>,
    /// The copy task; aborting it drops the tuner connection.
    pub task: JoinHandle<()>,
}

/// Opens a tuner stream and starts copying it into a temp file, resolving once
/// the first byte has landed.
///
/// Port of `SharedHttpStream.Open` + `LiveStream`'s constructor: a fresh
/// `UniqueId`, `{transcode_dir}/{uniqueId}.ts` as the buffer, a background copy,
/// and an error when the tuner produced nothing at all.
///
/// # Errors
///
/// Fails when the temp directory cannot be created, the tuner cannot be opened,
/// or the tuner closed without sending a byte (C# `EndOfStreamException`).
// The headers are a `MediaSourceInfo.RequiredHttpHeaders` map, which the DTO
// pins to the default hasher; generalizing here would only fight the seam.
#[allow(clippy::implicit_hasher)]
pub async fn open_shared_http_stream(
    source: &dyn TunerStreamSource,
    url: &str,
    headers: &HashMap<String, String>,
    transcode_dir: &Path,
) -> Result<OpenedSharedStream, ServiceError> {
    let unique_id = Uuid::new_v4().simple().to_string();
    let temp_path = transcode_dir.join(format!("{unique_id}.ts"));
    tokio::fs::create_dir_all(transcode_dir)
        .await
        .map_err(|e| LiveTvError::io(format!("create {}", transcode_dir.display()), e))?;

    let body = source.open(url, headers).await?;
    let alive = Arc::new(AtomicBool::new(true));
    let (first_byte_tx, first_byte_rx) = oneshot::channel();
    let task = tokio::spawn(copy_to_temp_file(
        body,
        temp_path.clone(),
        first_byte_tx,
        Arc::clone(&alive),
    ));

    let first_byte = tokio::time::timeout(
        std::time::Duration::from_secs(FIRST_BYTE_TIMEOUT_SECONDS),
        first_byte_rx,
    )
    .await;
    if let Ok(Ok(true)) = first_byte {
        Ok(OpenedSharedStream {
            unique_id,
            temp_path,
            opened_at: Utc::now(),
            alive,
            task,
        })
    } else {
        task.abort();
        let _ = tokio::fs::remove_file(&temp_path).await;
        Err(ServiceError::backend(format!(
            "zero bytes copied from live stream {url}"
        )))
    }
}

/// Copies the tuner body into `temp_path` until it ends or the task is aborted,
/// signalling `first_byte` the moment anything lands (C# `StartStreaming`).
async fn copy_to_temp_file(
    mut body: Box<dyn TunerStreamBody>,
    temp_path: PathBuf,
    first_byte: oneshot::Sender<bool>,
    alive: Arc<AtomicBool>,
) {
    // Whatever ends this task — the tuner hanging up, a write failure, an
    // abort — the buffer is gone afterwards, so the stream stops being
    // joinable at the same moment.
    let _sharing = SharingGuard(alive);
    let mut first_byte = Some(first_byte);
    let mut file = match tokio::fs::File::create(&temp_path).await {
        Ok(file) => file,
        Err(error) => {
            tracing::error!(path = %temp_path.display(), %error, "live tv: opening the live-stream buffer failed");
            if let Some(tx) = first_byte.take() {
                let _ = tx.send(false);
            }
            return;
        }
    };
    loop {
        match body.next_chunk().await {
            Ok(Some(chunk)) => {
                if chunk.is_empty() {
                    // Yield, so a body that only ever returns empty chunks
                    // cannot spin this task hot.
                    tokio::task::yield_now().await;
                    continue;
                }
                if let Err(error) = file.write_all(&chunk).await {
                    tracing::error!(path = %temp_path.display(), %error, "live tv: writing the live-stream buffer failed");
                    break;
                }
                // Flush every chunk: `tokio::fs::File` buffers, and a reader
                // tailing this file (a client, or ffmpeg transcoding the same
                // channel) must see the bytes as they arrive, not whenever the
                // buffer happens to fill.
                if let Err(error) = file.flush().await {
                    tracing::error!(path = %temp_path.display(), %error, "live tv: flushing the live-stream buffer failed");
                    break;
                }
                if let Some(tx) = first_byte.take() {
                    let _ = tx.send(true);
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(path = %temp_path.display(), %error, "live tv: the tuner stream ended");
                break;
            }
        }
    }
    let _ = file.flush().await;
    if let Some(tx) = first_byte.take() {
        let _ = tx.send(false);
    }
    // The tuner hung up on its own: the buffer is dead, so it goes (C#
    // `StartStreaming`'s `DeleteTempFiles` on the way out).
    let _ = tokio::fs::remove_file(&temp_path).await;
}

/// Clears the "still sharing" flag when the copy task ends, however it ends —
/// including an abort, which skips everything after the await it lands on.
struct SharingGuard(Arc<AtomicBool>);

impl Drop for SharingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        LiveStreamHandle, LiveStreamKind, TunerStreamBody, TunerStreamSource, extension_can_share,
        mime_type_can_share, open_shared_http_stream,
    };
    use async_trait::async_trait;
    use ferrofin_model::media_info::MediaProtocol;
    use ferrofin_traits::error::ServiceError;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A tuner that serves `chunk` forever — the fixture broadcast in miniature.
    pub(crate) struct LoopingTuner {
        pub(crate) chunk: Vec<u8>,
        pub(crate) opens: Arc<AtomicUsize>,
        pub(crate) content_type: Option<String>,
    }

    struct LoopingBody {
        chunk: Vec<u8>,
    }

    #[async_trait]
    impl TunerStreamBody for LoopingBody {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
            tokio::task::yield_now().await;
            Ok(Some(self.chunk.clone()))
        }
    }

    #[async_trait]
    impl TunerStreamSource for LoopingTuner {
        async fn open(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Result<Box<dyn TunerStreamBody>, ServiceError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(LoopingBody {
                chunk: self.chunk.clone(),
            }))
        }

        async fn content_type(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Option<String> {
            self.content_type.clone()
        }
    }

    /// A tuner that answers with an immediately-finished body.
    struct EmptyTuner;

    struct EmptyBody;

    #[async_trait]
    impl TunerStreamBody for EmptyBody {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
            Ok(None)
        }
    }

    #[async_trait]
    impl TunerStreamSource for EmptyTuner {
        async fn open(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Result<Box<dyn TunerStreamBody>, ServiceError> {
            Ok(Box::new(EmptyBody))
        }

        async fn content_type(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Option<String> {
            None
        }
    }

    #[test]
    fn the_extension_share_table_matches_upstream() {
        assert_eq!(extension_can_share("http://tuner/live.ts?ch=1"), Some(true));
        assert_eq!(extension_can_share("http://tuner/live.M2T"), Some(true));
        assert_eq!(extension_can_share("http://tuner/live.mkv"), Some(false));
        assert_eq!(extension_can_share("http://tuner/live"), None);
        // The query string never supplies the extension.
        assert_eq!(extension_can_share("http://tuner/live?f=x.ts"), None);
        // Nor does the host's own dots, on a URL with no path.
        assert_eq!(extension_can_share("http://tuner.example.com"), None);
        assert_eq!(extension_can_share("http://tuner.example.com/"), None);
    }

    #[test]
    fn the_mime_share_table_ignores_charset_parameters() {
        assert!(mime_type_can_share("video/MP2T"));
        assert!(mime_type_can_share("video/mp2t; charset=binary"));
        assert!(!mime_type_can_share("video/mp4"));
    }

    #[tokio::test]
    async fn an_open_stream_buffers_the_tuner_bytes_and_close_deletes_the_buffer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let opens = Arc::new(AtomicUsize::new(0));
        let tuner = LoopingTuner {
            chunk: vec![0x47; 188],
            opens: Arc::clone(&opens),
            content_type: None,
        };
        let opened =
            open_shared_http_stream(&tuner, "http://tuner/live.ts", &HashMap::new(), dir.path())
                .await
                .expect("open");
        let temp_path = opened.temp_path.clone();
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(
            temp_path.file_name().unwrap(),
            format!("{}.ts", opened.unique_id).as_str()
        );
        let first = tokio::fs::metadata(&temp_path).await.expect("buffer").len();
        assert!(
            first > 0,
            "the first chunk must have landed before open returned"
        );

        // It keeps growing: the copy task is still consuming the tuner. Poll
        // rather than sleeping once, so the assertion is `>` (a copy task that
        // died at the first chunk must fail this).
        let mut later = first;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            later = tokio::fs::metadata(&temp_path).await.expect("buffer").len();
            if later > first {
                break;
            }
        }
        assert!(
            later > first,
            "the copy task must still be filling the buffer"
        );
        assert!(opened.alive.load(Ordering::SeqCst));

        let handle = LiveStreamHandle {
            unique_id: opened.unique_id,
            original_stream_id: None,
            tuner_host_id: None,
            opened_at: opened.opened_at,
            consumer_count: 1,
            enable_stream_sharing: Arc::clone(&opened.alive),
            media_source: ferrofin_model::dto::MediaSourceInfo::default(),
            kind: LiveStreamKind::Shared {
                temp_path: temp_path.clone(),
                task: opened.task,
            },
        };
        assert!(handle.is_sharing());
        handle.close().await;
        assert!(!temp_path.exists(), "close must delete the buffer");
        assert!(
            !opened.alive.load(Ordering::SeqCst),
            "a closed stream must stop being joinable"
        );
    }

    /// A tuner that serves one chunk and then ends, the way a misconfigured
    /// source that hands back a finite file does.
    struct FiniteTuner;

    struct FiniteBody {
        sent: bool,
    }

    #[async_trait]
    impl TunerStreamBody for FiniteBody {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ServiceError> {
            if self.sent {
                return Ok(None);
            }
            self.sent = true;
            Ok(Some(vec![0x47; 188]))
        }
    }

    #[async_trait]
    impl TunerStreamSource for FiniteTuner {
        async fn open(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Result<Box<dyn TunerStreamBody>, ServiceError> {
            Ok(Box::new(FiniteBody { sent: false }))
        }

        async fn content_type(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn a_tuner_that_hangs_up_stops_the_stream_being_joinable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let opened = open_shared_http_stream(
            &FiniteTuner,
            "http://tuner/live.ts",
            &HashMap::new(),
            dir.path(),
        )
        .await
        .expect("open");
        // The tuner ended on its own: the copy task deletes the buffer, so the
        // stream must stop advertising itself as shareable — otherwise a later
        // viewer joins a stream whose file no longer exists.
        for _ in 0..200 {
            if !opened.alive.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !opened.alive.load(Ordering::SeqCst),
            "the dead stream must clear its sharing flag"
        );
        assert!(!opened.temp_path.exists(), "the buffer must be deleted");
    }

    #[test]
    fn the_protocol_table_covers_every_scheme_upstream_knows() {
        use super::path_protocol;
        assert_eq!(path_protocol("http://t/live"), MediaProtocol::Http);
        assert_eq!(path_protocol("HTTPS://t/live"), MediaProtocol::Http);
        assert_eq!(path_protocol("rtsp://t/live"), MediaProtocol::Rtsp);
        assert_eq!(path_protocol("rtmp://t/live"), MediaProtocol::Rtmp);
        assert_eq!(path_protocol("udp://239.0.0.1:1234"), MediaProtocol::Udp);
        assert_eq!(path_protocol("rtp://t/live"), MediaProtocol::Rtp);
        assert_eq!(path_protocol("ftp://t/live"), MediaProtocol::Ftp);
        assert_eq!(path_protocol("/media/live.ts"), MediaProtocol::File);
    }

    #[tokio::test]
    async fn a_tuner_that_sends_nothing_is_an_error_and_leaves_no_buffer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = open_shared_http_stream(
            &EmptyTuner,
            "http://tuner/live.ts",
            &HashMap::new(),
            dir.path(),
        )
        .await
        .expect_err("zero bytes must fail");
        assert!(error.to_string().contains("zero bytes"), "{error}");
        let mut entries = tokio::fs::read_dir(dir.path()).await.expect("read dir");
        assert!(
            entries.next_entry().await.expect("entry").is_none(),
            "the failed open must leave no buffer behind"
        );
    }
}
