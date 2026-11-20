//! A fake OpenID Connect identity provider for tests, so CI needs no network
//! (D12): an HTTP server on `127.0.0.1` that answers each path with the
//! [`FakeResponse`] set for it and records every request.
//!
//! [`FakeIdentityProvider::start`] serves a discovery document and a key set
//! under an issuer with a path, as a Keycloak realm has. Tests use it with
//! `AllowHttpIssuer` on, since it speaks plain HTTP. A test changes any path
//! with [`set`](FakeIdentityProvider::set): another discovery document, a
//! redirect, a slow or oversized answer, or an endpoint the defaults lack.
//!
//! Compiled for this crate's tests, and for other crates' through the
//! `test-util` feature (from `[dev-dependencies]` only).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// The fake issuer's path: the issuer is `http://127.0.0.1:{port}` followed by
/// it, as a Keycloak realm's issuer ends in `/realms/{name}`.
pub const ISSUER_PATH: &str = "/realms/ferrofin-test";

/// Where the fake serves its key set, below the issuer.
pub const JWKS_PATH: &str = "/protocol/openid-connect/certs";

/// The `kid` of the key the default key set holds.
pub const KEY_ID: &str = "fake-rs256";

/// The default key set's one key: the public half of a throwaway 2048-bit
/// RSA key generated for these tests, whose private half was discarded. A
/// test that signs tokens serves a key set of its own key.
const KEY_MODULUS: &str = "wVif7TjZyx84wuxUNqzyx1Kd4Weyk2YIo67Yit7Nf9HNnk7iYbW_4Vl7D5uLHS3ARglFGOfQ1ry_\
     X8bYO1IfxcoTqMdiBEnurbqCWBJpKUwaV7KLhkNLxKTd1v0Z2aXEVoPhuQS_tJxEnkGy4--Tc5Oj4_PfmzY_hpGpholWtCpZ\
     PN3LJxmqksnKSZKaGpxdJf52AjPOQt_5Nnw0TRm8N3qW60SDyNxcQJ1WkSEvhXh1qg9On5nv6tpstI9Xgp6iDAaGF1nAar3kq\
     VetiBZjcKwSsdKy28QMiiauloUkOBs42o2h7uvsVmt0YZ_Tk1_MMJqNLZ7uNgvSgdwLcwHlyQ";

/// What the fake answers on one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeResponse {
    /// The status code.
    pub status: u16,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
    /// How long the fake waits before it answers.
    pub delay: Duration,
    /// Whether the body is streamed in chunks with no `Content-Length`, so the
    /// client learns its length only by reading it.
    pub chunked: bool,
}

impl FakeResponse {
    /// `200` with `value` as its `application/json` body.
    #[must_use]
    pub fn json(value: &serde_json::Value) -> Self {
        Self::status(200).with_body("application/json", value.to_string().into_bytes())
    }

    /// `status` with no body.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
            chunked: false,
        }
    }

    /// `302` to `location`.
    #[must_use]
    pub fn redirect(location: &str) -> Self {
        Self::status(302).with_header("location", location)
    }

    /// This answer with `body` as its body, of type `content_type`.
    #[must_use]
    pub fn with_body(self, content_type: &str, body: Vec<u8>) -> Self {
        Self { body, ..self }.with_header("content-type", content_type)
    }

    /// This answer with one more header.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// This answer, sent after `delay`.
    #[must_use]
    pub fn delayed(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }

    /// This answer, its body streamed with no `Content-Length`.
    #[must_use]
    pub fn chunked(self) -> Self {
        Self {
            chunked: true,
            ..self
        }
    }
}

/// A request the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRequest {
    /// The method.
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The query, when there is one.
    pub query: Option<String>,
    /// The headers, in order, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// What the server task and the test share.
#[derive(Default)]
struct Shared {
    routes: HashMap<String, FakeResponse>,
    requests: Vec<FakeRequest>,
}

/// The fake identity provider, served until it is dropped.
pub struct FakeIdentityProvider {
    /// `http://127.0.0.1:{port}`.
    base: String,
    shared: Arc<Mutex<Shared>>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeIdentityProvider {
    /// Starts the fake on a free port of `127.0.0.1`, serving its
    /// [`discovery_document`](Self::discovery_document) and a key set of one
    /// RSA key ([`KEY_ID`]).
    ///
    /// # Panics
    ///
    /// When no port of `127.0.0.1` can be bound.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the fake identity provider");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the fake's address")
        );
        let shared = Arc::new(Mutex::new(Shared::default()));
        let app = axum::Router::new()
            .fallback(answer)
            .with_state(Arc::clone(&shared));
        let server = tokio::spawn(async move {
            // Ends only when the test drops the fake and aborts it.
            let _ = axum::serve(listener, app).await;
        });
        let fake = Self {
            base,
            shared,
            server,
        };
        fake.set(
            &fake.path("/.well-known/openid-configuration"),
            FakeResponse::json(&fake.discovery_document()),
        );
        fake.set(
            &fake.path(JWKS_PATH),
            FakeResponse::json(&serde_json::json!({
                "keys": [{
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": KEY_ID,
                    "n": KEY_MODULUS,
                    "e": "AQAB",
                }]
            })),
        );
        fake
    }

    /// The fake's issuer: `http://127.0.0.1:{port}` + [`ISSUER_PATH`].
    #[must_use]
    pub fn issuer(&self) -> String {
        self.url(ISSUER_PATH)
    }

    /// `suffix` below the issuer's path, as a path for [`set`](Self::set).
    #[must_use]
    pub fn path(&self, suffix: &str) -> String {
        format!("{ISSUER_PATH}{suffix}")
    }

    /// The fake's URL for `path`.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The discovery document the fake serves until a test sets another: the
    /// authorization code flow with PKCE `S256`, an RS256 key set, and token
    /// and userinfo endpoints, which the fake does not serve until a test
    /// sets them.
    #[must_use]
    pub fn discovery_document(&self) -> serde_json::Value {
        let issuer = self.issuer();
        serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/protocol/openid-connect/auth"),
            "token_endpoint": format!("{issuer}/protocol/openid-connect/token"),
            "userinfo_endpoint": format!("{issuer}/protocol/openid-connect/userinfo"),
            "jwks_uri": format!("{issuer}{JWKS_PATH}"),
            "response_types_supported": ["code", "id_token", "code id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "code_challenge_methods_supported": ["plain", "S256"],
        })
    }

    /// Answers `path` (with no query) with `response` from now on.
    pub fn set(&self, path: &str, response: FakeResponse) {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .routes
            .insert(path.to_owned(), response);
    }

    /// Every request received so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<FakeRequest> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .requests
            .clone()
    }
}

impl Drop for FakeIdentityProvider {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Records the request and answers it with what is set for its path, or
/// `404`.
async fn answer(
    State(shared): State<Arc<Mutex<Shared>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let response = {
        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.requests.push(FakeRequest {
            method: method.to_string(),
            path: uri.path().to_owned(),
            query: uri.query().map(str::to_owned),
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            body: body.to_vec(),
        });
        shared.routes.get(uri.path()).cloned()
    };
    let Some(response) = response else {
        return StatusCode::NOT_FOUND.into_response();
    };
    tokio::time::sleep(response.delay).await;
    let mut builder = Response::builder().status(response.status);
    for (name, value) in &response.headers {
        builder = builder.header(name, value);
    }
    let body = if response.chunked {
        let chunks: Vec<Result<Bytes, std::convert::Infallible>> = response
            .body
            .chunks(64 * 1024)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        Body::from_stream(futures_util::stream::iter(chunks))
    } else {
        Body::from(response.body)
    };
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
