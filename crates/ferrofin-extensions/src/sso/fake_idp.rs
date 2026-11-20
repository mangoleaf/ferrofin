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
//! It also signs a user in, as an identity provider does, on every path no
//! test has set:
//! - the authorization endpoint ([`AUTHORIZATION_PATH`]) sends the browser
//!   straight back to the request's `redirect_uri` with a one-time code, the
//!   `state` and `iss` ([`authorize`](FakeIdentityProvider::authorize) follows
//!   it as a browser would);
//! - the token endpoint ([`TOKEN_PATH`]) takes that code once, with the PKCE
//!   verifier whose `S256` challenge the authorization request carried, and
//!   answers an access token and an ID token signed with the fake's RSA key:
//!   the request's client id as its audience and its nonce;
//! - the userinfo endpoint ([`USERINFO_PATH`]) answers that access token with
//!   the user's claims.
//!
//! [`sign_in`](FakeIdentityProvider::sign_in) edits what those answer
//! ([`FakeSignIn`]): the subject and claims, each way of signing the ID token
//! wrongly, the `iss` parameter, an `error` in place of the code.
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
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openidconnect::core::{CoreHmacKey, CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey};
use openidconnect::{JsonWebKeyId, PrivateSigningKey};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// The fake issuer's path: the issuer is `http://127.0.0.1:{port}` followed by
/// it, as a Keycloak realm's issuer ends in `/realms/{name}`.
pub const ISSUER_PATH: &str = "/realms/ferrofin-test";

/// Where the fake serves its key set, below the issuer.
pub const JWKS_PATH: &str = "/protocol/openid-connect/certs";

/// The fake's authorization endpoint, below the issuer.
pub const AUTHORIZATION_PATH: &str = "/protocol/openid-connect/auth";

/// The fake's token endpoint, below the issuer.
pub const TOKEN_PATH: &str = "/protocol/openid-connect/token";

/// The fake's userinfo endpoint, below the issuer.
pub const USERINFO_PATH: &str = "/protocol/openid-connect/userinfo";

/// The subject the fake signs in by default.
pub const SUBJECT: &str = "fake-subject-0001";

/// The `preferred_username` of the default subject.
pub const USERNAME: &str = "fake-user";

/// The `kid` of the key the default key set holds.
pub const KEY_ID: &str = "fake-rs256";

