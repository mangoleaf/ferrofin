//! A tiny dependency-free HTTP mock server for testing the remote clients'
//! request → parse paths without touching the network. Serves a fixed JSON body
//! per request whose path contains a registered key.
//!
//! Test-only. Not a general HTTP server: it reads one buffer of the request,
//! routes by a path substring, and writes a complete `Connection: close`
//! response — enough for the provider clients' `GET`/`POST` calls.

#![cfg(test)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// A loopback listener for a mock server. Image downloads refuse loopback
/// unless the server's policy lets them reach it (gl-1096), so binding one
/// lets them: a test serving images here needs nothing else. A test of the
/// refusal itself gives its client its own policy
/// (`image_download::ImageHttp::new`).
async fn bind() -> TcpListener {
    crate::destination::test_support::allow_loopback_images();
    TcpListener::bind("127.0.0.1:0").await.expect("bind mock")
}

/// A running mock server; its [`base_url`](Self::base_url) is handed to a client.
/// The background task is aborted on drop.
pub struct MockServer {
    /// The `http://127.0.0.1:{port}` base to point a client at.
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
    requests: Arc<AtomicUsize>,
    targets: Arc<Mutex<Vec<String>>>,
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
        let routes = Arc::new(routes);
        let targets = Arc::new(Mutex::new(Vec::new()));
        let listener = bind().await;
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn({
            let targets = Arc::clone(&targets);
            async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let (routes, targets) = (Arc::clone(&routes), Arc::clone(&targets));
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
        let listener = bind().await;
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
        }
    }

    /// A server that answers the nth request with the nth of `responses`,
    /// complete raw HTTP, and every later one with the last of them. With
    /// `stall`, it then keeps the connection open and sends nothing more.
    async fn start_raw_inner(responses: Vec<Vec<u8>>, stall: bool) -> Self {
        let responses = Arc::new(responses);
        let requests = Arc::new(AtomicUsize::new(0));
        let listener = bind().await;
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn({
            let requests = Arc::clone(&requests);
            async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let (responses, requests) = (Arc::clone(&responses), Arc::clone(&requests));
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 16 * 1024];
                        let _ = sock.read(&mut buf).await;
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
