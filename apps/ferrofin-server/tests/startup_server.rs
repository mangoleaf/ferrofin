//! The startup server (gl-44, `SetupServer`): the HTTP port opens before the
//! database is opened and migrated and answers while the server starts, then
//! the same port serves the lifetime's router, with no refused connection in
//! between.

use std::time::{Duration, Instant};

use ferrofin_server::config::Config;
use reqwest::StatusCode;

#[path = "support/boot.rs"]
mod boot;

/// `GET /health/ready` is 503 failing `startup` from the moment the port
/// answers until the router is wired, then 200 from the same port.
#[tokio::test(flavor = "multi_thread")]
async fn the_port_answers_while_starting_then_serves_the_router() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    for _ in 0..boot::BOOT_ATTEMPTS {
        let config = Config {
            server_name: boot::unique_name("ferrofin-startup"),
            port: boot::free_port(),
            ..Config::test_stub(tmp.path())
        };
        let base = format!("http://127.0.0.1:{}", config.port);
        let name = config.server_name.clone();
        let run_config = config.clone();
        let server = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .expect("server runtime")
                .block_on(ferrofin_server::run(run_config))
        });

        // Every answer from the first one on, as (status, body).
        let mut answers: Vec<(StatusCode, String)> = Vec::new();
        let deadline = Instant::now() + boot::STARTUP_DEADLINE;
        while answers
            .last()
            .is_none_or(|(status, _)| *status != StatusCode::OK)
        {
            assert!(
                Instant::now() < deadline,
                "never ready on {base}: {answers:?}"
            );
            if server.is_finished() {
                break;
            }
            match client.get(format!("{base}/health/ready")).send().await {
                Ok(response) => {
                    let status = response.status();
                    answers.push((status, response.text().await.unwrap_or_default()));
                }
                Err(error) => assert!(
                    answers.is_empty(),
                    "the port refused a request after it first answered: {error}; {answers:?}"
                ),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if server.is_finished() {
            // Another test took the port between the probe and the bind.
            let outcome = server.join();
            assert!(
                matches!(&outcome, Ok(Err(error)) if error.chain().any(|cause| cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse))),
                "the server exited before serving: {outcome:?}"
            );
            continue;
        }
        assert!(
            boot::is_up(&client, &base, &name).await,
            "another server answered on {base}"
        );
        let (status, body) = &answers[0];
        assert_eq!(*status, StatusCode::SERVICE_UNAVAILABLE, "{answers:?}");
        assert!(body.contains("\"startup\""), "{body}");
        return;
    }
    panic!("every port tried was taken");
}
