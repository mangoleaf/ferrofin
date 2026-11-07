//! The limits Kestrel puts on an accepted connection.
//!
//! `axum::serve` gives hyper no timer, so hyper's own header-read timeout never
//! runs, and that one would measure the wait for a request as well. Kestrel
//! (`Http1Connection`, `HttpConnection.OnTimeout`, `TimeoutControl`) keeps the
//! stages apart, and this module ports that:
//!
//! - **Between requests**, a new connection included, a peer has
//!   [`KESTREL_LIMITS`]`.keep_alive` to send the first byte of a request. Blank
//!   lines do not count. Past it the connection closes without a response.
//! - **From that byte**, it has [`KESTREL_LIMITS`]`.request_headers` to finish
//!   the request line and headers. Past it the connection is answered `408` and
//!   closes.
//! - **While a handler waits for request body bytes** that have not arrived,
//!   the body must keep up [`KESTREL_LIMITS`]`.min_request_body_rate` (Kestrel's
//!   `MinRequestBodyDataRate`). Only the waits count: time the handler spends on
//!   anything else does not. Past it the handler's read fails and the request is
//!   answered as Jellyfin's `ExceptionMiddleware` answers the
//!   `BadHttpRequestException` Kestrel's read throws: `500`, `text/plain`,
//!   `Error processing request.`; then the connection closes.
//! - **While a write waits on the peer**, the bytes the peer has taken must keep
//!   up [`KESTREL_LIMITS`]`.min_response_rate` (Kestrel's `MinResponseDataRate`).
//!   Past it the connection is dropped, as Kestrel aborts it.
//!
//! So a slow handler, a response body that pauses and an idle WebSocket are
//! never cut: nothing waits on the peer. A paused player is: Kestrel gives
//! every byte its time at the rate, and the bytes a peer takes faster than that
//! leave their spare time to the ones after, so a paused player is cut once
//! the bytes it has taken have had their time. At 240 bytes a second, a player
//! paused after taking 100 MB holds its connection for about five days. A peer
//! that never reads is cut after the grace period, or after the time at the
//! rate of what the kernel took, whichever is longer.
//!
//! An upgraded socket (`/socket`) has left HTTP for good: no read limit applies
//! to it again. Its writes stay under the response rate, as Kestrel's do: its
//! upgraded stream writes through the same `Http1OutputProducer` flush, which
//! times them (`IHttpMinResponseDataRateFeature`), though `KestrelServerLimits`
//! says the limit has no effect there.
//!
//! [`LimitedIo`] enforces the connection's limits at the socket, below
//! `axum::serve` and hyper, so they also cover hyper-util's version sniffing,
//! which reads the first bytes with no timeout of its own. The socket sees the
//! first byte of a request and [`track_requests`] sees it start and end; they
//! share a [`ConnectionPhase`]. [`track_requests`] times the request bodies.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};

/// The limits on a connection: how long it may wait for a request, how long a
/// request's headers may take to arrive, and the rates its peer must keep up
/// while the server waits on it.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// The wait for the first byte of a request.
    pub keep_alive: Duration,
    /// The wait for the rest of the headers, from that byte.
    pub request_headers: Duration,
    /// The rate a request body must arrive at while a handler waits for it.
    pub min_request_body_rate: MinDataRate,
    /// The rate the peer must take a response at while a write waits on it.
    pub min_response_rate: MinDataRate,
}

/// A minimum rate a peer must keep up: Kestrel's `MinDataRate`.
#[derive(Debug, Clone, Copy)]
pub struct MinDataRate {
    /// The rate, in bytes a second.
    pub bytes_per_second: u32,
    /// How long the rate is not enforced for, from the first wait.
    pub grace: Duration,
}

impl MinDataRate {
    /// How long `bytes` take at this rate.
    fn time_for(self, bytes: u64) -> Duration {
        let nanos = u128::from(bytes) * 1_000_000_000 / u128::from(self.bytes_per_second.max(1));
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// Kestrel's defaults, which Jellyfin does not override
/// (`KestrelServerLimits.KeepAliveTimeout`, `RequestHeadersTimeout`,
/// `MinRequestBodyDataRate` and `MinResponseDataRate`).
///
/// TODO(gl-307): the four are tuning literals, candidate settings for the
/// owner; they keep Kestrel's values until then.
pub const KESTREL_LIMITS: ConnectionLimits = ConnectionLimits {
    keep_alive: Duration::from_secs(130),
    request_headers: Duration::from_secs(30),
    min_request_body_rate: KESTREL_MIN_DATA_RATE,
    min_response_rate: KESTREL_MIN_DATA_RATE,
};

/// Both of Kestrel's minimum data rates: 240 bytes a second after 5 seconds
/// ("Matches the default IIS minBytesPerSecond").
const KESTREL_MIN_DATA_RATE: MinDataRate = MinDataRate {
    bytes_per_second: 240,
    grace: Duration::from_secs(5),
};

/// Where the minimum data rates depart from Kestrel's, each with its fix path.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "a record for readers; a test checks its form")
)]
const DATA_RATE_DIVERGENCES: &[&str] = &[
    "The time is exact. Kestrel checks both rates on its one-second heartbeat, so a \
     limit passes up to a second late there; and it adds a whole heartbeat to a body's \
     time whenever a read waited at any point in it, so for a handler that reads in many \
     short waits it counts more time than was waited and can cut a body Ferrofin keeps. \
     Fix path: time in whole heartbeats, as TimeoutControl.Tick does.",
    "The response rate credits bytes when the socket takes them. Kestrel credits a \
     handler's write when it is flushed into the output pipe, whole however large, \
     before the socket has taken any of it. So a peer that stops reading early is cut \
     sooner here: by the time of what Kestrel holds in its pipe (up to 64 KiB, about 4.5 \
     minutes at 240 bytes a second, plus the handler's last write), and far sooner for \
     a large response written in one go (Jellyfin 12.4 still held its unread 1.9 MB \
     /api-docs/openapi.json after 25 minutes). A player paused after taking a few \
     megabytes holds for hours either way. Crediting hyper's frames instead, without \
     the cap below, would let a peer that reads a kilobyte every few minutes hold a \
     connection for good, since hyper takes another frame into its 400 KB buffer for \
     each one. Fix path: gl-1238, crediting the frames hyper takes from the handler, \
     with hyper's write buffer capped at Kestrel's MaxResponseBufferSize (64 KiB), \
     which needs a serve loop that configures hyper (axum::serve does not).",
    "A response's first write gets the grace period, and so does a write made after the \
     peer had taken everything in its time. Kestrel gives it to every flush, so a \
     WebSocket message or a streamed chunk written while the peer is behind it gets \
     the grace period there and only its time at the rate here. Fix path: as for the \
     credit, at the handler's writes.",
    "Over HTTP/2 (h2c), each stream's body is timed on its own. Kestrel times a \
     connection's request bodies together, and not while its connection-level \
     flow-control window is low. Fix path: gl-814.",
];

