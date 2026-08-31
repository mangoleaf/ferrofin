//! Response-compression negotiation, end to end through the assembled router.
//!
//! Jellyfin 10.11.8 registers ASP.NET Core's response-compression middleware
//! (`Jellyfin.Server/Startup.cs`), so a client that offers `br`/`gzip` gets a
//! compressed JSON body. These tests pin the observable contract, each case
//! measured against a live Jellyfin 10.11.8 first:
//!
//! * no `Accept-Encoding` → identity, no `Content-Encoding`;
//! * `br` wins over `gzip` when both are offered;
//! * `deflate`, `zstd` and `identity` are **not** offered, so those clients get
//!   an uncompressed body;
//! * decoding the compressed body yields the uncompressed bytes exactly — the
//!   JSON contract is untouched, compression is transport only.
//!
//! The media-type allow-list (which responses are eligible at all) is pinned by
//! the unit tests in `ferrofin_api::compression`.

use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_api::create_router;
use ferrofin_api::state::AppState;
use ferrofin_api::test_support::{
    FakeAuthContext, FakeAuthService, FakeConfig, FakeDto, FakeLibrary, FakeMediaSources,
    FakeMusic, FakeProviders, FakeQuickConnect, FakeSearch, FakeSessions, FakeSimilarItems,
    FakeUserData, FakeUserViews, FakeUsers,
};
use ferrofin_model::system::{PublicSystemInfo, SystemInfo};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::net::RequestContext;
use ferrofin_traits::system::{ServerApplicationHost, SystemManager};
use tower::ServiceExt;

/// The public system-info route: unauthenticated, `application/json`, and — with
/// [`StubSystem`] behind it — comfortably above the minimum compressible size.
const JSON_ROUTE: &str = "/System/Info/Public";

/// A [`SystemManager`] whose public info is large enough to be worth
/// compressing, so the size predicate is not what the test is measuring.
///
/// Note what these tests deliberately do NOT cover: the encoder's *level*.
/// `compression_layer` pins ASP.NET's `Fastest`, and that is not observable
/// from outside — on synthetic fixtures every level lands within ~6% of the
/// same size, so any threshold tight enough to catch a regression is loose
/// enough to break on a codec bump. The level was established by comparing
/// byte-for-byte against a live Jellyfin 10.11.8 (see `compression.rs`), and
/// the parity/perf suite is what would catch a silent change.
///
/// "Large enough" means over [`MIN_COMPRESSIBLE_BYTES`] (one TCP segment), so
/// `server_name` is padded rather than left at a realistic length — these tests
/// are about encoding correctness, and the floor has its own unit tests.
struct StubSystem;

#[async_trait]
impl SystemManager for StubSystem {
    async fn get_system_info(&self, _request: &RequestContext) -> Result<SystemInfo, ServiceError> {
        unimplemented!("not routed by these tests")
    }
    async fn get_public_system_info(
        &self,
        _request: &RequestContext,
    ) -> Result<PublicSystemInfo, ServiceError> {
        Ok(PublicSystemInfo {
            // Padded past the one-MTU compression floor on purpose (see above).
            server_name: Some("Ferrofin compression fixture server ".repeat(64)),
            version: Some("10.11.8".to_owned()),
            product_name: Some("Jellyfin Server".to_owned()),
            id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            startup_wizard_completed: Some(true),
            ..PublicSystemInfo::default()
        })
    }
    async fn restart(&self) -> Result<(), ServiceError> {
        unimplemented!("not routed by these tests")
    }
    async fn shutdown(&self) -> Result<(), ServiceError> {
        unimplemented!("not routed by these tests")
    }
    async fn get_system_storage_info(
        &self,
    ) -> Result<ferrofin_model::system::SystemStorageInfo, ServiceError> {
        unimplemented!("not routed by these tests")
    }
}

/// A minimal [`ServerApplicationHost`] with neutral getters.
struct StubHost;

#[async_trait]
impl ServerApplicationHost for StubHost {
    fn core_startup_has_completed(&self) -> bool {
        true
    }
    fn http_port(&self) -> u16 {
        8096
    }
    fn https_port(&self) -> u16 {
        8920
    }
    fn listen_with_https(&self) -> bool {
        false
    }
    fn name(&self) -> String {
        // Deliberately different from the friendly name so a regression that
        // swaps the two (as `/System/Ping` once did) fails the test.
        "Jellyfin Server".to_owned()
    }
    fn friendly_name(&self) -> String {
        "ferrofin-test".to_owned()
    }
    async fn get_smart_api_url(&self, _request: &RequestContext) -> Result<String, ServiceError> {
        unimplemented!("not routed by these tests")
    }
    async fn get_local_api_url(
        &self,
        _hostname: &str,
        _scheme: Option<&str>,
        _port: Option<u16>,
    ) -> Result<String, ServiceError> {
        unimplemented!("not routed by these tests")
    }
    fn expand_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
    fn reverse_virtual_path(&self, path: &str) -> String {
        path.to_owned()
    }
}

