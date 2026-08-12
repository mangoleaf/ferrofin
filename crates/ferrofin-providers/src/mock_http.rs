//! A tiny dependency-free HTTP mock server for testing the remote clients'
//! request → parse paths without touching the network. Serves a fixed JSON body
//! per request whose path contains a registered key.
//!
//! Test-only. Not a general HTTP server: it reads one buffer of the request,
//! routes by a path substring, and writes a complete `Connection: close`
//! response — enough for the provider clients' `GET`/`POST` calls.

#![cfg(test)]

use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// A running mock server; its [`base_url`](Self::base_url) is handed to a client.
/// The background task is aborted on drop.
pub struct MockServer {
    /// The `http://127.0.0.1:{port}` base to point a client at.
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
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
        let routes = Arc::new(routes);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
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
                    let body = routes
                        .iter()
                        .find(|(k, _)| path.contains(k))
                        .map_or_else(|| "{}".to_owned(), |(_, v)| v.clone());
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
        }
    }

    /// Convenience: a server returning `body` for any request.
    pub async fn always(body: &str) -> Self {
        Self::start(vec![("/", body.to_owned())]).await
    }
}