/// What a timed-out request's peer is sent: the status and close of Kestrel's
/// `RequestRejectionReason.RequestHeadersTimeout`.
const REQUEST_TIMEOUT_RESPONSE: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

const WAITING: u8 = 0;
const READING_HEADERS: u8 = 1;
const SERVING: u8 = 2;
const UPGRADED: u8 = 3;

/// What a connection is doing, shared by its socket and its service stack.
#[derive(Debug, Default)]
pub struct ConnectionPhase {
    phase: AtomicU8,
    /// The read hyper has parked, for the end of a request to wake.
    reader: AtomicWaker,
    /// A response has been handed to hyper, and its first write is to come.
    response_started: AtomicBool,
}

impl ConnectionPhase {
    /// Moves `from` to `to`, unless the connection has moved on. Whether it did.
    fn advance(&self, from: u8, to: u8) -> bool {
        self.phase
            .compare_exchange(from, to, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn get(&self) -> u8 {
        self.phase.load(Ordering::Relaxed)
    }

    fn set(&self, phase: u8) {
        self.phase.store(phase, Ordering::Relaxed);
    }
}

/// `instant` plus `duration`, or a time as good as never when that overflows.
fn later(instant: Instant, duration: Duration) -> Instant {
    instant
        .checked_add(duration)
        .unwrap_or_else(|| instant + Duration::from_hours(30 * 365 * 24))
}

/// `timer`, made on first need, set to run out at `deadline`: whether it has.
fn poll_until(
    timer: &mut Option<Pin<Box<Sleep>>>,
    deadline: Instant,
    cx: &mut Context<'_>,
) -> Poll<()> {
    let timer = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
    if timer.deadline() != deadline {
        timer.as_mut().reset(deadline);
    }
    timer.as_mut().poll(cx)
}

/// An accepted connection under [`ConnectionLimits`].
#[derive(Debug)]
pub struct LimitedIo<I> {
    io: I,
    limits: ConnectionLimits,
    phase: Arc<ConnectionPhase>,
    /// Runs out when the limit in force has passed. Boxed to keep this `Unpin`.
    timer: Pin<Box<Sleep>>,
    /// The phase `timer` is measuring.
    timing: Option<u8>,
    /// The last write came up against the peer. The end of a request is the end
    /// of its body reaching hyper, so what hyper holds of a response the peer is
    /// slow to read still has to go: no other limit runs until it has.
    write_blocked: bool,
    /// When the bytes the peer has taken must have gone by at the minimum
    /// response rate: Kestrel's `_writeTimingTimeoutTimestamp`.
    write_deadline: Option<Instant>,
    /// A response has started, and its first write has not had the grace
    /// period yet.
    write_grace_due: bool,
    /// Runs out at `write_deadline`, for a write that waits on the peer.
    write_timer: Option<Pin<Box<Sleep>>>,
}

impl<I> LimitedIo<I> {
    /// Puts `io`, a connection just accepted, under `limits`.
    pub fn new(io: I, limits: ConnectionLimits) -> Self {
        Self {
            io,
            limits,
            phase: Arc::default(),
            timer: Box::pin(tokio::time::sleep(limits.keep_alive)),
            timing: None,
            write_blocked: false,
            write_deadline: None,
            write_grace_due: false,
            write_timer: None,
        }
    }

    /// The connection underneath.
    pub fn inner(&self) -> &I {
        &self.io
    }

    /// What the service stack must be told of this connection's requests.
    pub fn requests(&self) -> RequestTracker {
        RequestTracker {
            phase: Arc::clone(&self.phase),
            body_rate: self.limits.min_request_body_rate,
        }
    }
}

impl<I: AsyncWrite + Unpin> LimitedIo<I> {
    /// Notes how a write went, `accepted` being what the socket took of it,
    /// and runs the minimum response rate while it waits on the peer: `Err`
    /// once the rate has not been kept up. One that got through after being
    /// blocked wakes the read, parked with no limit running, unless a request
    /// still runs: its end does that.
    fn note_write<T>(
        &mut self,
        cx: &mut Context<'_>,
        written: Poll<std::io::Result<T>>,
        accepted: impl FnOnce(&T) -> usize,
    ) -> Poll<std::io::Result<T>> {
        if self.phase.response_started.swap(false, Ordering::Relaxed) {
            self.write_grace_due = true;
        }
        let was_blocked = self.write_blocked;
        let blocked = written.is_pending();
        if was_blocked && !blocked && self.phase.get() != SERVING {
            cx.waker().wake_by_ref();
        }
        self.write_blocked = blocked;
        match written {
            Poll::Ready(Ok(value)) => {
                self.credit(accepted(&value), was_blocked);
                Poll::Ready(Ok(value))
            }
            Poll::Pending => match self.poll_response_rate(cx, was_blocked) {
                Poll::Ready(error) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            },
            failed @ Poll::Ready(Err(_)) => failed,
        }
    }

    /// Whether the peer had taken every byte in its time before this write,
    /// so that the write starts afresh: Kestrel gives each write its grace
    /// period.
    fn caught_up(&mut self, was_blocked: bool, now: Instant) -> bool {
        std::mem::take(&mut self.write_grace_due)
            || (!was_blocked && self.write_deadline.is_none_or(|deadline| deadline <= now))
    }

    /// Kestrel's `BytesWrittenToBuffer` for `bytes` the socket took: they may
    /// take their time at the rate, a fresh write at least the grace period,
    /// and the time bytes taken faster than the rate did not use carries over
    /// to the ones after.
    fn credit(&mut self, bytes: usize, was_blocked: bool) {
        if bytes == 0 {
            return;
        }
        let rate = self.limits.min_response_rate;
        let now = Instant::now();
        let time = rate.time_for(u64::try_from(bytes).unwrap_or(u64::MAX));
        let single = if self.caught_up(was_blocked, now) {
            later(now, time.max(rate.grace))
        } else {
            later(now, time)
        };
        let accumulated = self
            .write_deadline
            .map_or(single, |deadline| later(deadline, time));
        self.write_deadline = Some(single.max(accumulated));
    }

    /// Called with a write waiting on the peer: `Ready` once the bytes it took
    /// have had their time (Kestrel's `CheckForWriteDataRateTimeout`), and
    /// the connection is to be aborted.
    fn poll_response_rate(
        &mut self,
        cx: &mut Context<'_>,
        was_blocked: bool,
    ) -> Poll<std::io::Error> {
        let now = Instant::now();
        if self.caught_up(was_blocked, now) {
            let grace = later(now, self.limits.min_response_rate.grace);
            self.write_deadline = Some(self.write_deadline.map_or(grace, |d| d.max(grace)));
        }
        let deadline = *self
            .write_deadline
            .get_or_insert_with(|| later(now, self.limits.min_response_rate.grace));
        ready!(poll_until(&mut self.write_timer, deadline, cx));
        let _ = Pin::new(&mut self.io).poll_shutdown(cx);
        tracing::debug!(limit = "response data rate", "connection timed out");
        Poll::Ready(std::io::ErrorKind::TimedOut.into())
    }

    /// Called with no bytes to read: `Ready` once the running limit has passed.
    fn poll_limit(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // hyper parks a read that came up empty, and reads again only when the
        // socket stirs, which it may never do. The end of a request wakes it, for
        // the wait for the next one to start here.
        self.phase.reader.register(cx.waker());
        let phase = if self.write_blocked {
            SERVING
        } else {
            self.phase.get()
        };
        let (limit, limit_name) = match phase {
            WAITING => (self.limits.keep_alive, "keep-alive"),
            READING_HEADERS => (self.limits.request_headers, "request headers"),
            _ => {
                self.timing = None;
                return Poll::Pending;
            }
        };
        if self.timing != Some(phase) {
            self.timer.as_mut().reset(Instant::now() + limit);
            self.timing = Some(phase);
        }
        ready!(self.timer.as_mut().poll(cx));
        self.timing = None;
        // Best effort, once: these are a few bytes on a socket with nothing else
        // to send, and a peer that cannot take them is closed all the same.
        if phase == READING_HEADERS {
            let _ = Pin::new(&mut self.io).poll_write(cx, REQUEST_TIMEOUT_RESPONSE);
        }
        let _ = Pin::new(&mut self.io).poll_shutdown(cx);
        tracing::debug!(limit = limit_name, "connection timed out");
        Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()))
    }
}

