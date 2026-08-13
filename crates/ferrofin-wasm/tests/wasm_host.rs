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

/// The happy-path fixture: a full `ferrofin:plugin@0.1.0` component with a
/// bump allocator, constant strings, and a task-id dispatcher.
const FIXTURE_WAT: &str = r#"
(component
  (core module $m
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 4096))
    (global $events (mut i32) (i32.const 0))
    (func $realloc (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $ret i32)
      global.get $bump
      local.set $ret
      (global.set $bump (i32.add (global.get $bump) (local.get 3)))
      local.get $ret)

    ;; ── constant strings ────────────────────────────────────────────
    (data (i32.const 16)  "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff") ;; id (36)
    (data (i32.const 64)  "Hello")                                ;; name (5)
    (data (i32.const 80)  "1.2.3")                                ;; version (5)
    (data (i32.const 96)  "Test plugin")                          ;; description (11)
    (data (i32.const 128) "{\"a\":1}")                            ;; default config (7)
    (data (i32.const 144) "greet")                                ;; task id (5)
    (data (i32.const 160) "Greet")                                ;; task name (5)
    (data (i32.const 176) "Says hi")                              ;; task description (7)
    (data (i32.const 192) "Test")                                 ;; task category (4)
    (data (i32.const 208) "kaboom")                               ;; guest error (6)
    (data (i32.const 224) "grow-denied")                          ;; limiter report (11)
    (data (i32.const 240) "grow-allowed")                         ;; limiter report (12)

    ;; descriptor: () -> record of 4 strings (8 i32s at the ret area)
    (func (export "descriptor") (result i32)
      (i32.store (i32.const 512) (i32.const 16))
      (i32.store (i32.const 516) (i32.const 36))
      (i32.store (i32.const 520) (i32.const 64))
      (i32.store (i32.const 524) (i32.const 5))
      (i32.store (i32.const 528) (i32.const 80))
      (i32.store (i32.const 532) (i32.const 5))
      (i32.store (i32.const 536) (i32.const 96))
      (i32.store (i32.const 540) (i32.const 11))
      i32.const 512)

    ;; default-config: () -> string
    (func (export "default-config") (result i32)
      (i32.store (i32.const 576) (i32.const 128))
      (i32.store (i32.const 580) (i32.const 7))
      i32.const 576)

    ;; tasks: () -> list<task-descriptor>; one element of 4 strings at 640
    (func (export "tasks") (result i32)
      (i32.store (i32.const 640) (i32.const 144))
      (i32.store (i32.const 644) (i32.const 5))
      (i32.store (i32.const 648) (i32.const 160))
      (i32.store (i32.const 652) (i32.const 5))
      (i32.store (i32.const 656) (i32.const 176))
      (i32.store (i32.const 660) (i32.const 7))
      (i32.store (i32.const 664) (i32.const 192))
      (i32.store (i32.const 668) (i32.const 4))
      (i32.store (i32.const 704) (i32.const 640))
      (i32.store (i32.const 708) (i32.const 1))
      i32.const 704)

    ;; run-task: (string) -> result<_, string>
    ;; ret area: tag @768, err ptr @772, err len @776
    (func (export "run-task") (param $ptr i32) (param $len i32) (result i32)
      ;; "ok" (len 2) -> ok
      (if (i32.eq (local.get $len) (i32.const 2))
        (then
          (i32.store (i32.const 768) (i32.const 0))
          (i32.store (i32.const 772) (i32.const 0))
          (i32.store (i32.const 776) (i32.const 0))))
      (if (i32.eq (local.get $len) (i32.const 2))
        (then (return (i32.const 768))))

      ;; "count" (len 5) -> err(single digit '0'+events)
      (if (i32.eq (local.get $len) (i32.const 5))
        (then
          (i32.store8 (i32.const 300)
            (i32.add (i32.const 48) (global.get $events)))
          (i32.store (i32.const 768) (i32.const 1))
          (i32.store (i32.const 772) (i32.const 300))
          (i32.store (i32.const 776) (i32.const 1))))
      (if (i32.eq (local.get $len) (i32.const 5))
        (then (return (i32.const 768))))

      ;; len-4 ids dispatch on the first byte
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 116)) ;; 't'rap
        (then unreachable))
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 108)) ;; 'l'oop
        (then (loop $spin (br $spin))))
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 103)) ;; 'g'row
        (then
          ;; ask for +96 pages (6 MiB); -1 means the limiter said no
          (if (i32.eq (memory.grow (i32.const 96)) (i32.const -1))
            (then
              (i32.store (i32.const 768) (i32.const 1))
              (i32.store (i32.const 772) (i32.const 224))
              (i32.store (i32.const 776) (i32.const 11)))
            (else
              (i32.store (i32.const 768) (i32.const 1))
              (i32.store (i32.const 772) (i32.const 240))
              (i32.store (i32.const 776) (i32.const 12))))))
      (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 103))
        (then (return (i32.const 768))))

      ;; "boom" (or anything else) -> err("kaboom")
      (i32.store (i32.const 768) (i32.const 1))
      (i32.store (i32.const 772) (i32.const 208))
      (i32.store (i32.const 776) (i32.const 6))
      i32.const 768)

    ;; on-event: (string, string) -> (); "die" traps, else counts
    (func (export "on-event") (param $np i32) (param $nl i32) (param $pp i32) (param $pl i32)
      (if (i32.eq (local.get $nl) (i32.const 3))
        (then unreachable))
      (global.set $events (i32.add (global.get $events) (i32.const 1))))
  )
  (core instance $i (instantiate $m))

  ;; Exported functions may only reference exportable named types, so each
  ;; record is bound to a fresh exported index (the `(export $x ...)` form)
  ;; and the function types reference THAT index.
  (type $descriptor0 (record
    (field "id" string) (field "name" string)
    (field "version" string) (field "description" string)))
  (export $descriptor "plugin-descriptor" (type $descriptor0))
  (type $task0 (record
    (field "id" string) (field "name" string)
    (field "description" string) (field "category" string)))
  (export $task "task-descriptor" (type $task0))

  (func $descriptor (result $descriptor)
    (canon lift (core func $i "descriptor") (memory $i "memory") string-encoding=utf8))
  (func $default-config (result string)
    (canon lift (core func $i "default-config") (memory $i "memory") string-encoding=utf8))
  (func $tasks (result (list $task))
    (canon lift (core func $i "tasks") (memory $i "memory") string-encoding=utf8))
  (func $run-task (param "task-id" string) (result (result (error string)))
    (canon lift (core func $i "run-task") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))
  (func $on-event (param "event-name" string) (param "event-json" string)
    (canon lift (core func $i "on-event") (memory $i "memory")
      (realloc (core func $i "realloc")) string-encoding=utf8))

  (export "descriptor" (func $descriptor))
  (export "default-config" (func $default-config))
  (export "tasks" (func $tasks))
  (export "run-task" (func $run-task))
  (export "on-event" (func $on-event))
)
"#;

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
    assert_eq!(plugin.tasks.len(), 1);
    assert_eq!(plugin.tasks[0].id, "greet");
    assert_eq!(plugin.tasks[0].category, "Test");

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
    assert_eq!(tasks.len(), 1);
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
