//! Boots [`ferrofin_server::run`] on a free port for the real-HTTP tests.

#![allow(dead_code)] // Each test binary uses its own subset.

use std::time::Duration;

use ferrofin_server::config::Config;

/// How long a server gets to bind and finish booting: the admin seed hashes a
/// password before the listener opens, so "booting" is seconds of CPU, and a
/// poll budget sized for a quiet laptop is exactly what fails on a loaded
/// runner.
pub const STARTUP_DEADLINE: Duration = Duration::from_mins(2);

/// How many ports a boot tries before giving up: each retry follows a port
/// another test took between the probe and the bind, which is already rare.
pub const BOOT_ATTEMPTS: usize = 5;

/// A port nobody is listening on right now (bind 0, read, release).
///
/// A probe, not a reservation: the listener closes before the server binds, so
/// a test starting at the same moment can be handed the same port.
/// [`spawn_server`] is what makes that survivable.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A server name no other test's server reports.
///
/// Every test server shares an admin user and password, so a sibling that won
/// the race for the port would answer every request happily and the run would
/// only fall over later, somewhere unrelated; the name is how a test tells its
/// own server apart.
pub fn unique_name(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

/// The `ServerName` whoever is listening on `base` reports, if anyone is.
pub async fn server_name_at(client: &reqwest::Client, base: &str) -> Option<String> {
    let body: serde_json::Value = client
        .get(format!("{base}/System/Info/Public"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body["ServerName"].as_str().map(str::to_owned)
}

/// Whether *this* test's server — not merely *a* server — answers on `base`.
pub async fn is_up(client: &reqwest::Client, base: &str, name: &str) -> bool {
    server_name_at(client, base).await.as_deref() == Some(name)
}

/// Start `run` on a free port and wait until this server answers there.
///
/// The config's `server_name` gets a [`unique_name`] suffix, and the returned
/// config carries it. If the port went to somebody else between the probe and
/// the bind, `run` returns an address-in-use error once it reaches the bind;
/// take another port rather than poll one that now belongs to another test.
///
/// ponytail: a retried boot with `enable_metrics` serves `/metrics` without its
/// `http_*` and scan series, because `run` installs the process-global
/// instruments before it binds and the lost attempt's provider took them;
/// binding before the metrics pipeline would fix it, if a test ever trips it.
pub async fn spawn_server(
    client: &reqwest::Client,
    make_config: impl Fn(u16) -> Config,
) -> (std::thread::JoinHandle<anyhow::Result<()>>, String, Config) {
    for _ in 0..BOOT_ATTEMPTS {
        let mut config = make_config(free_port());
        config.server_name = unique_name(&config.server_name);
        let base = format!("http://127.0.0.1:{}", config.port);
        let run_config = config.clone();
        // `run`'s future trips rustc's higher-ranked `Send` inference under
        // `tokio::spawn`; the binary drives it from `main`, so give it its own
        // thread and runtime here too.
        let server = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .expect("server runtime")
                .block_on(ferrofin_server::run(run_config))
        });
        let deadline = std::time::Instant::now() + STARTUP_DEADLINE;
        let mut lost_the_port = false;
        while std::time::Instant::now() < deadline {
            if is_up(client, &base, &config.server_name).await {
                return (server, base, config);
            }
            if server.is_finished() {
                // Only a lost port is worth another try; any other exit (a
                // refused boot, a panic) is the failure to report.
                let outcome = server.join();
                let port_taken = matches!(&outcome, Ok(Err(error)) if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
                }));
                assert!(port_taken, "the server exited before serving: {outcome:?}");
                lost_the_port = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(lost_the_port, "server never became reachable on {base}");
    }
    panic!("no free port survived long enough to bind");
}