impl<I: AsyncRead + AsyncWrite + Unpin> AsyncRead for LimitedIo<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.io).poll_read(cx, buf) {
            Poll::Pending => this.poll_limit(cx),
            Poll::Ready(Ok(())) => {
                // `ParseRequest` skips the blank lines before a request. Bytes
                // restart no limit: each one measures from its own start.
                let received = &buf.filled()[before..];
                if received.iter().any(|byte| !matches!(byte, b'\r' | b'\n'))
                    && this.phase.advance(WAITING, READING_HEADERS)
                {
                    this.timing = None;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for LimitedIo<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.io).poll_write(cx, buf);
        this.note_write(cx, written, |taken| *taken)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.note_write(cx, written, |taken| *taken)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let flushed = Pin::new(&mut this.io).poll_flush(cx);
        this.note_write(cx, flushed, |()| 0)
    }

    /// A shutdown that waits on the peer (a TLS `close_notify` behind a full
    /// send buffer) is a write like any other.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let shut = Pin::new(&mut this.io).poll_shutdown(cx);
        this.note_write(cx, shut, |()| 0)
    }
}

/// Tells a connection's [`LimitedIo`] that its requests start and end, and
/// times their bodies.
#[derive(Debug, Clone)]
pub struct RequestTracker {
    phase: Arc<ConnectionPhase>,
    body_rate: MinDataRate,
}

impl RequestTracker {
    /// A request's headers have arrived. The connection's limits stand aside
    /// until the returned guard is dropped.
    #[must_use]
    pub fn start(&self) -> RunningRequest {
        self.phase.set(SERVING);
        RunningRequest {
            phase: Arc::clone(&self.phase),
            body_rate: self.body_rate,
            body_timed_out: Arc::default(),
        }
    }
}

/// A request that has not finished. Dropping it ends the request, also when a
/// disconnect or a panic cuts it short.
///
/// TODO(gl-814): one phase per connection holds for HTTP/1, which runs one
/// request at a time. HTTP/2 needs a count of its running streams.
#[derive(Debug)]
pub struct RunningRequest {
    phase: Arc<ConnectionPhase>,
    body_rate: MinDataRate,
    /// The request's body fell below the minimum rate.
    body_timed_out: Arc<AtomicBool>,
}

impl RunningRequest {
    /// Puts `request`'s body under the minimum request body rate. An empty
    /// one has nothing to wait for (Kestrel times no `ZeroContentLengthClose`
    /// body).
    fn time_body(&self, request: Request) -> Request {
        if request.body().is_end_stream() {
            return request;
        }
        let rate = self.body_rate;
        let timed_out = Arc::clone(&self.body_timed_out);
        request.map(|body| Body::new(TimedRequestBody::new(body, rate, timed_out)))
    }

    /// Ties this request to its `response`: the connection waits for its next
    /// request once hyper has finished with the body. A `101` ends HTTP on the
    /// connection, so no read limit applies again.
    ///
    /// A request whose body fell below the minimum rate is answered as
    /// Jellyfin answers the exception Kestrel's read throws, whatever the
    /// handler made of the failed read: the exception unwinds to its
    /// `ExceptionMiddleware`, which writes `GetStatusCode`'s `500` for a type
    /// it does not map. hyper then closes the connection, its body unread, as
    /// Kestrel does once the read has timed out.
    fn finish(self, response: Response) -> Response {
        let response = if self.body_timed_out.load(Ordering::Relaxed) {
            body_timeout_answer(response)
        } else {
            response
        };
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            self.phase.set(UPGRADED);
        }
        self.phase.response_started.store(true, Ordering::Relaxed);
        response.map(|body| {
            Body::new(TrackedBody {
                body,
                _request: self,
            })
        })
    }
}

/// `response` as `ExceptionMiddleware` answers a request whose body read threw:
/// `500`, `text/plain`, [`ERROR_BODY`](ferrofin_api::panic_boundary::ERROR_BODY).
/// What the layers around the handler added (CORS, `Content-Language`,
/// `X-Response-Time-ms`) stays, as it does upstream, where those middlewares
/// sit outside `ExceptionMiddleware`; what described the handler's own body
/// goes with it.
fn body_timeout_answer(response: Response) -> Response {
    let (mut parts, _) = response.into_parts();
    parts.status = StatusCode::INTERNAL_SERVER_ERROR;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain"),
    );
    Response::from_parts(parts, Body::from(ferrofin_api::panic_boundary::ERROR_BODY))
}

impl Drop for RunningRequest {
    fn drop(&mut self) {
        if self.phase.advance(SERVING, WAITING) {
            self.phase.reader.wake();
        }
    }
}

