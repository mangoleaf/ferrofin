//! In-process restart: `POST /System/Restart` drains the server and re-creates
//! the host inside the same process (Jellyfin's `Program.Main`
//! `do { … } while (_restartOnShutdown)`), so a container survives a restart
//! without a supervisor; `POST /System/Shutdown` makes `run` return.

use std::time::Duration;

use ferrofin_server::config::Config;

#[path = "support/boot.rs"]
mod boot;

use boot::{STARTUP_DEADLINE, free_port, is_up, spawn_server};

const ADMIN_USER: &str = "restart-admin";
const ADMIN_PASSWORD: &str = "restart-pw";
const CLIENT: &str =
    r#"MediaBrowser Client="restart-test", Device="test", DeviceId="restart-test", Version="1""#;

/// Ceiling for a single request, here only to turn a genuine hang into a
/// failure rather than a stalled job — it is not a latency assertion.
///
/// It has to be generous: `/Users/AuthenticateByName` runs Jellyfin's
/// 210 000-iteration PBKDF2-HMAC-SHA512 (`ferrofin_common::cryptography`), and
/// an unoptimized test build pays that in seconds. Seeding the same password at
/// startup was measured at ~13s on CI with the suite running in parallel, and a
/// verify costs the same, so a low ceiling here fails the run on a busy machine
/// instead of catching a bug. Do not tighten this to "something reasonable".
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// How long a set-up server gets to refuse a boot without its database. The
/// refusal comes before the database, ffmpeg or the listener are touched, so
/// it is quick; the bound only turns a regression that serves instead into a
/// failure rather than a test that never ends.
const REFUSAL_DEADLINE: Duration = Duration::from_mins(1);

