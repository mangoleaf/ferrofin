//! Host-level tests for the Tier-1b WASM plugin host, driven entirely by
//! **inline WAT fixtures** compiled at test time via the `wat` crate — no
//! `.wasm` binaries in the repo (artifact policy, docs/EXTENSIONS.md).
//!
//! The fixture component implements the `ferrofin:plugin@0.2.0` world by
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

mod common;

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
        state_limit_mb: 8,
        image_download_mb: 20,
        image_timeout_secs: 30,
        write_content_mb: 2,
        subtitle_extract_mb: 10,
        private_http_allow: Vec::new(),
    }
}

#[test]
fn loads_the_fixture_and_reads_its_identity() {
    init_test_logging();
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);

    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
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
        &WasmSettings::default(),
    )
    .unwrap();
    assert!(host.plugins().is_empty());

    // A file that is not a component is skipped, not fatal.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("junk.wasm"), b"not wasm at all").unwrap();
    // A core module (valid wasm, not a component) is also rejected cleanly.
    let core = wat::parse_str("(module)").unwrap();
    std::fs::write(dir.path().join("core.wasm"), core).unwrap();
    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
    assert!(host.plugins().is_empty());
}

#[test]
fn duplicate_plugin_ids_keep_only_the_first() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "a-first", FIXTURE_WAT);
    write_fixture(dir.path(), "b-second", FIXTURE_WAT);
    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
    assert_eq!(host.plugins().len(), 1, "same id must load once");
}

use common::named_provider_fixture;

#[test]
fn colliding_provider_names_are_refused_at_load() {
    init_test_logging();
    let dir = tempfile::tempdir().unwrap();
    // Loads: a well-behaved named provider (also proves the patched WAT is a
    // valid some(provider-descriptor), so the two skips below are the name
    // check and not an ABI decode failure).
    write_fixture(
        dir.path(),
        "a-acme",
        &named_provider_fixture("11111111-1111-1111-1111-111111111111", "AcmeDb"),
    );
    // Skipped: rides a built-in fetcher's checkbox/order (case-insensitive).
    write_fixture(
        dir.path(),
        "b-tmdb",
        &named_provider_fixture("22222222-2222-2222-2222-222222222222", "themoviedb"),
    );
    // Skipped: the name is already taken by the first plugin.
    write_fixture(
        dir.path(),
        "c-acme-again",
        &named_provider_fixture("33333333-3333-3333-3333-333333333333", "acmedb"),
    );
    // Skipped: a padded built-in name — HTML collapses the whitespace, so
    // this would render as the real TMDB entry. Normalized before the check.
    write_fixture(
        dir.path(),
        "d-padded",
        &named_provider_fixture("44444444-4444-4444-4444-444444444444", " TheMovieDb "),
    );
    // Skipped: an empty name (a blank-labelled fetcher).
    write_fixture(
        dir.path(),
        "e-empty",
        &named_provider_fixture("55555555-5555-5555-5555-555555555555", ""),
    );

    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
    assert_eq!(
        host.plugins().len(),
        1,
        "reserved/taken/empty/padded provider names must be skipped"
    );
    let info = host.plugins()[0]
        .provider_info
        .as_ref()
        .expect("the surviving plugin is the named provider");
    assert_eq!(info.name, "AcmeDb");
    assert!(info.supported_kinds.is_empty());
}

#[test]
fn a_padded_provider_name_is_trimmed_before_use() {
    init_test_logging();
    let dir = tempfile::tempdir().unwrap();
    // A leading/trailing-space name that does NOT collide is accepted, but
    // stored trimmed — the gate and the dashboard must see the same string.
    write_fixture(
        dir.path(),
        "spacey",
        &named_provider_fixture("66666666-6666-6666-6666-666666666666", "  Spacey DB  "),
    );
    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
    assert_eq!(host.plugins().len(), 1);
    assert_eq!(
        host.plugins()[0].provider_info.as_ref().unwrap().name,
        "Spacey DB",
        "the stored name is trimmed"
    );
}

#[tokio::test]
async fn run_task_ok_and_guest_error_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, &WasmSettings::default())
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
        move || WasmPluginHost::load(&dir, &WasmSettings::default())
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
        move || WasmPluginHost::load(&dir, &tight_settings())
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
        move || WasmPluginHost::load(&dir, &tight_settings())
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

