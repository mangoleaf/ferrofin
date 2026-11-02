//! The two time limits Kestrel puts on an accepted connection.
//!
//! `axum::serve` gives hyper no timer, so hyper's own header-read timeout never
//! runs, and that one would measure the wait for a request as well. Kestrel
//! (`Http1Connection`, `HttpConnection.OnTimeout`) keeps the two apart, and this
//! module ports that:
//!
//! - **Between requests**, a new connection included, a peer has
//!   [`KESTREL_LIMITS`]`.keep_alive` to send the first byte of a request. Blank
//!   lines do not count. Past it the connection closes without a response.
//! - **From that byte**, it has [`KESTREL_LIMITS`]`.request_headers` to finish
//!   the request line and headers. Past it the connection is answered `408` and
//!   closes.
//! - **While a request runs**, nothing here applies: a long download, a slow
//!   upload and a slow handler are not bounded. An upgraded socket (`/socket`)
//!   has left HTTP for good, and is never bounded either.
//!
//! [`LimitedIo`] enforces this at the socket, below `axum::serve` and hyper, so
//! it also covers hyper-util's version sniffing, which reads the first bytes with
//! no timeout of its own. The socket sees the first byte of a request and
//! [`track_requests`] sees it start and end; they share a [`ConnectionPhase`].
//!
//! TODO(gl-1074): port Kestrel's limits for the running request as well, the
//! minimum body and response data rates.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};

/// How long a connection may wait for a request, and how long a request's
/// headers may take to arrive.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    /// The wait for the first byte of a request.
    pub keep_alive: Duration,
    /// The wait for the rest of the headers, from that byte.
    pub request_headers: Duration,
}

/// Kestrel's defaults, which Jellyfin does not override
/// (`KestrelServerLimits.KeepAliveTimeout` and `RequestHeadersTimeout`).
pub const KESTREL_LIMITS: ConnectionLimits = ConnectionLimits {
    keep_alive: Duration::from_secs(130),
    request_headers: Duration::from_secs(30),
};

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
    /// slow to read still has to go: no limit runs until it has.
    write_blocked: bool,
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
        }
    }

    /// The connection underneath.
    pub fn inner(&self) -> &I {
        &self.io
    }

    /// What the service stack must be told of this connection's requests.
    pub fn requests(&self) -> RequestTracker {
        RequestTracker(Arc::clone(&self.phase))
    }
}

impl<I: AsyncWrite + Unpin> LimitedIo<I> {
    /// Notes how a write went. One that got through after being blocked wakes the
    /// read, parked with no limit running, unless a request still runs: its end
    /// does that.
    fn note_write<T>(&mut self, cx: &Context<'_>, written: Poll<T>) -> Poll<T> {
        let blocked = written.is_pending();
        if self.write_blocked && !blocked && self.phase.get() != SERVING {
            cx.waker().wake_by_ref();
        }
        self.write_blocked = blocked;
        written
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
        this.note_write(cx, written)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.note_write(cx, written)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let flushed = Pin::new(&mut this.io).poll_flush(cx);
        this.note_write(cx, flushed)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// Tells a connection's [`LimitedIo`] that its requests start and end.
#[derive(Debug, Clone)]
pub struct RequestTracker(Arc<ConnectionPhase>);

impl RequestTracker {
    /// A request's headers have arrived. The limits stand aside until the
    /// returned guard is dropped.
    #[must_use]
    pub fn start(&self) -> RunningRequest {
        self.0.set(SERVING);
        RunningRequest(Arc::clone(&self.0))
    }
}

/// A request that has not finished. Dropping it ends the request, also when a
/// disconnect or a panic cuts it short.
///
/// TODO(gl-814): one phase per connection holds for HTTP/1, which runs one
/// request at a time. HTTP/2 needs a count of its running streams.
#[derive(Debug)]
pub struct RunningRequest(Arc<ConnectionPhase>);

impl RunningRequest {
    /// Ties this request to its `response`: the connection waits for its next
    /// request once hyper has finished with the body. A `101` ends HTTP on the
    /// connection, so no limit applies again.
    fn finish(self, response: Response) -> Response {
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            self.0.set(UPGRADED);
        }
        response.map(|body| {
            Body::new(TrackedBody {
                body,
                _request: self,
            })
        })
    }
}

impl Drop for RunningRequest {
    fn drop(&mut self) {
        if self.0.advance(SERVING, WAITING) {
            self.0.reader.wake();
        }
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

/// Runs `service` for a connection's requests, reporting them to `tracker`.
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
    tower::ServiceBuilder::new()
        .map_future(move |future: S::Future| {
            let request = tracker.start();
            async move {
                let response = future.await?;
                Ok::<_, std::convert::Infallible>(request.finish(response))
            }
        })
        .service(service)
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

    use super::{ConnectionLimits, KESTREL_LIMITS, LimitedIo};

    /// Long enough that only a connection nothing closes takes this long.
    const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
    /// Short limits, for what real sockets must show.
    const LIMITS: ConnectionLimits = ConnectionLimits {
        keep_alive: Duration::from_millis(300),
        request_headers: Duration::from_millis(200),
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
            request_headers: LIMITS.request_headers,
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
        };
        let server = Server::start(hello(), limits);
        let mut stream = server.connect().await;
        stream.write_all(b"\r\n\r\n").await.expect("blank lines");
        assert_eq!(read_to_close(&mut stream).await, "");
        server.stop().await;
    }

    /// A body stalled for longer than either limit, as a paused player's is.
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
        let server = Server::start(router, LIMITS);
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
        let server = Server::start(router, LIMITS);
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

    /// An upload stalled for longer than either limit.
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

    /// A WebSocket silent for longer than either limit still answers.
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
        let server = Server::start(router, LIMITS);
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
}
