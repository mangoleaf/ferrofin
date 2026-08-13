//! Server-level integration test for Tier-1b WASM plugins: the full vertical,
//! over real HTTP semantics (`tower` oneshot against the real router).
//!
//! A `ferrofin:plugin@0.1.0` component (the shared inline-WAT fixture from
//! `ferrofin-wasm` — no committed `.wasm`, no wasm toolchain needed) is
//! dropped into `{data_dir}/plugins/` before boot. The test then proves the
//! plan's E1 claim — "to the API a WASM plugin is indistinguishable from a
//! compiled-in one":
//!
//! 1. `GET  /Plugins` — the WASM plugin is listed with its guest-reported
//!    identity.
//! 2. `GET  /Plugins/{id}/Configuration` — the guest's default config JSON.
//! 3. `GET  /ScheduledTasks` — the guest's tasks appear in the registry.
//! 4. `POST /ScheduledTasks/Running/{taskId}` — the always-ok guest task
//!    runs to a `Completed` result through the real task manager.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "wasm-plugins-pw";

/// The fixture plugin's guest-reported id (from `TEST_FIXTURE_WAT`).
const PLUGIN_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff";

const CLIENT_AUTH: &str = "MediaBrowser Client=\"wasm-test\", Device=\"test\", \
                           DeviceId=\"wasm-test-1\", Version=\"1.0\"";

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json body")
}

#[tokio::test]
// One linear client flow (boot → auth → list → config → run → poll); splitting
// it would only scatter the sequence the test exists to prove.
#[allow(clippy::too_many_lines)]
async fn wasm_plugin_surfaces_on_plugins_api_and_its_task_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();
    std::fs::create_dir_all(root.join("config")).unwrap();

    // Install the fixture component the way a user would: a .wasm file in
    // {data_dir}/plugins, present before boot.
    let plugins_dir = root.join("data").join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    let component = wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).expect("fixture compiles");
    std::fs::write(plugins_dir.join("hello.wasm"), component).unwrap();

    let config = Config {
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
        server_name: "ferrofin-wasm-test".to_owned(),
        log_level: "info".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        db_pool: None,
        enable_metrics: None,
        metrics_sample_interval: None,
        scan_progress_every: None,
        wasm_call_timeout_secs: None,
        wasm_memory_limit_mb: None,
        wasm_event_queue_capacity: None,
        wasm_private_http_allow: None,
    };

    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        filters: Vec::new(),
        encoders: Vec::new(),
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    let router = ferrofin_api::create_router(wired.state.clone());

    // Authenticate the seeded admin.
    let auth = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, CLIENT_AUTH)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(auth.status(), StatusCode::OK);
    let token = body_json(auth).await["AccessToken"]
        .as_str()
        .expect("access token")
        .to_owned();
    let bearer = format!("{CLIENT_AUTH}, Token=\"{token}\"");
    let get = |uri: &str| {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, bearer.clone())
            .body(Body::empty())
            .unwrap()
    };

    // 1. The WASM plugin is listed on /Plugins with its guest identity.
    let plugins = router.clone().oneshot(get("/Plugins")).await.unwrap();
    assert_eq!(plugins.status(), StatusCode::OK);
    let plugins = body_json(plugins).await;
    let entry = plugins
        .as_array()
        .expect("plugin list")
        .iter()
        .find(|p| {
            p["Id"].as_str().unwrap_or_default() == PLUGIN_ID.replace('-', "")
                || p["Id"].as_str().unwrap_or_default() == PLUGIN_ID
        })
        .unwrap_or_else(|| panic!("wasm plugin missing from /Plugins: {plugins}"))
        .clone();
    assert_eq!(entry["Name"], "Hello");
    assert_eq!(entry["Version"], "1.2.3");

    // 2. Its configuration endpoint serves the guest's default config.
    let config_uri = format!("/Plugins/{PLUGIN_ID}/Configuration");
    let cfg = router.clone().oneshot(get(&config_uri)).await.unwrap();
    assert_eq!(cfg.status(), StatusCode::OK, "plugin config is fetchable");
    assert_eq!(body_json(cfg).await["a"], 1, "guest default config served");

    // 3. The guest's tasks are in the scheduled-task registry.
    let tasks = router
        .clone()
        .oneshot(get("/ScheduledTasks"))
        .await
        .unwrap();
    assert_eq!(tasks.status(), StatusCode::OK);
    let tasks = body_json(tasks).await;
    let ok_task = tasks
        .as_array()
        .expect("task list")
        .iter()
        .find(|t| {
            t["Key"]
                .as_str()
                .is_some_and(|k| k.starts_with("wasm-") && k.ends_with("-ok"))
        })
        .unwrap_or_else(|| panic!("wasm 'ok' task missing from /ScheduledTasks"))
        .clone();
    assert_eq!(ok_task["Name"], "Okay");
    assert_eq!(ok_task["Category"], "Test");
    let task_id = ok_task["Id"].as_str().expect("task id").to_owned();

    // 4. Run it through the real task manager and poll to a Completed result.
    let run = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/ScheduledTasks/Running/{task_id}"))
                .header(header::AUTHORIZATION, bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(run.status(), StatusCode::NO_CONTENT, "task run accepted");

    let mut completed = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let info = router
            .clone()
            .oneshot(get(&format!("/ScheduledTasks/{task_id}")))
            .await
            .unwrap();
        assert_eq!(info.status(), StatusCode::OK);
        let info = body_json(info).await;
        if let Some(status) = info["LastExecutionResult"]["Status"].as_str() {
            completed = Some((status.to_owned(), info));
            break;
        }
    }
    let (status, info) = completed.expect("task produced a result within 5s");
    assert_eq!(
        status, "Completed",
        "the wasm task must complete successfully: {info}"
    );

    // Keep the temp dir (DB + plugin file) alive to the end.
    drop(Arc::new(temp));
}