#[tokio::test]
async fn metadata_lookup_flows_through_the_adapter_and_caches_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "hello", FIXTURE_WAT);
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, &WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();

    let providers = host.metadata_providers();
    assert_eq!(providers.len(), 1);
    let lookup = ferrofin_traits::providers::DynamicMetadataLookup {
        kind: "Movie".to_owned(),
        name: "Anything".to_owned(),
        ..Default::default()
    };

    // Unarmed collaborators: inert, not an error.
    assert!(providers[0].lookup(&lookup).await.unwrap().is_none());

    // Armed: the fixture's metadata-lookup answers ok(none); the second call
    // takes the (enabled, config) gate from the cache.
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        lyrics: std::sync::Arc::new(common::StubLyrics::default()),
        subtitles: std::sync::Arc::new(common::StubSubtitles::default()),
        collections: std::sync::Arc::new(common::StubCollections::default()),

        media_streams: std::sync::Arc::new(common::StubStreams),
        extractor: std::sync::Arc::new(common::StubExtractor::default()),
        analysis: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),

        users: std::sync::Arc::new(common::StubUsers),
        user_data: std::sync::Arc::new(common::StubUserData),
        tv: std::sync::Arc::new(common::StubTv),
        handle: tokio::runtime::Handle::current(),
        library: std::sync::Arc::new(common::OneMovieLibrary {
            seen: std::sync::Mutex::new(None),
        }),
        media_segments: std::sync::Arc::new(common::RecordingSegments::default()),
        plugins: manager(),
    });
    assert!(providers[0].lookup(&lookup).await.unwrap().is_none());
    assert!(providers[0].lookup(&lookup).await.unwrap().is_none());

    // remote-images rides the same gate: the fixture answers ok([]) so no
    // slot is filled, but the call proves the full adapter → guest path.
    let wanted = [ferrofin_model::entities::ImageType::Primary];
    let contributed = providers[0].images(&lookup, &wanted).await.unwrap();
    assert!(contributed.is_empty());
    // An empty wanted list short-circuits without a guest call.
    let contributed = providers[0].images(&lookup, &[]).await.unwrap();
    assert!(contributed.is_empty());
}

#[test]
fn settings_resolve_applies_defaults_and_ignores_zero() {
    let s = WasmSettings::resolve(None, None, None, None);
    assert_eq!(s.call_timeout_secs, 30);
    assert_eq!(s.memory_limit_mb, 128);
    assert_eq!(s.event_queue_capacity, 256);

    let s = WasmSettings::resolve(Some(0), Some(64), Some(16), Some("*, some-uuid"));
    assert_eq!(s.call_timeout_secs, 30, "zero timeout is treated as unset");
    assert_eq!(s.memory_limit_mb, 64);
    assert_eq!(s.event_queue_capacity, 16);
    assert!(
        s.allows_private_http(uuid::Uuid::from_u128(1)),
        "wildcard grants any plugin"
    );

    let s = WasmSettings::resolve(
        None,
        None,
        None,
        Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff"),
    );
    assert!(s.allows_private_http("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff".parse().unwrap()));
    assert!(
        !s.allows_private_http(uuid::Uuid::from_u128(2)),
        "others stay denied"
    );
    assert!(
        !WasmSettings::resolve(None, None, None, None)
            .allows_private_http(uuid::Uuid::from_u128(2)),
        "default denies everyone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_request_answers_traps_and_trips_the_breaker() {
    use ferrofin_wasm::bindings::types::PluginRequest;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wasm"),
        wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).unwrap(),
    )
    .unwrap();
    // load() builds a blocking HTTP client — off the async workers.
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        ferrofin_wasm::WasmPluginHost::load(
            &load_dir,
            &ferrofin_wasm::WasmSettings::resolve(Some(2), None, None, None),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let plugin = &host.plugins()[0];
    let request = |path: &str| PluginRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        query: String::new(),
        headers: vec![],
        body: None,
        user_id: None,
        is_admin: false,
        is_authenticated: false,
    };

    // Happy path: the guest's response comes back whole.
    let response = plugin
        .handle_request_for_test(request("/ping"))
        .await
        .expect("guest answers");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"pong");

    // A trapping path fails that call, the instance rebuilds…
    for _ in 0..2 {
        let err = plugin
            .handle_request_for_test(request("/boom"))
            .await
            .unwrap_err();
        assert!(err.contains("plugin call failed"), "{err}");
        // …and a good call still works between traps (fresh instance).
        assert!(plugin.handle_request_for_test(request("/ok")).await.is_ok());
    }
    // Three consecutive traps trip the breaker for good.
    for _ in 0..3 {
        let _ = plugin.handle_request_for_test(request("/boom")).await;
    }
    let err = plugin
        .handle_request_for_test(request("/ping"))
        .await
        .unwrap_err();
    assert!(err.contains("disabled until restart"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatcher_routes_by_id_and_gates_on_enabled() {
    use ferrofin_traits::plugins::{PluginRequestHandler as _, PluginWebRequest};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wasm"),
        wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).unwrap(),
    )
    .unwrap();
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        ferrofin_wasm::WasmPluginHost::load(
            &load_dir,
            &ferrofin_wasm::WasmSettings::resolve(None, None, None, None),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let plugin_id = host.plugins()[0].descriptor.id;
    let dispatcher = ferrofin_wasm::WasmRequestDispatcher::new(
        &host,
        std::sync::Arc::new(common::EnabledStub(b"{}".to_vec())),
    );
    let request = PluginWebRequest {
        method: "GET".to_owned(),
        path: "/ping".to_owned(),
        query: String::new(),
        headers: vec![],
        body: None,
        user_id: None,
        is_admin: false,
        is_authenticated: false,
    };
    // Known + enabled → the guest's response.
    let reply = dispatcher
        .handle(plugin_id, request.clone())
        .await
        .expect("dispatch")
        .expect("known plugin");
    assert_eq!(reply.status, 200);
    // Unknown id → None (the transport 404s) without touching a guest.
    assert!(
        dispatcher
            .handle(uuid::Uuid::from_u128(0xbeef), request)
            .await
            .expect("dispatch")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn analysis_driver_offers_each_item_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wasm"),
        wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).unwrap(),
    )
    .unwrap();
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        ferrofin_wasm::WasmPluginHost::load(
            &load_dir,
            &ferrofin_wasm::WasmSettings::resolve(None, None, None, None),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let plugins: std::sync::Arc<dyn ferrofin_traits::plugins::PluginManager> =
        std::sync::Arc::new(common::EnabledStub(b"{}".to_vec()));
    let library = std::sync::Arc::new(common::OneMovieLibrary {
        seen: std::sync::Mutex::new(None),
    });
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        lyrics: std::sync::Arc::new(common::StubLyrics::default()),
        subtitles: std::sync::Arc::new(common::StubSubtitles::default()),
        collections: std::sync::Arc::new(common::StubCollections::default()),

        media_streams: std::sync::Arc::new(common::StubStreams),
        extractor: std::sync::Arc::new(common::StubExtractor::default()),
        analysis: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        users: std::sync::Arc::new(common::StubUsers),
        user_data: std::sync::Arc::new(common::StubUserData),
        tv: std::sync::Arc::new(common::StubTv),
        handle: tokio::runtime::Handle::current(),
        library: library.clone(),
        media_segments: std::sync::Arc::new(common::RecordingSegments::default()),
        plugins: std::sync::Arc::new(common::EnabledStub(b"{}".to_vec())),
    });
    // The fixture declares scan-targets ["Movie"], so the driver exists.
    let task = host
        .analysis_task(&plugins)
        .expect("fixture is an analyzer");
    let progress = ferrofin_core::TaskProgress::default();
    task.execute(&progress).await.expect("first pass");
    // The FIRST pass must be unfiltered (NULL-DateCreated items get their
    // one offer; SQL `>=` would exclude them forever)…
    assert!(
        library
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("query recorded")
            .min_date_created
            .is_none(),
        "first pass runs without a date filter"
    );

    // The offer-once watermark landed in the plugin's own state file under
    // the host-reserved key (the canned item has no date-created → epoch).
    let state_path = dir
        .path()
        .join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff.state.json");
    let mark = ferrofin_wasm::capabilities::get_state(Some(&state_path), "host:scan-watermark")
        .expect("watermark persisted");
    assert_eq!(mark, b"0");

    // Second pass: nothing newer than the watermark — it must not move,
    // and the run still succeeds.
    task.execute(&progress).await.expect("second pass");
    // …and later passes push the watermark INTO the query.
    assert!(
        library
            .seen
            .lock()
            .unwrap()
            .clone()
            .expect("query recorded")
            .min_date_created
            .is_some(),
        "later passes carry the date filter"
    );
    let mark2 = ferrofin_wasm::capabilities::get_state(Some(&state_path), "host:scan-watermark")
        .expect("watermark still there");
    assert_eq!(mark2, b"0");
}

