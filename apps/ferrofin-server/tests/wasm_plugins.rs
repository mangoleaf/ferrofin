//! Server-level integration test for Tier-1b WASM plugins: the full vertical,
//! over real HTTP semantics (`tower` oneshot against the real router).
//!
//! A `ferrofin:plugin@0.2.0` component (the shared inline-WAT fixture from
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
        server_name: "ferrofin-wasm-test".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(root)
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

    // 2b. The guest's authored settings page surfaces on the dashboard
    //     discovery endpoint tagged with the plugin id (this is exactly how
    //     jellyfin-web decides to show a Settings button), and its bytes are
    //     served under both spellings jellyfin-web uses.
    let pages = router
        .clone()
        .oneshot(get("/web/ConfigurationPages"))
        .await
        .unwrap();
    assert_eq!(pages.status(), StatusCode::OK);
    let pages = body_json(pages).await;
    let page = pages
        .as_array()
        .expect("page list")
        .iter()
        .find(|p| p["Name"] == "fixture-page")
        .unwrap_or_else(|| panic!("fixture page missing from /web/ConfigurationPages: {pages}"))
        .clone();
    assert_eq!(
        page["PluginId"].as_str().map(|s| s.replace('-', "")),
        Some(PLUGIN_ID.replace('-', "")),
        "page is tagged with the plugin id jellyfin-web matches on"
    );
    assert_eq!(page["DisplayName"], "Hello", "labeled with the plugin name");
    let body = router
        .clone()
        .oneshot(get("/web/configurationpage?name=fixture-page"))
        .await
        .unwrap();
    assert_eq!(body.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(body.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "<div data-role=\"page\">fixture</div>",
        "authored page bytes served verbatim"
    );

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

/// A tiny multi-request loopback HTTP server for the repository flow:
/// `/manifest.json` and `/plugin.wasm` until the sender drops.
fn repo_server(
    manifest_for: impl FnOnce(&str) -> String,
    artifact: Vec<u8>,
) -> (String, std::sync::mpsc::Sender<()>) {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let manifest = manifest_for(&format!("http://{addr}"));
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let body: &[u8] = if request.contains("/plugin.wasm") {
                        &artifact
                    } else {
                        manifest.as_bytes()
                    };
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = stream.write_all(body);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (format!("http://{addr}"), stop_tx)
}

/// The full Jellyfin repository-install flow over the real router, with the
/// REAL artifact validator: add a repository → the catalog lists its package
/// → install → the .wasm is staged in {data_dir}/plugins and SystemInfo
/// reports HasPendingRestart (the plugin activates on the next boot).
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn repository_install_stages_plugin_and_flags_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path();
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("data")).unwrap();

    // The "repository": the shared WAT fixture as the released artifact.
    let artifact = wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).expect("fixture compiles");
    let md5 = ferrofin_common::extensions::md5_hex(&artifact);
    let (repo_base, _stop) = repo_server(
        |base| {
            format!(
                r#"[{{"name":"HelloFixture","description":"d","overview":"o","owner":"tester",
                    "category":"General","guid":"{PLUGIN_ID}",
                    "versions":[{{"version":"1.2.3","targetAbi":"{abi}",
                      "sourceUrl":"{base}/plugin.wasm","checksum":"{md5}",
                      "repositoryName":"loop","repositoryUrl":"{base}/manifest.json"}}]}}]"#,
                abi = ferrofin_wasm::PLUGIN_ABI,
            )
        },
        artifact.clone(),
    );

    let config = Config {
        server_name: "ferrofin-repo-install-test".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(root)
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

    // Authenticate.
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
        .unwrap()
        .to_owned();
    let bearer = format!("{CLIENT_AUTH}, Token=\"{token}\"");

    // 1. Register the repository (what the admin does in the dashboard).
    let set_repos = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Repositories")
                .header(header::AUTHORIZATION, bearer.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!([{
                        "Name": "loop", "Url": format!("{repo_base}/manifest.json"), "Enabled": true
                    }])
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(set_repos.status(), StatusCode::NO_CONTENT);

    // 2. The catalog lists the repository's package.
    let packages = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Packages")
                .header(header::AUTHORIZATION, bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(packages.status(), StatusCode::OK);
    let packages = body_json(packages).await;
    assert!(
        packages
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "HelloFixture"),
        "catalog lists the repo package: {packages}"
    );

    // 3. Install (jellyfin-web's call, guid included).
    let install = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/Packages/Installed/HelloFixture?assemblyGuid={PLUGIN_ID}&version=1.2.3"
                ))
                .header(header::AUTHORIZATION, bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(install.status(), StatusCode::NO_CONTENT, "install succeeds");

    // 4. The artifact is staged where the WASM host loads from…
    let staged = root
        .join("data")
        .join("plugins")
        .join(format!("{PLUGIN_ID}.wasm"));
    assert_eq!(std::fs::read(&staged).unwrap(), artifact, "artifact staged");

    // …and the server reports a pending restart (Jellyfin's activation model).
    let info = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .header(header::AUTHORIZATION, bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(info.status(), StatusCode::OK);
    let info = body_json(info).await;
    assert_eq!(
        info["HasPendingRestart"], true,
        "restart-required surfaces in SystemInfo: {info}"
    );

    drop(Arc::new(temp));
}