/// Builds an [`AppState`] whose system manager is [`StubSystem`]; every other
/// manager is a `test_support` fake that panics if reached.
fn state() -> AppState {
    AppState::new(
        Arc::new(FakeLibrary),
        Arc::new(FakeUsers),
        Arc::new(FakeUserViews),
        Arc::new(FakeUserData),
        Arc::new(FakeMediaSources),
        Arc::new(FakeSessions),
        Arc::new(StubSystem),
        Arc::new(StubHost),
        Arc::new(FakeConfig),
        Arc::new(FakeProviders),
        Arc::new(FakeMusic),
        Arc::new(FakeSimilarItems),
        Arc::new(FakeSearch),
        Arc::new(FakeDto),
        Arc::new(FakeAuthContext),
        Arc::new(FakeAuthService),
        Arc::new(FakeQuickConnect),
        Arc::new(ferrofin_api::test_support::FakePlaylists),
        Arc::new(ferrofin_api::test_support::FakeCollections),
        Arc::new(ferrofin_api::test_support::FakeTvSeries),
        Arc::new(ferrofin_api::test_support::FakeSubtitles),
        Arc::new(ferrofin_api::test_support::FakeLyrics),
        Arc::new(ferrofin_api::test_support::FakeMediaSegments),
        Arc::new(ferrofin_api::test_support::FakeTrickplay),
        Arc::new(ferrofin_api::test_support::FakeDevices),
        Arc::new(ferrofin_api::test_support::FakeClientEventLogger),
        Arc::new(ferrofin_api::test_support::FakeApiKeys),
        Arc::new(ferrofin_api::test_support::FakeLocalization),
        Arc::new(ferrofin_api::test_support::FakeDisplayPreferences),
        Arc::new(ferrofin_api::test_support::FakeActivity),
        Arc::new(ferrofin_api::test_support::FakeFileSystem),
        Arc::new(ferrofin_api::test_support::FakeTasks),
    )
}

/// Drives one GET through the real router with an optional `Accept-Encoding`.
async fn get(uri: &str, accept_encoding: Option<&str>) -> (StatusCode, Option<String>, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(ae) = accept_encoding {
        builder = builder.header(header::ACCEPT_ENCODING, ae);
    }
    let response = create_router(state())
        .oneshot(builder.body(Body::empty()).expect("request builds"))
        .await
        .expect("router responds");
    let status = response.status();
    let encoding = response
        .headers()
        .get(header::CONTENT_ENCODING)
        .map(|v| v.to_str().expect("header is ascii").to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    (status, encoding, bytes.to_vec())
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("valid gzip stream");
    out
}

fn unbrotli(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(bytes, 4096)
        .read_to_end(&mut out)
        .expect("valid brotli stream");
    out
}

#[tokio::test]
async fn no_accept_encoding_is_served_uncompressed() {
    let (status, encoding, body) = get(JSON_ROUTE, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding, None, "identity request must not be encoded");
    serde_json::from_slice::<serde_json::Value>(&body).expect("plain JSON body");
}

#[tokio::test]
async fn gzip_body_decodes_to_the_identity_bytes() {
    let (_, _, plain) = get(JSON_ROUTE, None).await;
    let (status, encoding, body) = get(JSON_ROUTE, Some("gzip")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding.as_deref(), Some("gzip"));
    assert_ne!(body, plain, "the body must actually be encoded");
    assert_eq!(
        gunzip(&body),
        plain,
        "decoded bytes must equal the uncompressed response byte for byte"
    );
}

#[tokio::test]
async fn brotli_body_decodes_to_the_identity_bytes() {
    let (_, _, plain) = get(JSON_ROUTE, None).await;
    let (status, encoding, body) = get(JSON_ROUTE, Some("br")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(encoding.as_deref(), Some("br"));
    assert_ne!(body, plain, "the body must actually be encoded");
    assert_eq!(unbrotli(&body), plain);
}

#[tokio::test]
async fn brotli_is_preferred_when_the_client_offers_both() {
    // Jellyfin 10.11.8 answers `Accept-Encoding: deflate, gzip, br, zstd` with
    // `Content-Encoding: br`.
    let (_, encoding, _) = get(JSON_ROUTE, Some("deflate, gzip, br, zstd")).await;
    assert_eq!(encoding.as_deref(), Some("br"));
}

#[tokio::test]
async fn encodings_jellyfin_does_not_offer_are_served_uncompressed() {
    for offered in ["deflate", "zstd", "identity"] {
        let (status, encoding, body) = get(JSON_ROUTE, Some(offered)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            encoding, None,
            "Jellyfin has no {offered} provider, so neither do we"
        );
        serde_json::from_slice::<serde_json::Value>(&body)
            .unwrap_or_else(|e| panic!("{offered} body must be plain JSON: {e}"));
    }
}