/// Serializes the tests that create a backup.
///
/// `POST /Backup/Create` holds a process-global single permit
/// (`BACKUP_IN_FLIGHT`, which bounds concurrent backups' memory), and a
/// caller that loses the race gets `503`. `nextest` gives each test its own
/// process and hides that; plain `cargo test` runs these servers as threads
/// in one process, where the second backup is refused.
static BACKUP_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn wait_until(client: &reqwest::Client, base: &str, name: &str, up: bool) {
    let deadline = std::time::Instant::now() + STARTUP_DEADLINE;
    while std::time::Instant::now() < deadline {
        if is_up(client, base, name).await == up {
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
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("client");
    let discovery_addr = std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("reserve discovery endpoint")
        .local_addr()
        .expect("discovery address");
    let (server, base, config) = spawn_server(&client, |port| Config {
        server_name: "ferrofin-restart".to_owned(),
        discovery_bind_addr: Some(discovery_addr),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        // The Prometheus pipeline is process-global: it must survive the restart.
        enable_metrics: Some(true),
        ..Config::test_stub(tmp.path())
    })
    .await;
    let name = config.server_name;
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
    let discovered = discover(discovery_addr).await;
    assert_eq!(discovered["Address"], base);
    assert_eq!(discovered["Name"], name);
    let public: serde_json::Value = client
        .get(format!("{base}/System/Info/Public"))
        .send()
        .await
        .expect("public info")
        .json()
        .await
        .expect("public JSON");
    assert_eq!(discovered["Id"], public["Id"]);
    assert!(discovered["EndpointAddress"].is_null());

    // Restart: the listener goes away and comes back while `run` keeps running.
    let tok = token(&client, &base).await;
    let configuration = prepare_dashboard_restart(&client, &base, &tok).await;
    assert_eq!(post(&client, &base, "/System/Restart", &tok).await, 204);
    wait_until(&client, &base, &name, false).await;
    wait_until(&client, &base, &name, true).await;
    assert!(!server.is_finished(), "a restart must not exit the process");
    assert_dashboard_restarted(&client, &base, &tok, &configuration).await;
    assert_eq!(metrics(&client).await, 200, "/metrics survives the restart");
    assert_eq!(
        discover(discovery_addr).await,
        discovered,
        "discovery survives restart with stable identity"
    );

    // The toggle is persisted immediately, but the current socket remains active
    // until the next lifetime, exactly like Jellyfin's hosted service.
    let status = client
        .post(format!("{base}/System/Configuration/network"))
        .header("Authorization", format!("{CLIENT}, Token=\"{tok}\""))
        .json(&serde_json::json!({"AutoDiscovery": false}))
        .send()
        .await
        .expect("disable discovery")
        .status();
    assert!(status.is_success());
    assert_eq!(discover(discovery_addr).await, discovered);
    assert_eq!(post(&client, &base, "/System/Restart", &tok).await, 204);
    wait_until(&client, &base, &name, false).await;
    wait_until(&client, &base, &name, true).await;
    let released = std::net::UdpSocket::bind(discovery_addr)
        .expect("disabled lifetime does not bind discovery");
    drop(released);

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
        !is_up(&client, &base, &name).await,
        "shutdown leaves nothing listening"
    );
}

/// Saved dashboard settings survive a new lifetime, while the restart flag
/// clears. The old session token must still authorize these checks.
async fn prepare_dashboard_restart(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> serde_json::Value {
    let mut config = get_json(client, base, "/System/Configuration", token).await;
    config["UICulture"] = serde_json::json!("de");
    config["QuickConnectAvailable"] = serde_json::json!(false);
    config["ParallelImageEncodingLimit"] = serde_json::json!(2);
    assert_eq!(
        post_json(client, base, "/System/Configuration", token, &config)
            .await
            .0,
        204
    );
    let info = get_json(client, base, "/System/Info", token).await;
    assert_eq!(info["HasPendingRestart"], true);
    config
}

async fn assert_dashboard_restarted(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    expected: &serde_json::Value,
) {
    let actual = get_json(client, base, "/System/Configuration", token).await;
    assert_eq!(
        &actual, expected,
        "saved settings survive the new host lifetime"
    );
    assert_eq!(
        get_json(client, base, "/System/Info", token).await["HasPendingRestart"],
        false
    );
    assert_eq!(
        get_json(client, base, "/QuickConnect/Enabled", token).await,
        false
    );
}

async fn discover(address: std::net::SocketAddr) -> serde_json::Value {
    let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("UDP client");
    client
        .send_to(b"who is JellyfinServer?", address)
        .await
        .expect("discovery request");
    let mut buffer = [0; 4096];
    let (length, source) =
        tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buffer))
            .await
            .expect("discovery deadline")
            .expect("discovery response");
    assert_eq!(source, address);
    serde_json::from_slice(&buffer[..length]).expect("discovery JSON")
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
    let _serial = BACKUP_SERIAL.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("client");
    let (server, base, config) = spawn_server(&client, |port| Config {
        server_name: "ferrofin-restore".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        ..Config::test_stub(tmp.path())
    })
    .await;
    let name = config.server_name;
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
    wait_until(&client, &base, &name, false).await;
    wait_until(&client, &base, &name, true).await;
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

/// `Program.StartApp` checks the server's directories first (Ferrofin, once
/// logging has started): a layout that points two of them at one directory stops the
/// boot before a scheduled restore is applied or a database is opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clashing_layout_stops_the_boot_before_the_restore_and_the_database() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = Config {
        port: free_port(),
        ..Config::test_stub(tmp.path())
    };
    config.cache_dir = config.data_dir.clone();
    // `POST /Backup/Restore`'s marker, naming an archive the boot never reaches.
    let pending = config.data_dir.join("data/backups/restore-pending");
    std::fs::create_dir_all(pending.parent().expect("backups dir")).expect("mkdir");
    std::fs::write(&pending, "never-applied.zip").expect("schedule a restore");
    let database = config.database_path();

    let refused = tokio::time::timeout(
        REFUSAL_DEADLINE,
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Runtime::new()
                .expect("server runtime")
                .block_on(ferrofin_server::run(config))
        }),
    )
    .await
    .expect("a clashing layout must stop the boot, not serve")
    .expect("server thread did not panic")
    .expect_err("a clashing layout must not start");

    let message = format!("{refused:#}");
    assert!(
        message.contains("Expected to find only .jellyfin-cache but found marker for"),
        "{message}"
    );
    assert!(!database.exists(), "the refused start opened a database");
    assert!(pending.is_file(), "the refused start applied the restore");
}