/// The private half of the fake's signing key, a throwaway 2048-bit RSA key
/// generated for these tests and used nowhere else: the default key set holds
/// its public half under [`KEY_ID`], and the token endpoint signs ID tokens
/// with it.
const SIGNING_KEY_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEoQIBAAKCAQEAx8WJ6aYHEZ4gBEVL6OFpwF2gJ2wijlmYO8Ol3xPFZ3ZGAsiR
7HSspXnh5KoKmw+z7mbNGUlU6g6Rv7+Fh2WqQit3/F67FMJzcnHTOfOb+9d2O3D6
1dBFist9JTORDLHfDTOPXFgEeJV/MCtvQukvZncfkQtpQpCPQEHFWoKGQCphLDn5
uvfll09LGA5MWcxLxFg66opgI2eAv5kEwz64LHRysHbZu6cuxMMqoB7j4+oKKk3E
SjcAh8gApgPoPhgGdLu43lkWtyjgZ6tMdUS7VyMJbwOJDIE5GIZ2dyzvaHxIZ0Z9
mTpkLCvD/SrZtokhv7p6r4bpZ0ULdIWLAE58TQIDAQABAoH/REfAW5vjzqG9T7hf
ublR6+4UT2UmaoE1BzhX+WdIkZRbWZg0SB2Y5PZDv2RfO6CiX1nDwGDQJS/VoTRU
73JQP02cNSYry2yrCPLev3AEYI/EWYzEL1CAdHAY1h5J9YzWMCzvUyDCZnSyhERU
QwpwqRUONQUxdQ7z54KTcdwrBsLapRHWOWb3s45bnWLiZr7uCZzD7MwfpqPMHnHb
+2biyNx2eXTKpkl4OEOSALj/VzX4odNMsvpfepkcpL7ZngDfNgMUeIvleNXqXLNR
Cx9t95Pz7+JwFDohL5K0W9JOl04aF3gMfA5jWZm7rNs056+PTVjpbDz7oIaIWE30
90KzAoGBAO5sVmBow2uRVJxH07JjkBzLNGuId0OsOaLlNnoIFLGx/klXZm2ovi6e
hnZ74UHo3+gy0NtKb65PCXbJpXYNxZoWmR9FboEd75SzZxxNTvd5cIoAGDsm9CR3
QLPTv6JlJ25fpujucbMZ/vcRXfopXYD69mzTG7DDuKMG5NY7hKCvAoGBANZ/vxTN
2TfkQ6b/yfa+Q/3er1u1Ris+0duiFol7f0Ho6ndcOhERN6Vuslip92o08dCBl3D5
HWs3Xbltm+eedwbNpt5mzy2ufOqTvouaPYYpvm9MSUsT4VDcqBevDbr7sHJ4DljT
b9R0OEKbFOYZ612DNYfzYrhNNxhZn9KymRnDAoGBAJsRyYhA+0E+L9rJeK3gxb2E
iN6bzn4sSPxiunDaSqNcUjhfaRZPlwp5PplXbyXQEblTO7Y1Drg/Xbdt5XkH8gqL
QiItpXbDAq0jlh+5RG//AywubZQjfM0u5t1PDqfPxLqr6cI8O6Uydjs5h3cKam9W
Supyz+LDGZanCDUyVsuvAoGAXwvcL+vHBO0wzR8q7xsAJc43BfwdS6xacWgxdFo0
2Y/L1XgYgteOQM/8Vy15ZKZxQGMsJ/M7BfdSoIfhPZEKTQlzl7kWtLdMwuUU2edO
u9Rjfg3frFTy0lH0ago3m1NLrG1mbhXCx/F3vIvbhtqVYKpzXfqLE/+jGjccitB4
5uECgYB0HztTwIPYg/8ONf11A3zb4imHFGC3iCb1p5/PoMU33gBKxiam7JFFPMaQ
YeDnUrEgTmhfL7Apf8FRkenXezOk48OKWZREkzdPGRUB/g/90uPnZ0q5KIpIlWQF
AwKfBoT4nsQ91Z6svoSr7rsV9dWZn6Fgh1vqbUA9k0HvXksXsg==
-----END RSA PRIVATE KEY-----";

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

/// How the fake's token endpoint signs the ID tokens it issues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeSignature {
    /// RS256 with the fake's key, under [`KEY_ID`]: a valid signature.
    Valid,
    /// RS256 with the fake's key, under this key id, which the default key
    /// set does not hold (a key the provider rotated in).
    KeyId(String),
    /// A valid signature over other claims, so it does not verify.
    Tampered,
    /// `alg: none`, with no signature.
    Unsigned,
    /// HS256, keyed with this secret.
    Hmac(String),
}

/// What the fake's sign-in endpoints answer. As [`start`](FakeIdentityProvider::start)
/// sets it, a valid sign-in of [`SUBJECT`]; each member a test changes makes
/// one bad answer a relying party must refuse.
#[derive(Debug, Clone)]
pub struct FakeSignIn {
    /// The user's `sub`.
    pub subject: String,
    /// Merged over the ID token's claims (`iss`, `sub`, `aud` the client id,
    /// `exp` in five minutes, `iat` now, the request's `nonce`, and
    /// `preferred_username` [`USERNAME`]); a `null` member removes the claim.
    pub claims: Map<String, Value>,
    /// How the ID token is signed.
    pub signature: FakeSignature,
    /// The `iss` the authorization endpoint's redirect carries (RFC 9207);
    /// `None` leaves it out. The issuer by default.
    pub iss: Option<String>,
    /// An `error` the authorization endpoint's redirect carries in place of
    /// a code, such as `access_denied`.
    pub error: Option<String>,
    /// Merged over the userinfo answer, which holds only the `sub`.
    pub userinfo: Map<String, Value>,
}

