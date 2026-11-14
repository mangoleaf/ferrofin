//! A tiny dependency-free HTTP mock server for testing the remote clients'
//! request → parse paths without touching the network. Serves a fixed JSON body
//! per request whose path contains a registered key.
//!
//! Test-only. Not a general HTTP server: it reads one buffer of the request,
//! routes by a path substring, and writes a complete `Connection: close`
//! response — enough for the provider clients' `GET`/`POST` calls.

#![cfg(test)]

use std::net::Ipv6Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::watch;

/// A loopback listener for a mock server, with its port's hold on `::1`
/// ([`loopback_listener`]). Image downloads refuse loopback unless the
/// server's policy lets them reach it (gl-1096), so binding one lets them: a
/// test serving images here needs nothing else. A test of the refusal itself
/// gives its client its own policy (`guarded_http::GuardedHttp::new`).
async fn bind() -> (TcpListener, Option<TcpSocket>) {
    crate::destination::test_support::allow_loopback_images();
    loopback_listener().await
}

/// A listener on `127.0.0.1`, and a socket that holds the same port on `::1`
/// (gl-1387). Keep the socket as long as the listener.
///
/// A client given `localhost` tries `::1` first. The held port is bound and
/// never listening, so it refuses that connection at once, and the client
/// goes on to `127.0.0.1`. A free port there could be another test process's
/// listener, or a host service that listens on IPv6 only, and that one would
/// get the request instead.
///
/// The hold is exclusive only while its socket does not set `SO_REUSEADDR`,
/// which a `TcpSocket` does not. The socket is `None` on a host with no IPv6
/// loopback, where no listener can take the port either.
pub(crate) async fn loopback_listener() -> (TcpListener, Option<TcpSocket>) {
    loop {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let port = listener.local_addr().expect("addr").port();
        let Ok(held) = TcpSocket::new_v6() else {
            return (listener, None);
        };
        match held.bind((Ipv6Addr::LOCALHOST, port).into()) {
            Ok(()) => return (listener, Some(held)),
            // Already taken on `::1`: take another port.
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(_) => return (listener, None),
        }
    }
}

