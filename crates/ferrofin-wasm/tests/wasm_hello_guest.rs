//! End-to-end test against the REAL example guest (`examples/wasm-hello`),
//! built from source at test time — no committed `.wasm` (artifact policy).
//!
//! Gated behind `FERROFIN_WASM_GUEST_TESTS=1` (the `FERROFIN_FFMPEG_TESTS`
//! pattern) because the guest build needs the wasm32-wasip2 target and a few
//! minutes of cold compile. CI always sets it; locally it skips with a
//! message unless opted in.
//!
//! This test also produces the **measured RSS-per-plugin** number that
//! validates (or corrects) the `FERROFIN_WASM_MEMORY_LIMIT_MB=128` default —
//! read the printed `RSS delta` lines in the test output.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::EventManager;
use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
use ferrofin_wasm::{WasmPluginHost, WasmSettings};

/// Resident set size of this process in KiB, from `/proc/self/status`.
fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmRSS present")
}

/// Builds the example guest and returns the component artifact path, or
/// `None` (with an explanation) when the wasm toolchain is unavailable.
fn build_guest() -> Option<PathBuf> {
    let guest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/wasm-hello");
    let output = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&guest_dir)
        // Let the island's own rust-toolchain.toml govern — the test runner's
        // toolchain pin would otherwise leak in and miss the wasm target.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .output()
        .expect("spawn cargo for the guest build");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A missing target/toolchain is an environment gap, not a bug.
        assert!(
            stderr.contains("wasm32-wasip2") || stderr.contains("toolchain"),
            "guest build failed for a reason other than a missing wasm toolchain:\n{stderr}"
        );
        eprintln!("SKIP: wasm32-wasip2 toolchain unavailable:\n{stderr}");
        return None;
    }
    Some(guest_dir.join("target/wasm32-wasip2/release/ferrofin_wasm_hello.wasm"))
}

/// Plugin-manager stub: enabled, config carries a custom greeting.
struct EnabledStub;

#[async_trait::async_trait]
impl PluginManager for EnabledStub {
    async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
        Ok(Vec::new())
    }
    async fn get_plugin(&self, id: uuid::Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
        Ok(Some(PluginDescriptor {
            id,
            enabled: true,
            ..PluginDescriptor::default()
        }))
    }
    async fn enable_plugin(&self, _id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn disable_plugin(&self, _id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn remove_plugin(&self, _id: uuid::Uuid) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn get_plugin_configuration(&self, _id: uuid::Uuid) -> Result<Vec<u8>, ServiceError> {
        Ok(br#"{"Greeting":"Integration says hi"}"#.to_vec())
    }
    async fn set_plugin_configuration(
        &self,
        _id: uuid::Uuid,
        _config: Vec<u8>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn plugin_image(&self, _id: uuid::Uuid) -> Result<Option<PluginImage>, ServiceError> {
        Ok(None)
    }
    async fn get_repositories(
        &self,
    ) -> Result<Vec<ferrofin_model::updates::RepositoryInfo>, ServiceError> {
        Ok(Vec::new())
    }
    async fn set_repositories(
        &self,
        _repositories: Vec<ferrofin_model::updates::RepositoryInfo>,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
    async fn list_packages(
        &self,
    ) -> Result<Vec<ferrofin_model::updates::PackageInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn real_guest_loads_runs_and_reports_rss() {
    if std::env::var("FERROFIN_WASM_GUEST_TESTS").as_deref() != Ok("1") {
        eprintln!("SKIP: set FERROFIN_WASM_GUEST_TESTS=1 to build+run the real guest");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ferrofin_wasm=debug")
        .with_test_writer()
        .try_init();

    let Some(artifact) = tokio::task::spawn_blocking(build_guest).await.unwrap() else {
        return; // toolchain unavailable — skipped above with a message
    };

    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(&artifact, dir.path().join("hello.wasm")).unwrap();
    let wasm_size_kib = std::fs::metadata(&artifact).unwrap().len() / 1024;

    let rss_before = rss_kib();
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let rss_after_load = rss_kib();

    assert_eq!(host.plugins().len(), 1, "the real guest must load");
    let plugin = &host.plugins()[0];
    assert_eq!(plugin.descriptor.name, "Hello Ferrofin");
    assert_eq!(
        plugin.descriptor.id.to_string(),
        "3f9a2f60-88f1-4f52-b3f4-6f3a1c2d9e01"
    );
    assert_eq!(plugin.tasks.len(), 1);

    // Deliver a couple of events, then run the real task through the real
    // ScheduledTask adapter (enabled-gate + config fetch + actor round-trip).
    let manager: Arc<dyn PluginManager> = Arc::new(EnabledStub);
    let events = ferrofin_core::FerrofinEventManager::new();
    host.subscribe_events(&events, &manager);
    events.publish("PlaybackStart", "{}").await.unwrap();
    events.publish("LibraryChanged", "{}").await.unwrap();

    let tasks = host.scheduled_tasks(&manager);
    let progress = ferrofin_core::TaskProgress::default();
    tasks[0]
        .execute(&progress)
        .await
        .expect("the greet task must succeed");
    let rss_after_run = rss_kib();

    // The first load pays one-time process costs (cranelift, engine, paging
    // wasmtime's code in). Loading a SECOND host with the same artifact
    // isolates an upper bound on the marginal cost of one more plugin (upper
    // because it also duplicates the engine, which real hosts share).
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::copy(&artifact, dir2.path().join("hello-again.wasm")).unwrap();
    let rss_before_second = rss_kib();
    let _host2 = tokio::task::spawn_blocking({
        let dir = dir2.path().to_path_buf();
        move || WasmPluginHost::load(&dir, WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let rss_after_second = rss_kib();

    // The E1 deliverable: measured, printed, compared against the limit.
    println!("== WASM plugin RSS measurement (validates FERROFIN_WASM_MEMORY_LIMIT_MB) ==");
    println!(".wasm artifact size:            {wasm_size_kib} KiB");
    println!(
        "first load (incl. one-time):    {} KiB",
        rss_after_load.saturating_sub(rss_before)
    );
    println!(
        "after first task run:           {} KiB (total since before load)",
        rss_after_run.saturating_sub(rss_before)
    );
    println!(
        "second host, same plugin:       {} KiB (upper bound on marginal per-plugin cost)",
        rss_after_second.saturating_sub(rss_before_second)
    );
    println!(
        "configured memory limit:        {} MiB",
        WasmSettings::default().memory_limit_mb
    );
}