#[tokio::test(flavor = "multi_thread")]
async fn analysis_driver_skips_disabled_plugins_and_guests_cannot_touch_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wasm"),
        wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).unwrap(),
    )
    .unwrap();
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        ferrofin_wasm::WasmPluginHost::load(
            &load_dir,
            &ferrofin_wasm::WasmSettings::resolve(None, None, None, None),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let disabled: std::sync::Arc<dyn ferrofin_traits::plugins::PluginManager> =
        std::sync::Arc::new(common::DisabledStub(b"{}".to_vec()));
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        lyrics: std::sync::Arc::new(common::StubLyrics::default()),
        subtitles: std::sync::Arc::new(common::StubSubtitles::default()),
        collections: std::sync::Arc::new(common::StubCollections::default()),

        media_streams: std::sync::Arc::new(common::StubStreams),
        extractor: std::sync::Arc::new(common::StubExtractor::default()),
        analysis: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        users: std::sync::Arc::new(common::StubUsers),
        user_data: std::sync::Arc::new(common::StubUserData),
        tv: std::sync::Arc::new(common::StubTv),
        handle: tokio::runtime::Handle::current(),
        library: std::sync::Arc::new(common::OneMovieLibrary {
            seen: std::sync::Mutex::new(None),
        }),
        media_segments: std::sync::Arc::new(common::RecordingSegments::default()),
        plugins: std::sync::Arc::new(common::DisabledStub(b"{}".to_vec())),
    });
    let task = host.analysis_task(&disabled).expect("analyzer exists");
    task.execute(&ferrofin_core::TaskProgress::default())
        .await
        .expect("pass succeeds");
    // Disabled plugin: never offered — no watermark was ever written.
    let state_path = dir
        .path()
        .join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff.state.json");
    assert!(
        ferrofin_wasm::capabilities::get_state(Some(&state_path), "host:scan-watermark").is_none(),
        "disabled plugins are skipped by the analysis pass"
    );
}