/// An authorization request the fake answered with a code.
struct Authorization {
    client_id: String,
    redirect_uri: String,
    nonce: Option<String>,
    code_challenge: Option<String>,
}

/// What the server task and the test share.
struct Shared {
    issuer: String,
    routes: HashMap<String, FakeResponse>,
    requests: Vec<FakeRequest>,
    sign_in: FakeSignIn,
    /// The codes issued and not yet exchanged.
    codes: HashMap<String, Authorization>,
    /// The access tokens issued, by token: the subject's userinfo.
    access_tokens: HashMap<String, String>,
    issued: usize,
}

/// The fake identity provider, served until it is dropped.
pub struct FakeIdentityProvider {
    /// `http://127.0.0.1:{port}`.
    base: String,
    shared: Arc<Mutex<Shared>>,
    server: tokio::task::JoinHandle<()>,
}

/// The fake's signing key, under `kid`.
fn signing_key(kid: &str) -> CoreRsaPrivateSigningKey {
    CoreRsaPrivateSigningKey::from_pem(SIGNING_KEY_PEM, Some(JsonWebKeyId::new(kid.to_owned())))
        .expect("the fake's signing key reads")
}

impl FakeIdentityProvider {
    /// Starts the fake on a free port of `127.0.0.1`, serving its
    /// [`discovery_document`](Self::discovery_document), a key set of its one
    /// RSA key ([`KEY_ID`]), and its sign-in endpoints.
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
        let issuer = format!("{base}{ISSUER_PATH}");
        let shared = Arc::new(Mutex::new(Shared {
            sign_in: FakeSignIn {
                subject: SUBJECT.to_owned(),
                claims: Map::new(),
                signature: FakeSignature::Valid,
                iss: Some(issuer.clone()),
                error: None,
                userinfo: Map::new(),
            },
            issuer,
            routes: HashMap::new(),
            requests: Vec::new(),
            codes: HashMap::new(),
            access_tokens: HashMap::new(),
            issued: 0,
        }));
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
            FakeResponse::json(&Self::key_set(KEY_ID)),
        );
        fake
    }

    /// A key set holding the public half of the fake's signing key, under
    /// `kid`.
    ///
    /// # Panics
    ///
    /// Never: the key is a constant that reads.
    #[must_use]
    pub fn key_set(kid: &str) -> Value {
        let mut key =
            serde_json::to_value(signing_key(kid).as_verification_key()).expect("a key serializes");
        key["alg"] = json!("RS256");
        json!({ "keys": [key] })
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
    /// authorization code flow with PKCE `S256`, an RS256 key set, and its
    /// sign-in endpoints.
    #[must_use]
    pub fn discovery_document(&self) -> Value {
        let issuer = self.issuer();
        json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}{AUTHORIZATION_PATH}"),
            "token_endpoint": format!("{issuer}{TOKEN_PATH}"),
            "userinfo_endpoint": format!("{issuer}{USERINFO_PATH}"),
            "jwks_uri": format!("{issuer}{JWKS_PATH}"),
            "response_types_supported": ["code", "id_token", "code id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "code_challenge_methods_supported": ["plain", "S256"],
        })
    }

    /// Answers `path` (with no query) with `response` from now on, in place
    /// of whatever the fake answered there.
    pub fn set(&self, path: &str, response: FakeResponse) {
        self.lock().routes.insert(path.to_owned(), response);
    }

    /// Edits what the sign-in endpoints answer from now on.
    pub fn sign_in(&self, edit: impl FnOnce(&mut FakeSignIn)) {
        edit(&mut self.lock().sign_in);
    }

    /// Every request received so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<FakeRequest> {
        self.lock().requests.clone()
    }

    /// Follows the redirect to the authorization endpoint `location` as a
    /// browser would, and returns where the fake sends the browser back: the
    /// callback URL with its query.
    ///
    /// # Panics
    ///
    /// When the fake does not answer that with a redirect.
    pub async fn authorize(&self, location: &str) -> String {
        let answer = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client")
            .get(location)
            .send()
            .await
            .expect("the authorization endpoint answers");
        assert_eq!(answer.status(), 302, "the authorization endpoint redirects");
        answer.headers()["location"]
            .to_str()
            .expect("a location")
            .to_owned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for FakeIdentityProvider {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// `value` as a JWT segment: unpadded base64url of its JSON.
fn segment(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

/// A JWT of `claims`, signed as `signature` says.
fn sign(signature: &FakeSignature, claims: &Value) -> String {
    let (header, key) = match signature {
        FakeSignature::Unsigned => {
            return format!("{}.{}.", segment(&json!({"alg": "none"})), segment(claims));
        }
        FakeSignature::Hmac(secret) => {
            let header = json!({"alg": "HS256", "typ": "JWT"});
            let input = format!("{}.{}", segment(&header), segment(claims));
            let mac = CoreHmacKey::new(secret.as_bytes())
                .sign(&CoreJwsSigningAlgorithm::HmacSha256, input.as_bytes())
                .expect("HS256 signs");
            return format!("{input}.{}", URL_SAFE_NO_PAD.encode(mac));
        }
        FakeSignature::KeyId(kid) => (
            json!({"alg": "RS256", "typ": "JWT", "kid": kid}),
            signing_key(kid),
        ),
        FakeSignature::Valid | FakeSignature::Tampered => (
            json!({"alg": "RS256", "typ": "JWT", "kid": KEY_ID}),
            signing_key(KEY_ID),
        ),
    };
    let signed = |claims: &Value| {
        let input = format!("{}.{}", segment(&header), segment(claims));
        let signature = key
            .sign(
                &CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
                input.as_bytes(),
            )
            .expect("RS256 signs");
        (input, URL_SAFE_NO_PAD.encode(signature))
    };
    if *signature == FakeSignature::Tampered {
        let mut other = claims.clone();
        other["sub"] = json!("someone-else");
        let (_, other_signature) = signed(&other);
        format!("{}.{}.{other_signature}", segment(&header), segment(claims))
    } else {
        let (input, signature) = signed(claims);
        format!("{input}.{signature}")
    }
}

/// The query of `uri` as pairs.
fn query_pairs(query: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(query).into_owned().collect()
}

/// A `302` to `base` with `params` added to its query.
fn redirect_to(base: &str, params: &[(&str, &str)]) -> Response {
    let Ok(mut url) = url::Url::parse(base) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    url.query_pairs_mut().extend_pairs(params);
    FakeResponse::redirect(url.as_str()).into_response()
}

/// The authorization endpoint: straight back to the `redirect_uri` with a
/// code, or the [`FakeSignIn::error`].
fn authorize(shared: &mut Shared, query: &[u8]) -> Response {
    let query = query_pairs(query);
    let (Some(redirect_uri), Some(client_id)) = (query.get("redirect_uri"), query.get("client_id"))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let state = query.get("state").map_or("", String::as_str);
    let mut params = vec![("state", state.to_owned())];
    if let Some(error) = &shared.sign_in.error {
        params.push(("error", error.clone()));
    } else {
        shared.issued += 1;
        let code = format!("fake-code-{}", shared.issued);
        shared.codes.insert(
            code.clone(),
            Authorization {
                client_id: client_id.clone(),
                redirect_uri: redirect_uri.clone(),
                nonce: query.get("nonce").cloned(),
                code_challenge: query
                    .get("code_challenge")
                    .filter(|_| {
                        query.get("code_challenge_method").map(String::as_str) == Some("S256")
                    })
                    .cloned(),
            },
        );
        params.push(("code", code));
    }
    if let Some(iss) = &shared.sign_in.iss {
        params.push(("iss", iss.clone()));
    }
    let params: Vec<(&str, &str)> = params.iter().map(|(n, v)| (*n, v.as_str())).collect();
    redirect_to(redirect_uri, &params)
}

/// An OAuth error answer of the token endpoint.
fn token_error(error: &str) -> Response {
    FakeResponse {
        status: 400,
        ..FakeResponse::json(&json!({ "error": error }))
    }
    .into_response()
}

/// The token endpoint: a code, once, for the client and redirect URI it was
/// issued to and with the verifier of its challenge.
fn exchange(shared: &mut Shared, body: &[u8]) -> Response {
    let form = query_pairs(body);
    let Some(authorization) = form.get("code").and_then(|code| shared.codes.remove(code)) else {
        return token_error("invalid_grant");
    };
    let verified = match (&authorization.code_challenge, form.get("code_verifier")) {
        (Some(challenge), Some(verifier)) => {
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == *challenge
        }
        _ => false,
    };
    if !verified || form.get("redirect_uri") != Some(&authorization.redirect_uri) {
        return token_error("invalid_grant");
    }
    let now = chrono::Utc::now().timestamp();
    let sign_in = &shared.sign_in;
    let mut claims = json!({
        "iss": shared.issuer,
        "sub": sign_in.subject,
        "aud": authorization.client_id,
        "exp": now + 300,
        "iat": now,
        "preferred_username": USERNAME,
    });
    if let Some(nonce) = &authorization.nonce {
        claims["nonce"] = json!(nonce);
    }
    for (name, value) in &sign_in.claims {
        match value {
            Value::Null => {
                claims.as_object_mut().map(|claims| claims.remove(name));
            }
            value => claims[name] = value.clone(),
        }
    }
    let id_token = sign(&sign_in.signature, &claims);
    shared.issued += 1;
    let access_token = format!("fake-access-token-{}", shared.issued);
    shared
        .access_tokens
        .insert(access_token.clone(), sign_in.subject.clone());
    FakeResponse::json(&json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
    }))
    .into_response()
}

/// The userinfo endpoint: the user's claims, for an access token it issued.
fn userinfo(shared: &Shared, headers: &HeaderMap) -> Response {
    let subject = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| shared.access_tokens.get(token));
    let Some(subject) = subject else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let mut claims = Map::new();
    claims.insert("sub".to_owned(), json!(subject));
    claims.extend(shared.sign_in.userinfo.clone());
    FakeResponse::json(&Value::Object(claims)).into_response()
}

/// Records the request and answers it with what is set for its path, else
/// with the sign-in endpoint at that path, else `404`.
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
        if let Some(response) = shared.routes.get(uri.path()).cloned() {
            response
        } else {
            let endpoint = uri.path().strip_prefix(ISSUER_PATH).unwrap_or_default();
            let query = uri.query().unwrap_or_default().as_bytes();
            return match endpoint {
                AUTHORIZATION_PATH => authorize(&mut shared, query),
                TOKEN_PATH if method == Method::POST => exchange(&mut shared, &body),
                USERINFO_PATH => userinfo(&shared, &headers),
                _ => StatusCode::NOT_FOUND.into_response(),
            };
        }
    };
    tokio::time::sleep(response.delay).await;
    response.into_response()
}

impl IntoResponse for FakeResponse {
    fn into_response(self) -> Response {
        let mut builder = Response::builder().status(self.status);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let body = if self.chunked {
            let chunks: Vec<Result<Bytes, std::convert::Infallible>> = self
                .body
                .chunks(64 * 1024)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            Body::from_stream(futures_util::stream::iter(chunks))
        } else {
            Body::from(self.body)
        };
        builder
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}