/// A running mock server; its [`base_url`](Self::base_url) is handed to a client.
/// The background task is aborted on drop.
pub struct MockServer {
    /// The `http://127.0.0.1:{port}` base to point a client at.
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
    requests: Arc<AtomicUsize>,
    targets: Arc<Mutex<Vec<String>>>,
    /// The head of each request a [`start_raw`](Self::start_raw) server had,
    /// lower-cased.
    heads: Arc<Mutex<Vec<String>>>,
    /// The port held on `::1` ([`loopback_listener`]).
    _ipv6: Option<TcpSocket>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockServer {
    /// Starts a server that, for each request, returns the JSON body of the
    /// first `(path_substring, body)` route whose substring the request path
    /// contains; unmatched requests get `{}`.
    pub async fn start(routes: Vec<(&'static str, String)>) -> Self {
        Self::with_headers(routes, "").await
    }

    /// Like start, but attach quota headers to each successful response.
    pub async fn with_headers(routes: Vec<(&'static str, String)>, headers: &'static str) -> Self {
        Self::serve(routes, headers, None).await
    }

    /// Like [`always`](Self::always), but each request is answered only once
    /// `gate` has counted `open_at` requests, across every server sharing it:
    /// servers on one gate answer only clients whose requests overlap.
    pub async fn gated(body: &str, gate: Arc<watch::Sender<usize>>, open_at: usize) -> Self {
        Self::serve(vec![("/", body.to_owned())], "", Some((gate, open_at))).await
    }

    async fn serve(
        routes: Vec<(&'static str, String)>,
        headers: &'static str,
        gate: Option<(Arc<watch::Sender<usize>>, usize)>,
    ) -> Self {
        let routes = Arc::new(routes);
        let targets = Arc::new(Mutex::new(Vec::new()));
        let (listener, ipv6) = bind().await;
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn({
            let targets = Arc::clone(&targets);
            async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let (routes, targets) = (Arc::clone(&routes), Arc::clone(&targets));
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 16 * 1024];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("");
                        targets.lock().expect("lock").push(path.to_owned());
                        if let Some((gate, open_at)) = gate {
                            gate.send_modify(|seen| *seen += 1);
                            let _ = gate.subscribe().wait_for(|seen| *seen >= open_at).await;
                        }
                        let body = routes
                            .iter()
                            .find(|(k, _)| path.contains(k))
                            .map_or_else(|| "{}".to_owned(), |(_, v)| v.clone());
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            requests: Arc::default(),
            targets,
            heads: Arc::default(),
            _ipv6: ipv6,
        }
    }

    /// Convenience: a server returning `body` for any request.
    pub async fn always(body: &str) -> Self {
        Self::start(vec![("/", body.to_owned())]).await
    }

    /// Like [`start`](Self::start) but each route also names the
    /// `Content-Type` to answer with — needed by the artwork writer, whose
    /// whole job is to believe the RESPONSE's media type rather than the URL's
    /// suffix. An unmatched request gets `404`.
    pub async fn start_typed(routes: Vec<(&'static str, &'static str, Vec<u8>)>) -> Self {
        let routes = Arc::new(routes);
        let (listener, ipv6) = bind().await;
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("");
                    let mut resp = match routes.iter().find(|(k, _, _)| path.contains(k)) {
                        Some((_, content_type, body)) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes(),
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    if let Some((_, _, body)) = routes.iter().find(|(k, _, _)| path.contains(k)) {
                        resp.extend_from_slice(body);
                    }
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            requests: Arc::default(),
            targets: Arc::default(),
            heads: Arc::default(),
            _ipv6: ipv6,
        }
    }

    /// A server that answers the nth request with the nth of `responses`,
    /// complete raw HTTP, and every later one with the last of them. With
    /// `stall`, it then keeps the connection open and sends nothing more.
    async fn start_raw_inner(responses: Vec<Vec<u8>>, stall: bool) -> Self {
        let responses = Arc::new(responses);
        let requests = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let (listener, ipv6) = bind().await;
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn({
            let (requests, heads) = (Arc::clone(&requests), Arc::clone(&heads));
            async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let (responses, requests) = (Arc::clone(&responses), Arc::clone(&requests));
                    let heads = Arc::clone(&heads);
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 16 * 1024];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                        heads.lock().expect("lock").push(head);
                        let nth = requests.fetch_add(1, Ordering::SeqCst);
                        let response = &responses[nth.min(responses.len() - 1)];
                        let _ = sock.write_all(response).await;
                        if stall {
                            let _ = sock.read(&mut buf).await;
                        }
                        let _ = sock.shutdown().await;
                    });
                }
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            requests,
            targets: Arc::default(),
            heads,
            _ipv6: ipv6,
        }
    }

    /// See [`start_raw_inner`](Self::start_raw_inner).
    pub async fn start_raw(responses: Vec<Vec<u8>>) -> Self {
        Self::start_raw_inner(responses, false).await
    }

    /// See [`start_raw_inner`](Self::start_raw_inner).
    pub async fn start_stalled(response: Vec<u8>) -> Self {
        Self::start_raw_inner(vec![response], true).await
    }

    /// The requests a [`start_raw`](Self::start_raw) server has had.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// The head of each request a [`start_raw`](Self::start_raw) server has
    /// had, lower-cased, in order of arrival.
    pub fn heads(&self) -> Vec<String> {
        self.heads.lock().expect("lock").clone()
    }

    /// The decoded `key` query parameter of each request to `path`, in order of
    /// arrival. Only [`start`](Self::start) and
    /// [`with_headers`](Self::with_headers) servers record their requests.
    pub fn query_values(&self, path: &str, key: &str) -> Vec<String> {
        let targets = self.targets.lock().expect("lock");
        targets
            .iter()
            .filter_map(|target| reqwest::Url::parse(&format!("{}{target}", self.base_url)).ok())
            .filter(|url| url.path() == path)
            .filter_map(|url| {
                url.query_pairs()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.into_owned())
            })
            .collect()
    }

    /// A raw `200` response with `headers`, closed after `body`.
    pub fn raw_ok(headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 200 OK\r\n{headers}Connection: close\r\n\r\n").into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// A raw response with no body, of `status`.
    pub fn raw_status(status: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    /// gl-1387: every kind of mock holds its port on `::1`, which a client
    /// given `localhost` tries first. No other listener can take it, and a
    /// connection there is refused at once, so the client goes on to
    /// `127.0.0.1`. (A host with no IPv6 loopback has nothing to hold.)
    #[tokio::test]
    async fn a_mock_holds_its_port_on_ipv6_loopback() {
        if std::net::TcpListener::bind("[::1]:0").is_err() {
            return;
        }
        let mocks = [
            MockServer::always("{}").await,
            MockServer::start_typed(vec![("/", "image/png", vec![1])]).await,
            MockServer::start_raw(vec![MockServer::raw_ok("", b"ok")]).await,
        ];
        for mock in &mocks {
            let port: u16 = mock
                .base_url
                .rsplit(':')
                .next()
                .and_then(|port| port.parse().ok())
                .expect("a port");
            let taken =
                std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)).expect_err("port is held");
            assert_eq!(taken.kind(), ErrorKind::AddrInUse, "{}", mock.base_url);
            let refused = tokio::net::TcpStream::connect((Ipv6Addr::LOCALHOST, port))
                .await
                .expect_err("nothing answers on ::1");
            assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
        }
    }
}
