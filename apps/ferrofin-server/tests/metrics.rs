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
        data_dir: root.join("data"),
        config_dir: root.join("config"),
        cache_dir: root.join("cache"),
        web_dir: root.join("web"),
        bind_addr: "127.0.0.1".parse().unwrap(),
        port: 0,
        https_port: 0,
        published_url: None,
        base_url: String::new(),
        omdb_api_key: String::new(),
        studios_repo_url: String::new(),
        tvdb_api_key: String::new(),
        tvdb_subscriber_pin: String::new(),
        fanart_personal_api_key: String::new(),
        musicbrainz_base_url: String::new(),
        ffmpeg_path: None,
        ffprobe_path: None,
        library_roots: Vec::new(),
        server_name: "ferrofin-metrics-test".to_owned(),
        log_level: "info".to_owned(),
        admin_user: "admin".to_owned(),
        admin_password: String::new(),
        db_pool: None,
        enable_metrics: None,
        metrics_sample_interval: None,
        scan_progress_every: None,
        wasm_call_timeout_secs: None,
        wasm_memory_limit_mb: None,
        wasm_event_queue_capacity: None,
    }
}

/// Boots the real composition root over a fresh in-memory database.
async fn boot() -> (WiredApp, Database, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    let config = test_config(tmp.path());
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        filters: Vec::new(),
        encoders: Vec::new(),
    };
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, tx).await.unwrap();
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
    let (wired, db, _tmp) = boot().await;

    // Mirror what the composition root does when EnableMetrics is set.
    let handle =
        ferrofin_metrics::init(RouteLabels::default(), tokio::runtime::Handle::current()).unwrap();
    let router = metrics_wiring::mount(ferrofin_api::create_router(wired.state.clone()), &handle);
    metrics_wiring::spawn_sampler(&handle, Arc::clone(&wired.state.sessions), db.clone(), 0);

    // Let the sampler's immediate first tick populate the mirror gauges.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    // First scrape warms the HTTP counter (its own request is recorded post-body).
    let (warm, _) = get(router.clone(), "/metrics").await;
    assert_eq!(warm, StatusCode::OK);

    let (status, body) = get(router, "/metrics").await;
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
}