/// A request body under the minimum request body rate: Kestrel's
/// `MessageBody` read timing (`StartTimingReadAsync`, `StopTimingRead`) and
/// `TimeoutControl.CheckForReadDataRateTimeout`.
struct TimedRequestBody {
    body: Body,
    rate: MinDataRate,
    /// The bytes read so far: `_readTimingBytesRead`.
    bytes: u64,
    /// The time spent waiting for them: `_readTimingElapsedTicks`.
    waited: Duration,
    /// Since when the handler has been waiting, while it does. A read the
    /// handler abandons while it waits still counts until its next poll: the
    /// extractors that read request bodies never abandon one.
    waiting_since: Option<Instant>,
    /// Runs out when the wait has gone on too long. Made on the first wait.
    timer: Option<Pin<Box<Sleep>>>,
    /// Set when it did, for the request's answer.
    timed_out: Arc<AtomicBool>,
}

impl TimedRequestBody {
    fn new(body: Body, rate: MinDataRate, timed_out: Arc<AtomicBool>) -> Self {
        Self {
            body,
            rate,
            bytes: 0,
            waited: Duration::ZERO,
            waiting_since: None,
            timer: None,
            timed_out,
        }
    }

    /// Called with the handler waiting for bytes that have not arrived:
    /// `Ready` with the read's failure once the body is too slow. Kestrel
    /// times out once the waiting has outlasted the grace period with the
    /// bytes over it below the rate, which is once it outlasts the longer of
    /// the grace period and the time the bytes so far take at the rate.
    fn poll_rate(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Error> {
        let since = *self.waiting_since.get_or_insert_with(Instant::now);
        let allowed = self.rate.grace.max(self.rate.time_for(self.bytes));
        let deadline = later(since, allowed.saturating_sub(self.waited));
        ready!(poll_until(&mut self.timer, deadline, cx));
        self.timed_out.store(true, Ordering::Relaxed);
        tracing::debug!(limit = "request body data rate", "request timed out");
        Poll::Ready(std::io::ErrorKind::TimedOut.into())
    }
}

impl HttpBody for TimedRequestBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let this = self.get_mut();
        // Every read after the timeout fails as well, as Kestrel's do
        // (`RequestTimedOut`): the body never looks complete.
        if this.timed_out.load(Ordering::Relaxed) {
            return Poll::Ready(Some(Err(axum::Error::new(std::io::Error::from(
                std::io::ErrorKind::TimedOut,
            )))));
        }
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(frame) => {
                if let Some(since) = this.waiting_since.take() {
                    this.waited += since.elapsed();
                }
                if let Some(Ok(frame)) = &frame
                    && let Some(data) = frame.data_ref()
                {
                    this.bytes += u64::try_from(data.len()).unwrap_or(u64::MAX);
                }
                Poll::Ready(frame)
            }
            Poll::Pending => this
                .poll_rate(cx)
                .map(|error| Some(Err(axum::Error::new(error)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// A response body that keeps its request running, with its length intact.
struct TrackedBody {
    body: Body,
    _request: RunningRequest,
}

impl HttpBody for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// Runs `service` for a connection's requests, reporting them to `tracker`
/// and timing their bodies.
pub fn track_requests<S>(
    tracker: RequestTracker,
    service: S,
) -> impl tower::Service<
    Request,
    Response = Response,
    Error = std::convert::Infallible,
    Future: Send + 'static,
> + Clone
+ Send
+ 'static
where
    S: tower::Service<Request, Response = Response, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    tower::service_fn(move |request: Request| {
        let running = tracker.start();
        let request = running.time_body(request);
        let service = service.clone();
        async move {
            let response = tower::ServiceExt::oneshot(service, request).await?;
            Ok::<_, std::convert::Infallible>(running.finish(response))
        }
    })
}

/// A listener whose connections are under [`ConnectionLimits`].
pub struct LimitedListener<L> {
    listener: L,
    limits: ConnectionLimits,
}

impl<L> LimitedListener<L> {
    /// Puts every connection `listener` accepts under `limits`.
    pub fn new(listener: L, limits: ConnectionLimits) -> Self {
        Self { listener, limits }
    }
}

impl<L: axum::serve::Listener> axum::serve::Listener for LimitedListener<L> {
    type Io = LimitedIo<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, peer) = self.listener.accept().await;
        (LimitedIo::new(io, self.limits), peer)
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use axum::response::IntoResponse as _;
    use axum::routing::{get, post};
    use rstest::rstest;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
    use tokio::net::TcpStream;
    use tokio::sync::oneshot;
    use tokio::time::{sleep, timeout};

    use super::{
        ConnectionLimits, DATA_RATE_DIVERGENCES, KESTREL_LIMITS, KESTREL_MIN_DATA_RATE, LimitedIo,
        MinDataRate, TimedRequestBody,
    };

    /// Long enough that only a connection nothing closes takes this long.
    const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
    /// Short limits for the wait for a request, for what real sockets must
    /// show. The data rates' grace periods outlast every test that uses them.
    const LIMITS: ConnectionLimits = ConnectionLimits {
        keep_alive: Duration::from_millis(300),
        request_headers: Duration::from_millis(200),
        min_request_body_rate: KESTREL_MIN_DATA_RATE,
        min_response_rate: KESTREL_MIN_DATA_RATE,
    };
    /// Short data rates, for what real sockets must show: the request body's
    /// is Kestrel's after a fifth of a second, and the response's fast enough
    /// to run out on what the kernels hold.
    const RATE_LIMITS: ConnectionLimits = ConnectionLimits {
        keep_alive: Duration::from_secs(20),
        request_headers: Duration::from_secs(20),
        min_request_body_rate: MinDataRate {
            bytes_per_second: 240,
            grace: Duration::from_millis(200),
        },
        min_response_rate: MinDataRate {
            bytes_per_second: 64 * 1024 * 1024,
            grace: Duration::from_millis(200),
        },
    };
    /// Every limit short.
    const ALL_LIMITS: ConnectionLimits = ConnectionLimits {
        min_request_body_rate: RATE_LIMITS.min_request_body_rate,
        min_response_rate: RATE_LIMITS.min_response_rate,
        ..LIMITS
    };
    /// A pause past both of [`LIMITS`].
    const PAST_LIMITS: Duration = Duration::from_millis(500);
    const HELLO: &[u8] = b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n";

    struct Server {
        address: SocketAddr,
        stop: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Server {
        /// Serves `router` through the lifetime's real serve loop.
        fn start(router: Router, limits: ConnectionLimits) -> Self {
            use axum::serve::Listener as _;

            let listener = crate::listen::bind("127.0.0.1:0".parse().expect("address"));
            let listener = listener.expect("bind");
            let address = listener.local_addr().expect("local address");
            let app = crate::lifetime_app(router, "", None);
            let serving = crate::Serving::start_with_limits(
                vec![crate::listen::ServedListener::Plain(listener)],
                &app,
                limits,
            );
            let (stop, stopped) = oneshot::channel::<()>();
            let task = tokio::spawn(serving.drain(
                async move {
                    let _ = stopped.await;
                },
                1,
            ));
            Self {
                address,
                stop: Some(stop),
                task,
            }
        }

        async fn connect(&self) -> TcpStream {
            TcpStream::connect(self.address).await.expect("connect")
        }

        async fn stop(mut self) {
            let _ = self.stop.take().expect("running").send(());
            self.task.await.expect("join").expect("serve");
        }
    }

    fn hello() -> Router {
        Router::new().route("/hello", get(|| async { "hello" }))
    }

    /// Everything the server sends until it closes the connection.
    async fn read_to_close(stream: &mut TcpStream) -> String {
        let mut received = Vec::new();
        timeout(ANSWER_TIMEOUT, stream.read_to_end(&mut received))
            .await
            .expect("the server closes the connection")
            .expect("read");
        String::from_utf8_lossy(&received).into_owned()
    }

    /// One whole `hello` response, leaving the connection open.
    async fn read_hello(stream: &mut TcpStream) -> String {
        let mut received = Vec::new();
        let mut chunk = [0; 1024];
        while !received.ends_with(b"hello") {
            let read = timeout(ANSWER_TIMEOUT, stream.read(&mut chunk))
                .await
                .expect("answered in time")
                .expect("read");
            assert_ne!(read, 0, "closed mid-response: {received:?}");
            received.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    #[tokio::test]
    async fn a_connection_that_sends_nothing_is_closed_without_a_response() {
        let server = Server::start(hello(), LIMITS);
        let mut stream = server.connect().await;
        assert_eq!(read_to_close(&mut stream).await, "");
        server.stop().await;
    }

    /// hyper parks its read during a request whether the handler waits or not.
    /// With the header limit too long to wake it, only the end of the request
    /// can start the next wait.
    #[rstest]
    #[case::answered_at_once(Duration::ZERO)]
    #[case::answered_after_a_wait(PAST_LIMITS)]
    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_closed_without_a_response(#[case] wait: Duration) {
        let router = Router::new().route(
            "/hello",
            get(move || async move {
                sleep(wait).await;
                "hello"
            }),
        );
        let limits = ConnectionLimits {
            keep_alive: LIMITS.keep_alive,
            request_headers: ANSWER_TIMEOUT * 2,
            ..LIMITS
        };
        let server = Server::start(router, limits);
        let mut stream = server.connect().await;
        stream.write_all(HELLO).await.expect("request");
        let response = read_hello(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(read_to_close(&mut stream).await, "");
        server.stop().await;
    }

    /// Each request starts the wait over, and the wrapped body keeps its length.
    #[tokio::test]
    async fn a_connection_in_use_outlives_the_wait_for_a_request() {
        let limits = ConnectionLimits {
            keep_alive: Duration::from_secs(2),
            ..LIMITS
        };
        let server = Server::start(hello(), limits);
        let mut stream = server.connect().await;
        for _ in 0..4 {
            sleep(Duration::from_millis(800)).await;
            stream.write_all(HELLO).await.expect("request");
            let response = read_hello(&mut stream).await;
            assert!(response.contains("content-length: 5"), "{response}");
        }
        server.stop().await;
    }

    #[rstest]
    #[case::mid_request_line(&b"GET /hel"[..])]
    #[case::mid_headers(&b"GET /hello HTTP/1.1\r\nHost: x\r\nX-Slow: "[..])]
    #[case::before_the_version_sniff_has_its_bytes(&b"G"[..])]
    #[tokio::test]
    async fn a_stalled_request_is_answered_408(#[case] sent: &[u8]) {
        let server = Server::start(hello(), LIMITS);
        let mut stream = server.connect().await;
        stream.write_all(sent).await.expect("request");
        let response = read_to_close(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
        assert!(response.contains("connection: close"), "{response}");
        server.stop().await;
    }

    /// Blank lines do not start the header limit: `ParseRequest` skips them, so
    /// the connection is still waiting for a request.
    #[tokio::test]
    async fn blank_lines_before_a_request_are_not_a_request() {
        let limits = ConnectionLimits {
            keep_alive: LIMITS.keep_alive,
            request_headers: ANSWER_TIMEOUT * 2,
            ..LIMITS
        };
        let server = Server::start(hello(), limits);
        let mut stream = server.connect().await;
        stream.write_all(b"\r\n\r\n").await.expect("blank lines");
        assert_eq!(read_to_close(&mut stream).await, "");
        server.stop().await;
    }

    /// A body stalled for longer than any limit, as a paused player's is: the
    /// server waits on the handler, not the peer.
    #[tokio::test]
    async fn a_response_that_pauses_is_not_cut() {
        let router = Router::new().route(
            "/stream",
            get(|| async {
                let chunks = futures_util::stream::unfold(0, |sent| async move {
                    if sent == 3 {
                        return None;
                    }
                    sleep(PAST_LIMITS).await;
                    Some((Ok::<_, std::io::Error>(format!("chunk{sent}")), sent + 1))
                });
                Body::from_stream(chunks)
            }),
        );
        let server = Server::start(router, ALL_LIMITS);
        let mut stream = server.connect().await;
        stream
            .write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .expect("request");
        let response = read_to_close(&mut stream).await;
        for chunk in ["chunk0", "chunk1", "chunk2"] {
            assert!(response.contains(chunk), "{response}");
        }
        assert!(response.ends_with("0\r\n\r\n"), "{response}");
        server.stop().await;
    }

    #[tokio::test]
    async fn a_slow_handler_is_not_cut() {
        let router = Router::new().route(
            "/hello",
            get(|| async {
                sleep(PAST_LIMITS).await;
                "hello"
            }),
        );
        let server = Server::start(router, ALL_LIMITS);
        let mut stream = server.connect().await;
        stream.write_all(HELLO).await.expect("request");
        let response = read_hello(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        server.stop().await;
    }

    /// A body hyper takes in one go is still on its way when the request ends,
    /// and the wait for the next request starts once it has gone.
    #[tokio::test]
    async fn a_response_the_peer_is_slow_to_read_is_not_cut() {
        const SIZE: usize = 16 * 1024 * 1024;
        let router = Router::new().route("/big", get(|| async { vec![b'x'; SIZE] }));
        let server = Server::start(router, LIMITS);
        // A small window, for the body to outgrow what the kernels hold.
        let socket = tokio::net::TcpSocket::new_v4().expect("socket");
        socket.set_recv_buffer_size(4096).expect("buffer size");
        let mut stream = socket.connect(server.address).await.expect("connect");
        stream
            .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("request");
        sleep(PAST_LIMITS).await;
        let response = read_to_close(&mut stream).await;
        let (_, body) = response.split_once("\r\n\r\n").expect("a head");
        assert_eq!(body.len(), SIZE);
        server.stop().await;
    }

    /// An empty response ends its request as a body does.
    #[tokio::test]
    async fn a_connection_idle_after_an_empty_response_is_closed() {
        let router = Router::new().route(
            "/empty",
            get(|| async { axum::http::StatusCode::NO_CONTENT }),
        );
        let server = Server::start(router, LIMITS);
        let mut stream = server.connect().await;
        stream
            .write_all(b"GET /empty HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("request");
        let response = read_to_close(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert!(response.ends_with("\r\n\r\n"), "{response}");
        assert!(!response.contains("408"), "{response}");
        server.stop().await;
    }

    /// A handler that answers without reading the body: hyper closes.
    #[tokio::test]
    async fn an_early_response_to_an_unfinished_body_is_delivered() {
        let router = Router::new().route("/upload", post(|| async { "refused" }));
        let server = Server::start(router, LIMITS);
        let mut stream = server.connect().await;
        stream
            .write_all(b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\n\r\nabc")
            .await
            .expect("request");
        let response = read_to_close(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("refused"), "{response}");
        server.stop().await;
    }

    /// An upload stalled for longer than either wait for a request, within the
    /// body rate's grace period.
    #[tokio::test]
    async fn an_upload_that_pauses_is_not_cut() {
        let router = Router::new().route(
            "/upload",
            post(|body: String| async move { format!("got {}", body.len()) }),
        );
        let server = Server::start(router, LIMITS);
        let mut stream = server.connect().await;
        stream
            .write_all(
                b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\n\
                  Connection: close\r\n\r\nabc",
            )
            .await
            .expect("head");
        for part in [b"def", b"ghi"] {
            sleep(PAST_LIMITS).await;
            stream.write_all(part).await.expect("body");
        }
        let response = read_to_close(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("got 9"), "{response}");
        server.stop().await;
    }

    /// Headers with a `Content-Length`, a few body bytes and then nothing: the
    /// hold gl-1074 found on `POST /Users/AuthenticateByName`.
    #[tokio::test]
    async fn a_stalled_request_body_is_answered_500_and_closed() {
        let router = Router::new()
            .route(
                "/upload",
                post(|body: String| async move { format!("got {}", body.len()) }),
            )
            .layer(axum::middleware::map_response(
                |mut response: axum::response::Response| async move {
                    response
                        .headers_mut()
                        .insert("x-outer", axum::http::HeaderValue::from_static("kept"));
                    response
                },
            ));
        let server = Server::start(router, RATE_LIMITS);
        let mut stream = server.connect().await;
        stream
            .write_all(b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\nabc")
            .await
            .expect("request");
        let response = read_to_close(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 500"), "{response}");
        assert!(
            response.contains("content-type: text/plain\r\n"),
            "{response}"
        );
        assert!(response.contains("content-length: 25\r\n"), "{response}");
        assert!(response.contains("x-outer: kept\r\n"), "{response}");
        assert!(
            response.ends_with("Error processing request."),
            "{response}"
        );
        server.stop().await;
    }

    /// A large response its peer never reads, past what the kernels hold.
    #[tokio::test]
    async fn a_response_the_peer_stops_reading_is_aborted() {
        const SIZE: usize = 64 * 1024 * 1024;
        let router = Router::new().route("/big", get(|| async { vec![b'x'; SIZE] }));
        let server = Server::start(router, RATE_LIMITS);
        let socket = tokio::net::TcpSocket::new_v4().expect("socket");
        socket.set_recv_buffer_size(4096).expect("buffer size");
        let mut stream = socket.connect(server.address).await.expect("connect");
        stream
            .write_all(b"GET /big HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("request");
        sleep(Duration::from_secs(2)).await;
        let mut received = Vec::new();
        let read = timeout(ANSWER_TIMEOUT, stream.read_to_end(&mut received))
            .await
            .expect("the server closes the connection");
        assert!(
            read.is_err() || received.len() < SIZE,
            "the whole response arrived: {} bytes",
            received.len()
        );
        server.stop().await;
    }

    /// A WebSocket silent for longer than any limit still answers: nothing
    /// waits on its peer.
    #[tokio::test]
    async fn an_upgraded_socket_is_not_cut() {
        let router = Router::new().route(
            "/socket",
            get(|upgrade: WebSocketUpgrade| async move {
                upgrade
                    .on_upgrade(|mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if let Message::Text(text) = message {
                                let _ = socket.send(Message::Text(text)).await;
                            }
                        }
                    })
                    .into_response()
            }),
        );
        let server = Server::start(router, ALL_LIMITS);
        let mut stream = server.connect().await;
        stream
            .write_all(
                b"GET /socket HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\n\
                  Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .expect("upgrade");
        let mut received = Vec::new();
        let mut chunk = [0; 1024];
        while !received.ends_with(b"\r\n\r\n") {
            let read = timeout(ANSWER_TIMEOUT, stream.read(&mut chunk))
                .await
                .expect("answered in time")
                .expect("read");
            assert_ne!(read, 0, "closed during the handshake");
            received.extend_from_slice(&chunk[..read]);
        }
        assert!(received.starts_with(b"HTTP/1.1 101"), "{received:?}");

        sleep(PAST_LIMITS).await;

        // A masked text frame, with the all-zero mask that leaves it readable.
        stream
            .write_all(b"\x81\x82\0\0\0\0hi")
            .await
            .expect("frame");
        let mut echoed = [0; 4];
        timeout(ANSWER_TIMEOUT, stream.read_exact(&mut echoed))
            .await
            .expect("answered in time")
            .expect("the socket is open");
        assert_eq!(&echoed, b"\x81\x02hi");
        server.stop().await;
    }

    /// Reads from `io` until `elapsed` of paused time has passed, which must
    /// not end the read, and then for `grace` more, which must.
    async fn assert_ends_after(
        io: &mut LimitedIo<DuplexStream>,
        elapsed: Duration,
        grace: Duration,
    ) -> std::io::Error {
        let mut buffer = [0; 16];
        assert!(
            timeout(elapsed, io.read(&mut buffer)).await.is_err(),
            "ended before {elapsed:?}"
        );
        timeout(grace, io.read(&mut buffer))
            .await
            .expect("ended in time")
            .expect_err("timed out")
    }

    /// The values themselves, on the paused clock.
    #[tokio::test(start_paused = true)]
    async fn the_wait_for_a_request_is_kestrels_130_seconds() {
        let (_peer, io) = tokio::io::duplex(64);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let error =
            assert_ends_after(&mut io, Duration::from_secs(129), Duration::from_secs(2)).await;
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_for_headers_is_kestrels_30_seconds_from_the_first_byte() {
        let (mut peer, io) = tokio::io::duplex(256);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let mut buffer = [0; 16];
        // Most of the keep-alive wait passes first: it has no bearing.
        assert!(
            timeout(Duration::from_secs(100), io.read(&mut buffer))
                .await
                .is_err()
        );
        peer.write_all(b"G").await.expect("first byte");
        assert_eq!(io.read(&mut buffer).await.expect("read"), 1);

        let error =
            assert_ends_after(&mut io, Duration::from_secs(29), Duration::from_secs(2)).await;
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let mut answered = String::new();
        drop(io);
        peer.read_to_string(&mut answered).await.expect("answer");
        assert!(answered.starts_with("HTTP/1.1 408"), "{answered}");
    }

    /// A trickle of bytes never extends the limit, which is the point of it.
    #[tokio::test(start_paused = true)]
    async fn a_trickle_of_header_bytes_does_not_extend_the_limit() {
        let (mut peer, io) = tokio::io::duplex(256);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let mut buffer = [0; 16];
        for _ in 0..3 {
            peer.write_all(b"G").await.expect("byte");
            assert_eq!(io.read(&mut buffer).await.expect("read"), 1);
            assert!(
                timeout(Duration::from_secs(9), io.read(&mut buffer))
                    .await
                    .is_err()
            );
        }
        let error = assert_ends_after(&mut io, Duration::from_secs(2), Duration::from_secs(2));
        assert_eq!(error.await.kind(), std::io::ErrorKind::TimedOut);
    }

    /// Nor do blank lines extend the wait for a request.
    #[tokio::test(start_paused = true)]
    async fn blank_lines_do_not_extend_the_wait_for_a_request() {
        let (mut peer, io) = tokio::io::duplex(256);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let mut buffer = [0; 16];
        assert!(
            timeout(Duration::from_secs(100), io.read(&mut buffer))
                .await
                .is_err()
        );
        peer.write_all(b"\r\n").await.expect("blank line");
        assert_eq!(io.read(&mut buffer).await.expect("read"), 2);
        let error = assert_ends_after(&mut io, Duration::from_secs(29), Duration::from_secs(2));
        assert_eq!(error.await.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A running request stands the limits aside; ending it starts the wait.
    #[tokio::test(start_paused = true)]
    async fn no_limit_runs_during_a_request_and_the_wait_starts_after_it() {
        let (mut peer, io) = tokio::io::duplex(64);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let mut buffer = [0; 64];
        peer.write_all(b"GET / HTTP/1.1\r\n\r\n")
            .await
            .expect("head");
        assert_eq!(io.read(&mut buffer).await.expect("read"), 18);
        let request = io.requests().start();
        assert!(
            timeout(Duration::from_hours(1), io.read(&mut buffer))
                .await
                .is_err(),
            "nothing ends a running request"
        );
        drop(request);
        let error =
            assert_ends_after(&mut io, Duration::from_secs(129), Duration::from_secs(2)).await;
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    /// An upload trickled in at two and a half times the rate, for three
    /// times its grace period. The grace period is long next to the gaps, so a
    /// stalled runner does not cut it.
    #[tokio::test]
    async fn an_upload_above_the_rate_is_not_cut() {
        let router = Router::new().route(
            "/upload",
            post(|body: String| async move { format!("got {}", body.len()) }),
        );
        let limits = ConnectionLimits {
            min_request_body_rate: MinDataRate {
                bytes_per_second: 240,
                grace: Duration::from_secs(1),
            },
            ..RATE_LIMITS
        };
        let server = Server::start(router, limits);
        let mut stream = server.connect().await;
        stream
            .write_all(
                b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 1800\r\n\
                  Connection: close\r\n\r\n",
            )
            .await
            .expect("head");
        // 60 bytes every 100 ms: 600 bytes a second, for three seconds.
        for _ in 0..30 {
            stream.write_all(&[b'x'; 60]).await.expect("body");
            sleep(Duration::from_millis(100)).await;
        }
        let response = read_to_close(&mut stream).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("got 1800"), "{response}");
        server.stop().await;
    }

    /// A response that has started, for the grace period of its first write,
    /// kept alive by what is returned.
    fn start_response<I>(io: &LimitedIo<I>) -> axum::response::Response {
        io.requests()
            .start()
            .finish(axum::response::Response::new(Body::empty()))
    }

    /// Writes to `io`, whose peer reads nothing more, until `elapsed` of
    /// paused time has passed, which must not end the write, and then for
    /// `grace` more, which must.
    async fn assert_write_ends_after(
        io: &mut LimitedIo<DuplexStream>,
        elapsed: Duration,
        grace: Duration,
    ) -> std::io::Error {
        assert!(
            timeout(elapsed, io.write_all(b"more")).await.is_err(),
            "ended before {elapsed:?}"
        );
        timeout(grace, io.write_all(b"more"))
            .await
            .expect("ended in time")
            .expect_err("timed out")
    }

    /// The peer's side holds ten seconds' worth at 240 bytes a second, and
    /// takes nothing more.
    #[tokio::test(start_paused = true)]
    async fn the_response_rate_is_kestrels_240_bytes_a_second() {
        let (_peer, io) = tokio::io::duplex(2400);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let _response = start_response(&io);
        io.write_all(&[b'x'; 2400]).await.expect("taken");
        let error = assert_write_ends_after(
            &mut io,
            Duration::from_millis(9900),
            Duration::from_millis(200),
        );
        assert_eq!(error.await.kind(), std::io::ErrorKind::TimedOut);
    }

    /// However little the peer's side holds.
    #[tokio::test(start_paused = true)]
    async fn a_write_has_kestrels_5_second_grace_period() {
        let (_peer, io) = tokio::io::duplex(24);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let _response = start_response(&io);
        io.write_all(&[b'x'; 24]).await.expect("taken");
        let error = assert_write_ends_after(
            &mut io,
            Duration::from_millis(4900),
            Duration::from_millis(200),
        );
        assert_eq!(error.await.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A paused player: what it took faster than the rate leaves its spare
    /// time to the bytes after (`BytesWrittenToBuffer`'s accumulated timeout).
    #[tokio::test(start_paused = true)]
    async fn a_paused_peer_has_the_time_of_every_byte_it_took() {
        let (mut peer, io) = tokio::io::duplex(24_000);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let _response = start_response(&io);
        let mut taken = vec![0; 24_000];
        io.write_all(&taken).await.expect("taken");
        sleep(Duration::from_secs(1)).await;
        peer.read_exact(&mut taken).await.expect("read");
        io.write_all(&taken).await.expect("taken");
        // 100 seconds for each 24,000 bytes, from the first write.
        let error =
            assert_write_ends_after(&mut io, Duration::from_secs(198), Duration::from_secs(2));
        assert_eq!(error.await.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A peer taking a few bytes every few seconds, below the rate, never
    /// earns a fresh grace period.
    #[tokio::test(start_paused = true)]
    async fn a_trickle_below_the_rate_is_cut() {
        let (mut peer, io) = tokio::io::duplex(24);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let _response = start_response(&io);
        let reader = tokio::spawn(async move {
            let mut taken = [0; 24];
            loop {
                sleep(Duration::from_secs(4)).await;
                if peer.read_exact(&mut taken).await.is_err() {
                    break;
                }
            }
        });
        let started = tokio::time::Instant::now();
        let error = timeout(Duration::from_secs(60), io.write_all(&[b'x'; 2400]))
            .await
            .expect("ended in time")
            .expect_err("timed out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        // The grace period, and the 24 bytes the peer took at 4 s.
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
        drop(io);
        reader.await.expect("reader");
    }

    /// A WebSocket message after the peer took everything in its time: the
    /// write starts afresh, as each of Kestrel's flushes does.
    #[tokio::test(start_paused = true)]
    async fn a_write_after_the_peer_caught_up_has_the_grace_period_again() {
        let (mut peer, io) = tokio::io::duplex(24);
        let mut io = LimitedIo::new(io, KESTREL_LIMITS);
        let _response = start_response(&io);
        let mut taken = [0; 24];
        io.write_all(&taken).await.expect("taken");
        peer.read_exact(&mut taken).await.expect("read");
        sleep(Duration::from_secs(60)).await;
        io.write_all(&taken).await.expect("taken");
        let error = assert_write_ends_after(
            &mut io,
            Duration::from_millis(4900),
            Duration::from_millis(200),
        );
        assert_eq!(error.await.kind(), std::io::ErrorKind::TimedOut);
    }

    /// A request body fed through a channel, under Kestrel's rate.
    fn timed_body() -> (
        tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
        TimedRequestBody,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let chunks = futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|chunk| (chunk, receiver))
        });
        let timed_out = std::sync::Arc::default();
        let body = TimedRequestBody::new(
            Body::from_stream(chunks),
            KESTREL_MIN_DATA_RATE,
            std::sync::Arc::clone(&timed_out),
        );
        (sender, body, timed_out)
    }

    /// The next frame of `body`, as a handler reads it.
    async fn next_frame(
        body: &mut TimedRequestBody,
    ) -> Option<Result<http_body::Frame<axum::body::Bytes>, axum::Error>> {
        use axum::body::HttpBody as _;
        futures_util::future::poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx)).await
    }

    /// Reads `body` until `elapsed` of paused time has passed, which must not
    /// end the read, and then for `grace` more, which must fail it.
    async fn assert_read_fails_after(
        body: &mut TimedRequestBody,
        elapsed: Duration,
        grace: Duration,
    ) {
        assert!(
            timeout(elapsed, next_frame(body)).await.is_err(),
            "ended before {elapsed:?}"
        );
        let frame = timeout(grace, next_frame(body))
            .await
            .expect("ended in time");
        assert!(matches!(frame, Some(Err(_))), "{frame:?}");
    }

    /// 2,400 bytes, then nothing: ten seconds' worth at 240 bytes a second.
    #[tokio::test(start_paused = true)]
    async fn the_request_body_rate_is_kestrels_240_bytes_a_second() {
        let (sender, mut body, timed_out) = timed_body();
        sender
            .send(Ok(vec![b'x'; 2400].into()))
            .await
            .expect("send");
        assert!(matches!(next_frame(&mut body).await, Some(Ok(_))));
        assert_read_fails_after(
            &mut body,
            Duration::from_millis(9900),
            Duration::from_millis(200),
        )
        .await;
        assert!(timed_out.load(std::sync::atomic::Ordering::Relaxed));
        let after = next_frame(&mut body).await;
        assert!(
            matches!(after, Some(Err(_))),
            "every later read fails: {after:?}"
        );
    }

    /// Time the handler spends on anything but waiting for the body does not
    /// count against it, however long.
    #[tokio::test(start_paused = true)]
    async fn only_the_wait_for_the_body_counts() {
        let (sender, mut body, _timed_out) = timed_body();
        sender.send(Ok(vec![b'x'; 24].into())).await.expect("send");
        assert!(matches!(next_frame(&mut body).await, Some(Ok(_))));
        sleep(Duration::from_hours(1)).await;
        // The grace period, from the wait's start.
        assert_read_fails_after(
            &mut body,
            Duration::from_millis(4900),
            Duration::from_millis(200),
        )
        .await;
    }

    /// 120 bytes every 400 ms, 300 bytes a second, for a minute.
    #[tokio::test(start_paused = true)]
    async fn a_request_body_above_the_rate_is_not_cut() {
        let (sender, mut body, timed_out) = timed_body();
        let feeder = tokio::spawn(async move {
            for _ in 0..150 {
                sleep(Duration::from_millis(400)).await;
                sender.send(Ok(vec![b'x'; 120].into())).await.expect("send");
            }
        });
        let mut received = 0;
        while let Some(frame) = next_frame(&mut body).await {
            let frame = frame.expect("not cut");
            received += frame.data_ref().map_or(0, axum::body::Bytes::len);
        }
        assert_eq!(received, 150 * 120);
        assert!(!timed_out.load(std::sync::atomic::Ordering::Relaxed));
        feeder.await.expect("feeder");
    }

    #[test]
    fn every_data_rate_divergence_names_its_fix_path() {
        for divergence in DATA_RATE_DIVERGENCES {
            assert!(divergence.contains("Fix path: "), "{divergence}");
        }
    }

    /// A connection whose shutdown waits on its peer for good, as a TLS
    /// `close_notify` behind a full send buffer does.
    struct ShutdownPends(DuplexStream);

    impl tokio::io::AsyncRead for ShutdownPends {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for ShutdownPends {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    /// A shutdown that waits on the peer is timed as a write is.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_the_peer_holds_up_is_cut_after_the_grace_period() {
        let (_peer, io) = tokio::io::duplex(24);
        let mut io = LimitedIo::new(ShutdownPends(io), KESTREL_LIMITS);
        let _response = start_response(&io);
        assert!(
            timeout(Duration::from_millis(4900), io.shutdown())
                .await
                .is_err(),
            "ended before the grace period"
        );
        let error = timeout(Duration::from_millis(200), io.shutdown())
            .await
            .expect("ended in time")
            .expect_err("timed out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
}
