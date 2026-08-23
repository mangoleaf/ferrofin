//! In-process restart: `POST /System/Restart` drains the server and re-creates
//! the host inside the same process (Jellyfin's `Program.Main`
//! `do { … } while (_restartOnShutdown)`), so a container survives a restart
//! without a supervisor; `POST /System/Shutdown` makes `run` return.

use std::time::Duration;

use ferrofin_server::config::Config;

const ADMIN_USER: &str = "restart-admin";
const ADMIN_PASSWORD: &str = "restart-pw";
const CLIENT: &str =
    r#"MediaBrowser Client="restart-test", Device="test", DeviceId="restart-test", Version="1""#;

/// A port nobody is listening on right now (bind 0, read, release).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn is_up(client: &reqwest::Client, base: &str) -> bool {
    client
        .get(format!("{base}/System/Info/Public"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

async fn wait_until(client: &reqwest::Client, base: &str, up: bool) {
    for _ in 0..600 {
        if is_up(client, base).await == up {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "server never became {}",
        if up { "reachable" } else { "unreachable" }
    );
}

async fn token(client: &reqwest::Client, base: &str) -> String {
    let body: serde_json::Value = client
        .post(format!("{base}/Users/AuthenticateByName"))
        .header("Authorization", CLIENT)
        .json(&serde_json::json!({"Username": ADMIN_USER, "Pw": ADMIN_PASSWORD}))
        .send()
        .await
        .expect("auth request")
        .json()
        .await
        .expect("auth json");
    body["AccessToken"]
        .as_str()
        .expect("access token")
        .to_owned()
}

async fn post(client: &reqwest::Client, base: &str, path: &str, token: &str) -> u16 {
    client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("{CLIENT}, Token=\"{token}\""))
        .send()
        .await
        .expect("request")
        .status()
        .as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recreates_the_host_in_process_and_shutdown_exits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let port = free_port();
    let config = Config {
        server_name: "ferrofin-restart".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        // The Prometheus pipeline is process-global: it must survive the restart.
        enable_metrics: Some(true),
        ..Config::test_stub(tmp.path())
    };
    // `run`'s future trips rustc's higher-ranked `Send` inference under
    // `tokio::spawn`; the binary drives it from `main`, so give it its own thread
    // and runtime here too.
    let server = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("server runtime")
            .block_on(ferrofin_server::run(config))
    });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    wait_until(&client, &base, true).await;
    let metrics = |client: &reqwest::Client| {
        let url = format!("{base}/metrics");
        let client = client.clone();
        async move {
            client
                .get(url)
                .send()
                .await
                .expect("metrics request")
                .status()
                .as_u16()
        }
    };
    assert_eq!(metrics(&client).await, 200);

    // Restart: the listener goes away and comes back while `run` keeps running.
    let tok = token(&client, &base).await;
    assert_eq!(post(&client, &base, "/System/Restart", &tok).await, 204);
    wait_until(&client, &base, false).await;
    wait_until(&client, &base, true).await;
    assert!(!server.is_finished(), "a restart must not exit the process");
    assert_eq!(metrics(&client).await, 200, "/metrics survives the restart");

    // Shutdown: `run` returns.
    let tok = token(&client, &base).await;
    assert_eq!(post(&client, &base, "/System/Shutdown", &tok).await, 204);
    let outcome = tokio::time::timeout(
        Duration::from_mins(1),
        tokio::task::spawn_blocking(move || server.join()),
    )
    .await
    .expect("run returns after shutdown")
    .expect("join task")
    .expect("server thread did not panic");
    outcome.expect("run exits cleanly");
    assert!(
        !is_up(&client, &base).await,
        "shutdown leaves nothing listening"
    );
}

async fn get_json(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
) -> serde_json::Value {
    client
        .get(format!("{base}{path}"))
        .header("Authorization", format!("{CLIENT}, Token=\"{token}\""))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json")
}

async fn post_json(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let resp = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("{CLIENT}, Token=\"{token}\""))
        .json(body)
        .send()
        .await
        .expect("request");
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// `POST /Backup/Restore` schedules the archive and restarts; the restarted host
/// boots from the restored tree (Jellyfin's `ScheduleRestoreAndRestartServer` +
/// `RestoreBackupPath` sequence).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_restore_applies_on_the_in_process_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let port = free_port();
    let config = Config {
        server_name: "ferrofin-restore".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        ..Config::test_stub(tmp.path())
    };
    let server = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("server runtime")
            .block_on(ferrofin_server::run(config))
    });
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    wait_until(&client, &base, true).await;
    let tok = token(&client, &base).await;

    // A distinctive config value, backed up, then overwritten.
    let branding = |text: &str| serde_json::json!({"LoginDisclaimer": text, "CustomCss": "", "SplashscreenEnabled": false});
    let (st, _) = post_json(
        &client,
        &base,
        "/System/Configuration/Branding",
        &tok,
        &branding("before"),
    )
    .await;
    assert_eq!(st, 204);
    let (st, manifest) = post_json(
        &client,
        &base,
        "/Backup/Create",
        &tok,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(st, 200, "{manifest}");
    let archive = manifest["Path"].as_str().expect("archive path").to_owned();
    // A DATABASE change after the backup: a user that must be gone after restore.
    let (st, created) = post_json(
        &client,
        &base,
        "/Users/New",
        &tok,
        &serde_json::json!({"Name": "post-backup", "Password": "pw"}),
    )
    .await;
    assert_eq!(st, 200, "{created}");
    let post_backup_user = created["Id"].as_str().expect("user id").to_owned();
    let (st, _) = post_json(
        &client,
        &base,
        "/System/Configuration/Branding",
        &tok,
        &branding("after"),
    )
    .await;
    assert_eq!(st, 204);
    assert_eq!(
        get_json(&client, &base, "/Branding/Configuration", &tok).await["LoginDisclaimer"],
        "after"
    );

    // Restore: 204, the server restarts, and the backed-up value is live again.
    let (st, _) = post_json(
        &client,
        &base,
        "/Backup/Restore",
        &tok,
        &serde_json::json!({"ArchiveFileName": archive}),
    )
    .await;
    assert_eq!(st, 204);
    wait_until(&client, &base, false).await;
    wait_until(&client, &base, true).await;
    assert!(!server.is_finished(), "restore restarts in-process");
    let tok = token(&client, &base).await;
    assert_eq!(
        get_json(&client, &base, "/Branding/Configuration", &tok).await["LoginDisclaimer"],
        "before"
    );
    let users = get_json(&client, &base, "/Users", &tok).await;
    assert!(
        !users
            .as_array()
            .expect("users")
            .iter()
            .any(|u| u["Id"] == post_backup_user),
        "the restored database predates the user: {users}"
    );

    assert_eq!(post(&client, &base, "/System/Shutdown", &tok).await, 204);
    tokio::task::spawn_blocking(move || server.join())
        .await
        .expect("join task")
        .expect("server thread did not panic")
        .expect("run exits cleanly");
}
