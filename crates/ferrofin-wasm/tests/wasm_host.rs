//! Host-level tests for the Tier-1b WASM plugin host, driven entirely by
//! **inline WAT fixtures** compiled at test time via the `wat` crate — no
//! `.wasm` binaries in the repo (artifact policy, PLAN_PLUGIN_TIERS.md).
//!
//! The fixture component implements the `ferrofin:plugin@0.1.0` world by
//! hand at the canonical-ABI level. Its `run-task` export dispatches on the
//! task id so one component covers every containment path:
//! `ok` succeeds · `boom` returns an orderly guest error · `trap` hits
//! `unreachable` · `loop` spins forever (epoch deadline) · `grow` asks for
//! ~6 MiB and reports whether the limiter denied it · `count` reports how
//! many events `on-event` has seen (as a single digit in the error string).

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::events::EventManager;
use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
use ferrofin_wasm::{WasmPluginHost, WasmSettings};

use ferrofin_wasm::TEST_FIXTURE_WAT as FIXTURE_WAT;

/// Makes the host's `error!`/`warn!` skip-reasons visible in test output.
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ferrofin_wasm=trace")
        .with_test_writer()
        .try_init();
}

/// Writes `wat` compiled to binary into `dir/{name}.wasm`.
fn write_fixture(dir: &std::path::Path, name: &str, wat_src: &str) -> PathBuf {
    let bytes = wat::parse_str(wat_src).expect("fixture WAT must compile");
    let path = dir.join(format!("{name}.wasm"));
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// A plugin manager stub: every plugin enabled, `{}` config.
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
        Ok(b"{}".to_vec())
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

fn manager() -> Arc<dyn PluginManager> {
    Arc::new(EnabledStub)
}

/// Small settings so the containment tests run fast: 1 s deadline, 2 MiB
/// memory cap (32 pages — the fixture's +96-page grow must be denied).
fn tight_settings() -> WasmSettings {
    WasmSettings {
        call_timeout_secs: 1,
        memory_limit_mb: 2,
        event_queue_capacity: 8,
    }
}

#[test]
fn loads_the_fixture_and_reads_its_identity() {
    init_test_logging();
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);

    let host = WasmPluginHost::load(dir.path(), WasmSettings::default()).unwrap();
    assert_eq!(host.plugins().len(), 1);

    let plugin = &host.plugins()[0];
    assert_eq!(
        plugin.descriptor.id.to_string(),
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff"
    );
    assert_eq!(plugin.descriptor.name, "Hello");
    assert_eq!(plugin.descriptor.version, "1.2.3");
    assert_eq!(plugin.descriptor.description, "Test plugin");
    assert_eq!(plugin.default_config, b"{\"a\":1}");
    assert_eq!(plugin.tasks.len(), 2);
    assert_eq!(plugin.tasks[0].id, "greet");
    assert_eq!(plugin.tasks[0].category, "Test");
    assert_eq!(plugin.tasks[1].id, "ok");

    let registered = host.registered_plugins();
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].descriptor.name, "Hello");
    // RegisteredPlugin::new normalizes can_uninstall to true (Jellyfin
    // parity) — and for a drop-in .wasm file that is also semantically true.
    assert!(registered[0].descriptor.can_uninstall);
}

#[test]
fn missing_dir_and_garbage_files_load_empty() {
    // Missing directory: an empty host, not an error.
    let host = WasmPluginHost::load(
        std::path::Path::new("/no/such/dir"),
        WasmSettings::default(),
    )
    .unwrap();
    assert!(host.plugins().is_empty());

    // A file that is not a component is skipped, not fatal.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("junk.wasm"), b"not wasm at all").unwrap();
    // A core module (valid wasm, not a component) is also rejected cleanly.
    let core = wat::parse_str("(module)").unwrap();
    std::fs::write(dir.path().join("core.wasm"), core).unwrap();
    let host = WasmPluginHost::load(dir.path(), WasmSettings::default()).unwrap();
    assert!(host.plugins().is_empty());
}

#[test]
fn duplicate_plugin_ids_keep_only_the_first() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "a-first", FIXTURE_WAT);
    write_fixture(dir.path(), "b-second", FIXTURE_WAT);
    let host = WasmPluginHost::load(dir.path(), WasmSettings::default()).unwrap();
    assert_eq!(host.plugins().len(), 1, "same id must load once");
}