/// Jellyfin 12.2's start-over guard: a server that has been set up refuses to
/// start once its database is gone — instead of creating an empty one and
/// seeding a new administrator — and creates nothing; a scheduled restore that
/// brings the database back starts it again, users and passwords intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_up_server_refuses_a_missing_database_until_a_restore_brings_it_back() {
    let _serial = BACKUP_SERIAL.lock().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("client");
    let make_config = |port| Config {
        server_name: "ferrofin-start-over".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        port,
        ..Config::test_stub(tmp.path())
    };
    // Seeding the configured administrator marks setup as completed.
    let (server, base, _) = spawn_server(&client, make_config).await;
    let tok = token(&client, &base).await;
    let admin_id = get_json(&client, &base, "/Users/Me", &tok).await["Id"].clone();
    assert!(admin_id.is_string(), "{admin_id}");
    let (st, manifest) = post_json(
        &client,
        &base,
        "/Backup/Create",
        &tok,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(st, 200, "{manifest}");
    let archive = std::path::PathBuf::from(manifest["Path"].as_str().expect("archive path"));
    assert_eq!(post(&client, &base, "/System/Shutdown", &tok).await, 204);
    tokio::task::spawn_blocking(move || server.join())
        .await
        .expect("join task")
        .expect("server thread did not panic")
        .expect("run exits cleanly");

    let database = make_config(0).database_path();
    for suffix in ["", "-wal", "-shm"] {
        let mut path = database.clone().into_os_string();
        path.push(suffix);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove {}: {e}", path.to_string_lossy()),
        }
    }

    let config = make_config(free_port());
    let refused = tokio::time::timeout(
        REFUSAL_DEADLINE,
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Runtime::new()
                .expect("server runtime")
                .block_on(ferrofin_server::run(config))
        }),
    )
    .await
    .expect("a set-up server without its database must refuse to start, not serve")
    .expect("server thread did not panic")
    .expect_err("a set-up server must not start without its database");
    let message = format!("{refused:#}");
    assert!(message.contains("the database does not exist"), "{message}");
    assert!(message.contains("IsStartupWizardCompleted"), "{message}");
    assert!(!database.exists(), "the refused start created a database");

    // `POST /Backup/Restore`'s marker, as the endpoint writes it.
    std::fs::write(
        archive.with_file_name("restore-pending"),
        archive
            .file_name()
            .expect("archive name")
            .as_encoded_bytes(),
    )
    .expect("schedule the restore");
    let (server, base, _) = spawn_server(&client, make_config).await;
    let tok = token(&client, &base).await;
    assert_eq!(
        get_json(&client, &base, "/Users/Me", &tok).await["Id"],
        admin_id,
        "the restored database's administrator, not a newly seeded one"
    );
    assert_eq!(post(&client, &base, "/System/Shutdown", &tok).await, 204);
    tokio::task::spawn_blocking(move || server.join())
        .await
        .expect("join task")
        .expect("server thread did not panic")
        .expect("run exits cleanly");
}

/// The shared boot helper (`support/boot.rs`) every real-server test uses: two
/// servers configured alike, the second handed the port the first holds, and
/// it comes up on another port rather than taking the first for its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_loses_its_port_retries_instead_of_attaching_to_the_holder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("client");
    let (first, first_base, first_config) = spawn_server(&client, |port| Config {
        port,
        ..Config::test_stub(&tmp.path().join("first"))
    })
    .await;
    let handed_the_taken_port = std::sync::atomic::AtomicBool::new(false);
    let (second, second_base, _) = spawn_server(&client, |port| Config {
        port: if handed_the_taken_port.swap(true, std::sync::atomic::Ordering::Relaxed) {
            port
        } else {
            first_config.port
        },
        ..Config::test_stub(&tmp.path().join("second"))
    })
    .await;
    assert_ne!(
        second_base, first_base,
        "attached to the server holding the port"
    );

    for (base, server) in [(first_base, first), (second_base, second)] {
        let login: serde_json::Value = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", CLIENT)
            .json(&serde_json::json!({"Username": "admin", "Pw": ""}))
            .send()
            .await
            .expect("auth request")
            .json()
            .await
            .expect("auth json");
        let tok = login["AccessToken"].as_str().expect("access token");
        assert_eq!(post(&client, &base, "/System/Shutdown", tok).await, 204);
        tokio::task::spawn_blocking(move || server.join())
            .await
            .expect("join task")
            .expect("server thread did not panic")
            .expect("run exits cleanly");
    }
}
