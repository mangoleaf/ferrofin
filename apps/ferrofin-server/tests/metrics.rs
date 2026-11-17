//! Integration coverage for the optional `/metrics` endpoint wiring.
//!
//! `apps/ferrofin-server` is coverage-exempt, but this must pass: it exercises the
//! real mount + background sampler + exposition render over the actual API
//! router, built from the real composition root. The gate itself
//! (`if config.enable_metrics`) is trivial glue — the default gate value is
//! asserted here; the config round-trip is covered by the configuration
//! manager's own tests.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ferrofin_db::Database;
use ferrofin_metrics::RouteLabels;
use ferrofin_server::config::Config;
use ferrofin_server::metrics_wiring;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

/// A bootstrap [`Config`] with every path under `root`.
fn test_config(root: &std::path::Path) -> Config {
    Config {
        server_name: "ferrofin-metrics-test".to_owned(),
        ..Config::test_stub(root)
    }
}

/// Boots the real composition root over a fresh in-memory database.
async fn boot() -> (WiredApp, Database, tempfile::TempDir) {
    boot_with_network(None).await
}

/// [`boot`] with a saved `network.json` (the dashboard's network settings)
/// in place first, when one is given.
async fn boot_with_network(network: Option<&str>) -> (WiredApp, Database, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    if let Some(network) = network {
        let named = tmp.path().join("config/users/named");
        std::fs::create_dir_all(&named).unwrap();
        std::fs::write(named.join("network.json"), network).unwrap();
    }
    let config = test_config(tmp.path());
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, tx)
        .await
        .unwrap();
    (wired, db, tmp)
}

async fn get(router: axum::Router, uri: &str) -> (StatusCode, String) {
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn metrics_disabled_by_default_and_route_absent() {
    let (wired, _db, _tmp) = boot().await;
    // The gate value the composition root reads defaults to false.
    assert!(
        !wired
            .state
            .config
            .configuration()
            .await
            .unwrap()
            .enable_metrics
    );

    // Without mounting, `/metrics` is not a registered route → 404.
    let (status, _) = get(ferrofin_api::create_router(wired.state), "/metrics").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metrics_enabled_serves_exposition() {
    // A blocklist naming one remote subnet; the in-process requests below
    // carry no peer address and read as loopback, which the gate exempts.
    let (wired, db, _tmp) = boot_with_network(Some(
        r#"{"RemoteIPFilter":["198.51.100.0/24"],"IsRemoteIPFilterBlacklist":true}"#,
    ))
    .await;

    // Mirror what the composition root does when EnableMetrics is set.
    let handle =
        ferrofin_metrics::init(RouteLabels::default(), tokio::runtime::Handle::current()).unwrap();
    let router = metrics_wiring::mount(
        ferrofin_api::create_router(wired.state.clone()),
        &handle,
        &wired.state,
    );
    let _sampler = metrics_wiring::spawn_sampler(
        metrics_wiring::register_gauges(&handle),
        Arc::clone(&wired.state.sessions),
        db.clone(),
        0,
    );

    // Let the sampler's immediate first tick populate the mirror gauges.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    // First scrape warms the HTTP counter (its own request is recorded post-body).
    let (warm, _) = get(router.clone(), "/metrics").await;
    assert_eq!(warm, StatusCode::OK);

    let (status, body) = get(router.clone(), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    for name in [
        "http_requests_received_total", // HTTP family (from the warm scrape)
        "process_open_handles",         // process family
        "ferrofin_db_pool_connections", // sampler-fed gauge
    ] {
        assert!(
            body.contains(name),
            "missing `{name}` in exposition:\n{body}"
        );
    }
    // gl-903: Startup's response-time and CORS middleware run above
    // `MapMetrics`, and mounting `/metrics` keeps the API router's layered
    // fallback, so both answers carry the headers.
    for (uri, status) in [
        ("/metrics", StatusCode::OK),
        ("/no/such/path", StatusCode::NOT_FOUND),
    ] {
        let request = Request::builder()
            .uri(uri)
            .header("origin", "http://client.example")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status, "{uri}");
        for name in ["x-response-time-ms", "access-control-allow-origin"] {
            assert_eq!(
                response.headers().get_all(name).iter().count(),
                1,
                "{uri}: {name}"
            );
        }
    }

    // An unmatched path still gets the API router's own (layered) fallback.
    let marked = axum::Router::new().layer(axum::middleware::from_fn(
        |request: Request<Body>, next: axum::middleware::Next| async move {
            let mut response = next.run(request).await;
            response
                .headers_mut()
                .insert("x-api-router", axum::http::HeaderValue::from_static("1"));
            response
        },
    ));
    let response = metrics_wiring::mount(marked, &handle, &wired.state)
        .oneshot(
            Request::builder()
                .uri("/no/such/path")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers().contains_key("x-api-router"));

    // `/metrics` is behind the same IP-access gate as the API (upstream maps
    // it after `UseIPBasedAccessValidation`): a peer the network policy
    // refuses gets the gate's 503, an admitted one the exposition.
    for (peer, expected) in [
        ("198.51.100.4:5000", StatusCode::SERVICE_UNAVAILABLE),
        ("203.0.113.9:5000", StatusCode::OK),
    ] {
        let mut request = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        let status = router.clone().oneshot(request).await.unwrap().status();
        assert_eq!(status, expected, "{peer}");
    }

    // The gate covers the `/metrics` route alone. Turning metrics on must not
    // change what a refused peer gets for a path no route matches.
    let mut request = Request::builder()
        .uri("/no/such/path")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "198.51.100.4:5000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let status = router.oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::NOT_FOUND);
}