#[tokio::test]
async fn run_task_ok_and_guest_error_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();

    let tasks = host.scheduled_tasks(&manager());
    assert_eq!(tasks.len(), 2, "both advertised tasks get adapters");
    let task = &tasks[0];
    assert_eq!(task.name(), "Greet");
    assert!(task.key().starts_with("wasm-aaaaaaaa"));

    // The ok path succeeds...
    host.plugins()[0]
        .run_task_for_test("ok".to_owned())
        .await
        .expect("the fixture's ok task must succeed");
    // ...and an orderly guest `err(string)` round-trips its message.
    let err = host.plugins()[0]
        .run_task_for_test("boom".to_owned())
        .await
        .unwrap_err();
    assert_eq!(err, "kaboom");

    // The fixture's advertised task id is `greet` (len 5 → the `count`
    // branch), so the adapter path surfaces the guest error as a
    // ServiceError carrying the guest's message ('0' events seen so far).
    let progress = ferrofin_core::TaskProgress::default();
    let err = task.execute(&progress).await.unwrap_err();
    assert!(
        err.to_string().contains('0'),
        "count task reports 0 events seen, got: {err}"
    );
}

#[tokio::test]
async fn events_published_on_the_manager_reach_the_guest() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();

    let events = ferrofin_core::FerrofinEventManager::new();
    host.subscribe_events(&events, &manager());

    // Two real deliveries...
    events.publish("PlaybackStart", "{}").await.unwrap();
    events.publish("PlaybackStopped", "{}").await.unwrap();
    // ...must be visible to the guest. Delivery is async (spawn + queue +
    // actor thread), so poll the guest's counter until it reaches 2.
    let task = &host.scheduled_tasks(&manager())[0];
    let progress = ferrofin_core::TaskProgress::default();
    let mut seen = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let err = task.execute(&progress).await.unwrap_err().to_string();
        if err.contains('2') {
            seen = err;
            break;
        }
    }
    assert!(seen.contains('2'), "guest never saw both events");
}

#[tokio::test]
async fn memory_limiter_denies_growth_beyond_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, tight_settings())
    })
    .await
    .unwrap()
    .unwrap();

    // Drive run-task("grow") through the runtime via a scheduled task run is
    // not possible (the advertised id is `greet`), so use the host's plugins
    // handle directly through the public adapter path: build a fake task
    // list is unnecessary — instead assert via the guest report string.
    let outcome = host.plugins()[0].run_task_for_test("grow".to_owned()).await;
    assert_eq!(
        outcome.unwrap_err(),
        "grow-denied",
        "the 2 MiB limiter must deny a 6 MiB grow"
    );
}

#[tokio::test]
async fn epoch_deadline_interrupts_a_spinning_guest_and_breaker_trips() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, tight_settings())
    })
    .await
    .unwrap()
    .unwrap();
    let plugin = &host.plugins()[0];

    // 1st failure: the infinite loop is interrupted by the 1 s deadline.
    let started = std::time::Instant::now();
    let err = plugin
        .run_task_for_test("loop".to_owned())
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "deadline did not interrupt the guest"
    );
    assert!(
        err.contains("plugin call failed"),
        "unexpected error: {err}"
    );

    // 2nd + 3rd failures: traps. The breaker (limit 3) must now be open.
    for _ in 0..2 {
        let _ = plugin.run_task_for_test("trap".to_owned()).await;
    }
    let err = plugin.run_task_for_test("ok".to_owned()).await.unwrap_err();
    assert!(
        err.contains("disabled until restart"),
        "breaker should be open after 3 consecutive failures, got: {err}"
    );
}

#[test]
fn settings_resolve_applies_defaults_and_ignores_zero() {
    let s = WasmSettings::resolve(None, None, None);
    assert_eq!(s.call_timeout_secs, 30);
    assert_eq!(s.memory_limit_mb, 128);
    assert_eq!(s.event_queue_capacity, 256);

    let s = WasmSettings::resolve(Some(0), Some(64), Some(16));
    assert_eq!(s.call_timeout_secs, 30, "zero timeout is treated as unset");
    assert_eq!(s.memory_limit_mb, 64);
    assert_eq!(s.event_queue_capacity, 16);
}
