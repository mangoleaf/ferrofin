//! Host-level tests for the Tier-1b WASM plugin host, driven entirely by
//! **inline WAT fixtures** compiled at test time via the `wat` crate — no
//! `.wasm` binaries in the repo (artifact policy, docs/EXTENSIONS.md).
//!
//! The fixture component implements the `ferrofin:plugin@0.5.0` world by
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
use ferrofin_traits::library::VirtualFolderManager;
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
    // Skipped: a name on a new-library allowlist that Ferrofin registers no
    // provider for (upstream's "Screen Grabber" image fetcher) — it would
    // start ticked in a new library, unlike every other plugin.
    write_fixture(
        dir.path(),
        "f-screen-grabber",
        &named_provider_fixture("77777777-7777-7777-7777-777777777777", "screen grabber"),
    );

    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
    assert_eq!(
        host.plugins().len(),
        1,
        "reserved/taken/empty/padded/allowlisted provider names must be skipped"
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

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn library_disabled_analyzer_items_reoffer_after_another_library_advances_watermark() {
    use ferrofin_traits::library::{LibraryManager, VirtualFolderManager};
    use ferrofin_traits::media_segments::MediaSegmentManager;
    use ferrofin_traits::persistence::ItemPersistenceService;
    let dir = tempfile::tempdir().unwrap();
    // Count actual guest analysis calls through the fixture's existing `count` export.
    let wat = FIXTURE_WAT.replace("(func (export \"scan-media\") (param i32) (result i32)",
        "(func (export \"scan-media\") (param i32) (result i32) (global.set $events (i32.add (global.get $events) (i32.const 1)))");
    write_fixture(dir.path(), "analyzer", &wat);
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        WasmPluginHost::load(&load_dir, &WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let name = host.plugins()[0].descriptor.name.clone();
    let plugin_id = host.plugins()[0].descriptor.id;
    let plugins: Arc<dyn PluginManager> = Arc::new(common::EnabledStub(b"{}".to_vec()));
    let db = ferrofin_db::Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let persistence = Arc::new(ferrofin_core::FerrofinItemPersistenceService::new(
        db.clone(),
    ));
    let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
        Arc::new(ferrofin_core::item_type_lookup::ItemTypeLookup::new());
    let library: Arc<dyn LibraryManager> = Arc::new(ferrofin_core::FerrofinLibraryManager::new(
        Arc::new(ferrofin_core::FerrofinItemRepository::new(
            db.clone(),
            lookup,
        )),
        Arc::new(ferrofin_core::FerrofinItemCountService::new(db.clone())),
        persistence.clone(),
        Arc::new(ferrofin_core::FerrofinPeopleRepository::new(db.clone())),
    ));
    let blocked = uuid::Uuid::new_v4();
    let allowed = uuid::Uuid::new_v4();
    let blocked_path = dir.path().join("blocked");
    let allowed_path = dir.path().join("allowed");
    std::fs::create_dir_all(&blocked_path).unwrap();
    std::fs::create_dir_all(&allowed_path).unwrap();
    let row = |id: uuid::Uuid, root: &std::path::Path, date| {
        ferrofin_db::entities::base_items::BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            type_: ferrofin_core::item_type_lookup::stored_type_name(
                ferrofin_model::data::BaseItemKind::Movie,
            )
            .unwrap()
            .to_owned(),
            path: Some(root.join("movie.mkv").to_string_lossy().into_owned()),
            date_created: chrono::DateTime::from_timestamp_micros(date),
            ..Default::default()
        }
    };
    persistence
        .save_items(&[
            row(blocked, &blocked_path, 10),
            row(allowed, &allowed_path, 20),
        ])
        .await
        .unwrap();
    let folders = Arc::new(ferrofin_core::FerrofinVirtualFolderManager::new(
        dir.path().join("views"),
    ));
    let mut options = ferrofin_model::configuration::LibraryOptions {
        path_infos: vec![ferrofin_model::configuration::MediaPathInfo {
            path: blocked_path.to_string_lossy().into_owned(),
        }],
        disabled_media_segment_providers: vec![name.to_uppercase()],
        ..Default::default()
    };
    folders
        .add_virtual_folder("Blocked", None, &options)
        .await
        .unwrap();
    folders
        .add_virtual_folder(
            "Allowed",
            None,
            &ferrofin_model::configuration::LibraryOptions {
                path_infos: vec![ferrofin_model::configuration::MediaPathInfo {
                    path: allowed_path.to_string_lossy().into_owned(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manager = Arc::new(
        ferrofin_core::FerrofinMediaSegmentManager::new(db, library.clone())
            .with_virtual_folders(folders.clone()),
    );
    ferrofin_traits::media_segments::MediaSegmentProvider::cache_segments(
        &ferrofin_core::CachedMediaSegmentProvider::new(
            name.clone(),
            plugin_id,
            plugins.clone(),
            vec![ferrofin_model::data::BaseItemKind::Movie],
            dir.path().join(format!("{plugin_id}.segments")),
        ),
        blocked,
        &[],
    )
    .await
    .unwrap();
    for provider in host.media_segment_providers(&plugins) {
        manager.register_segment_provider(provider);
    }
    assert_eq!(manager.registered_segment_providers()[0].name, name);
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        handle: tokio::runtime::Handle::current(),
        library,
        media_segments: manager,
        plugins: plugins.clone(),
        lyrics: Arc::new(common::StubLyrics::default()),
        subtitles: Arc::new(common::StubSubtitles::default()),
        collections: Arc::new(common::StubCollections::default()),
        media_streams: Arc::new(common::StubStreams),
        extractor: Arc::new(common::StubExtractor::default()),
        analysis: Arc::new(tokio::sync::Semaphore::new(1)),
        users: Arc::new(common::StubUsers),
        user_data: Arc::new(common::StubUserData),
        tv: Arc::new(common::StubTv),
    });
    let task = host.analysis_task(&plugins).unwrap();
    let progress = ferrofin_core::TaskProgress::default();
    task.execute(&progress).await.unwrap();
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('1'), "{count}");
    let state = dir.path().join(format!("{plugin_id}.state.json"));
    assert_eq!(
        ferrofin_wasm::capabilities::get_state(Some(&state), "host:scan-watermark").unwrap(),
        b"20"
    );
    let pending: Vec<uuid::Uuid> = serde_json::from_slice(
        &ferrofin_wasm::capabilities::get_state(Some(&state), "host:segment-policy-pending")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(pending, [blocked]);
    options.disabled_media_segment_providers.clear();
    folders
        .update_library_options("Blocked", &options)
        .await
        .unwrap();
    task.execute(&progress).await.unwrap();
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('2'), "{count}");
    assert!(
        ferrofin_wasm::capabilities::get_state(Some(&state), "host:segment-policy-pending")
            .is_none()
    );
    task.execute(&progress).await.unwrap();
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('2'), "{count}");
}

#[tokio::test(flavor = "multi_thread")]
async fn persisted_task_producer_registration_is_restored_only_for_a_loaded_guest() {
    use ferrofin_traits::media_segments::MediaSegmentProvider;
    let dir = tempfile::tempdir().unwrap();
    let wat = FIXTURE_WAT.replace(
        "(i32.store (i32.const 1156) (i32.const 1))",
        "(i32.store (i32.const 1156) (i32.const 0))",
    );
    write_fixture(dir.path(), "task-only", &wat);
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        WasmPluginHost::load(&load_dir, &WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let plugins: Arc<dyn PluginManager> = Arc::new(common::EnabledStub(b"{}".to_vec()));
    assert!(host.media_segment_providers(&plugins).is_empty());
    let descriptor = host.plugins()[0].descriptor.clone();
    let output_dir = dir.path().join(format!("{}.segments", descriptor.id));
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::write(output_dir.join(".ferrofin-config-incomplete"), b"partial").unwrap();
    assert!(
        host.media_segment_providers(&plugins).is_empty(),
        "failed or empty publication directories cannot register a task producer"
    );
    // A successful producer write persists this cache. No arbitrary DB provider rows are inspected.
    let producer = ferrofin_core::CachedMediaSegmentProvider::new(
        descriptor.name.clone(),
        descriptor.id,
        plugins.clone(),
        vec![ferrofin_model::data::BaseItemKind::Movie],
        dir.path().join(format!("{}.segments", descriptor.id)),
    );
    producer
        .cache_segments(uuid::Uuid::new_v4(), &[])
        .await
        .unwrap();
    drop(host);
    let load_dir = dir.path().to_path_buf();
    let restarted = tokio::task::spawn_blocking(move || {
        WasmPluginHost::load(&load_dir, &WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let providers = restarted.media_segment_providers(&plugins);
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].name(), descriptor.name);
    for entry in std::fs::read_dir(&output_dir).unwrap().flatten() {
        if entry
            .path()
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
        {
            std::fs::remove_file(entry.path()).unwrap();
        }
    }
    assert_eq!(
        restarted.media_segment_providers(&plugins).len(),
        1,
        "successful producer identity survives final-item cleanup"
    );
    std::fs::create_dir(
        dir.path()
            .join(format!("{}.segments", uuid::Uuid::new_v4())),
    )
    .unwrap();
    assert_eq!(
        restarted.media_segment_providers(&plugins).len(),
        1,
        "an unloaded descriptor cannot become a provider"
    );
}

/// Saves a real off/on transition during the guest's first output gate.
struct SwitchDuringWriteFolders {
    inner: Arc<ferrofin_core::FerrofinVirtualFolderManager>,
    reads: std::sync::atomic::AtomicUsize,
    provider_name: String,
}
#[async_trait::async_trait]
impl ferrofin_traits::library::VirtualFolderManager for SwitchDuringWriteFolders {
    async fn get_virtual_folders(
        &self,
    ) -> Result<Vec<ferrofin_model::entities_media::VirtualFolderInfo>, ServiceError> {
        let mut folders = self.inner.get_virtual_folders().await?;
        if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            let mut options = folders[0].library_options.clone().unwrap();
            options.disabled_media_segment_providers = vec![self.provider_name.clone()];
            self.inner
                .update_library_options("Movies", &options)
                .await?;
            folders = self.inner.get_virtual_folders().await?;
            // The setting is already enabled again when the guest/driver receives its result.
            options.disabled_media_segment_providers.clear();
            self.inner
                .update_library_options("Movies", &options)
                .await?;
        }
        Ok(folders)
    }
    async fn add_virtual_folder(
        &self,
        name: &str,
        collection_type: Option<ferrofin_model::entities::CollectionTypeOptions>,
        options: &ferrofin_model::configuration::LibraryOptions,
    ) -> Result<(), ServiceError> {
        self.inner
            .add_virtual_folder(name, collection_type, options)
            .await
    }
    async fn remove_virtual_folder(&self, name: &str) -> Result<(), ServiceError> {
        self.inner.remove_virtual_folder(name).await
    }
    async fn rename_virtual_folder(&self, name: &str, new_name: &str) -> Result<(), ServiceError> {
        self.inner.rename_virtual_folder(name, new_name).await
    }
    async fn add_media_path(
        &self,
        name: &str,
        path: &ferrofin_model::configuration::MediaPathInfo,
    ) -> Result<(), ServiceError> {
        self.inner.add_media_path(name, path).await
    }
    async fn update_media_path(
        &self,
        name: &str,
        path: &ferrofin_model::configuration::MediaPathInfo,
    ) -> Result<(), ServiceError> {
        self.inner.update_media_path(name, path).await
    }
    async fn remove_media_path(&self, name: &str, path: &str) -> Result<(), ServiceError> {
        self.inner.remove_media_path(name, path).await
    }
    async fn update_library_options(
        &self,
        name: &str,
        options: &ferrofin_model::configuration::LibraryOptions,
    ) -> Result<(), ServiceError> {
        self.inner.update_library_options(name, options).await
    }
}

/// The existing guest plus an actual canonical host import called from scan-media.
fn policy_writer_wat() -> String {
    let prelude = r#"
  (type $host-interface (instance
    (type $segment0 (record (field "segment-type" string) (field "start-ticks" s64) (field "end-ticks" s64)))
    (export "media-segment" (type $segment (eq $segment0)))
    (type $write (func (param "item-id" string) (param "segments" (list $segment)) (result (result (error string)))))
    (export "write-media-segments" (func (type $write)))
  ))
  (import "ferrofin:plugin/host@0.5.0" (instance $host (type $host-interface)))
  (alias export $host "write-media-segments" (func $write))
  (core module $mem
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 32768))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $ret i32)
      (local.set $ret (global.get $bump))
      (global.set $bump (i32.add (global.get $bump) (local.get 3)))
      (local.get $ret)))
  (core instance $mem (instantiate $mem))
  (core func $write (canon lower (func $write) (memory (core memory $mem "memory")) (realloc (core func $mem "realloc")) string-encoding=utf8))
  (core instance $host-lowered (export "write" (func $write)))
"#;
    FIXTURE_WAT
        .replace("(memory (export \"memory\") 1)", "(import \"mem\" \"memory\" (memory 1)) (export \"memory\" (memory 0))")
        .replace("(core module $m", "(core module $m (import \"host\" \"write\" (func $write (param i32 i32 i32 i32 i32)))")
        .replace("(core instance $i (instantiate $m))", "(core instance $i (instantiate $m (with \"mem\" (instance $mem)) (with \"host\" (instance $host-lowered))))")
        .replace("(func (export \"scan-media\") (param i32) (result i32)",
            "(func (export \"scan-media\") (param i32) (result i32) (global.set $events (i32.add (global.get $events) (i32.const 1))) (call $write (i32.load (local.get 0)) (i32.load (i32.add (local.get 0) (i32.const 4))) (i32.const 0) (i32.const 0) (i32.const 2048))")
        .replace("(component", &format!("(component{prelude}"))
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn analyzer_retains_a_host_skipped_write_even_when_library_is_reenabled_before_reply() {
    use ferrofin_traits::library::VirtualFolderManager;
    use ferrofin_traits::media_segments::MediaSegmentManager;
    init_test_logging();
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "policy-writer", &policy_writer_wat());
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        WasmPluginHost::load(&load_dir, &WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let plugins: Arc<dyn PluginManager> = Arc::new(common::EnabledStub(b"{}".to_vec()));
    let item = uuid::Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff01").unwrap();
    let db = ferrofin_db::Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let media = dir.path().join("media");
    let library = common::persisted_movie_library(&db, &media, item).await;
    assert_eq!(
        host.plugins().len(),
        1,
        "the policy writer must instantiate"
    );
    let inner = Arc::new(ferrofin_core::FerrofinVirtualFolderManager::new(
        dir.path().join("views"),
    ));
    inner
        .add_virtual_folder(
            "Movies",
            None,
            &ferrofin_model::configuration::LibraryOptions {
                path_infos: vec![ferrofin_model::configuration::MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let folders = Arc::new(SwitchDuringWriteFolders {
        inner: inner.clone(),
        reads: std::sync::atomic::AtomicUsize::new(0),
        provider_name: host.plugins()[0].descriptor.name.clone(),
    });
    let manager = Arc::new(
        ferrofin_core::FerrofinMediaSegmentManager::new(db, library.clone())
            .with_virtual_folders(folders),
    );
    let historical = ferrofin_model::media_segments::MediaSegmentDto {
        item_id: item,
        type_: ferrofin_model::media_segments::MediaSegmentType::Intro,
        start_ticks: 1,
        end_ticks: 100,
        ..Default::default()
    };
    ferrofin_traits::media_segments::MediaSegmentProvider::cache_segments(
        &ferrofin_core::CachedMediaSegmentProvider::new(
            host.plugins()[0].descriptor.name.clone(),
            host.plugins()[0].descriptor.id,
            plugins.clone(),
            vec![ferrofin_model::data::BaseItemKind::Movie],
            dir.path()
                .join(format!("{}.segments", host.plugins()[0].descriptor.id)),
        ),
        item,
        &[historical],
    )
    .await
    .unwrap();
    for provider in host.media_segment_providers(&plugins) {
        manager.register_segment_provider(provider);
    }
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        handle: tokio::runtime::Handle::current(),
        library,
        media_segments: manager.clone(),
        plugins: plugins.clone(),
        lyrics: Arc::new(common::StubLyrics::default()),
        subtitles: Arc::new(common::StubSubtitles::default()),
        collections: Arc::new(common::StubCollections::default()),
        media_streams: Arc::new(common::StubStreams),
        extractor: Arc::new(common::StubExtractor::default()),
        analysis: Arc::new(tokio::sync::Semaphore::new(1)),
        users: Arc::new(common::StubUsers),
        user_data: Arc::new(common::StubUserData),
        tv: Arc::new(common::StubTv),
    });
    let task = host.analysis_task(&plugins).unwrap();
    let progress = ferrofin_core::TaskProgress::default();
    let state = dir
        .path()
        .join(format!("{}.state.json", host.plugins()[0].descriptor.id));
    let snapshot = dir.path().join(format!(
        "{}.segments/{item}.json",
        host.plugins()[0].descriptor.id
    ));
    task.execute(&progress).await.unwrap();
    assert!(
        inner.get_virtual_folders().await.unwrap()[0]
            .library_options
            .as_ref()
            .unwrap()
            .disabled_media_segment_providers
            .is_empty()
    );
    let retained: Vec<ferrofin_model::media_segments::MediaSegmentDto> =
        serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(
        retained.len(),
        1,
        "the real host suppressed the first guest's empty replacement"
    );
    let pending: Vec<uuid::Uuid> = serde_json::from_slice(
        &ferrofin_wasm::capabilities::get_state(Some(&state), "host:segment-policy-pending")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(pending, [item]);
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('1'), "{count}");
    task.execute(&progress).await.unwrap();
    let replaced: Vec<ferrofin_model::media_segments::MediaSegmentDto> =
        serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
    assert!(replaced.is_empty());
    assert!(
        ferrofin_wasm::capabilities::get_state(Some(&state), "host:segment-policy-pending")
            .is_none()
    );
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('2'), "{count}");
    task.execute(&progress).await.unwrap();
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('2'), "{count}");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn state_only_analyzer_is_not_advertised_or_blocked_as_a_segment_producer() {
    use ferrofin_traits::media_segments::MediaSegmentManager;
    use ferrofin_traits::persistence::ItemPersistenceService;
    let dir = tempfile::tempdir().unwrap();
    let wat = FIXTURE_WAT.replace("(func (export \"scan-media\") (param i32) (result i32)",
        "(func (export \"scan-media\") (param i32) (result i32) (global.set $events (i32.add (global.get $events) (i32.const 1)))");
    write_fixture(dir.path(), "state-only", &wat);
    let load_dir = dir.path().to_path_buf();
    let host = tokio::task::spawn_blocking(move || {
        WasmPluginHost::load(&load_dir, &WasmSettings::default())
    })
    .await
    .unwrap()
    .unwrap();
    let plugins: Arc<dyn PluginManager> = Arc::new(common::EnabledStub(b"{}".to_vec()));
    assert!(host.media_segment_providers(&plugins).is_empty());
    let library = Arc::new(common::OneMovieLibrary {
        seen: std::sync::Mutex::new(None),
    });
    let item = uuid::Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff01").unwrap();
    let db = ferrofin_db::Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    ferrofin_core::FerrofinItemPersistenceService::new(db.clone())
        .save_items(&[ferrofin_traits::library::LibraryManager::get_item_by_id(
            library.as_ref(),
            item,
        )
        .await
        .unwrap()
        .unwrap()])
        .await
        .unwrap();
    let folders = Arc::new(ferrofin_core::FerrofinVirtualFolderManager::new(
        dir.path().join("views"),
    ));
    folders
        .add_virtual_folder(
            "Movies",
            None,
            &ferrofin_model::configuration::LibraryOptions {
                path_infos: vec![ferrofin_model::configuration::MediaPathInfo {
                    path: "/".to_owned(),
                }],
                disabled_media_segment_providers: vec![host.plugins()[0].descriptor.name.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manager = Arc::new(
        ferrofin_core::FerrofinMediaSegmentManager::new(db, library.clone())
            .with_virtual_folders(folders),
    );
    for candidate in host.media_segment_provider_candidates(&plugins) {
        assert!(
            !manager
                .adopt_loaded_segment_provider(candidate)
                .await
                .unwrap()
        );
    }
    assert!(manager.registered_segment_providers().is_empty());
    host.set_runtime_collaborators(ferrofin_wasm::capabilities::Collaborators {
        handle: tokio::runtime::Handle::current(),
        library,
        media_segments: manager.clone(),
        plugins: plugins.clone(),
        lyrics: Arc::new(common::StubLyrics::default()),
        subtitles: Arc::new(common::StubSubtitles::default()),
        collections: Arc::new(common::StubCollections::default()),
        media_streams: Arc::new(common::StubStreams),
        extractor: Arc::new(common::StubExtractor::default()),
        analysis: Arc::new(tokio::sync::Semaphore::new(1)),
        users: Arc::new(common::StubUsers),
        user_data: Arc::new(common::StubUserData),
        tv: Arc::new(common::StubTv),
    });
    let task = host.analysis_task(&plugins).unwrap();
    task.execute(&ferrofin_core::TaskProgress::default())
        .await
        .unwrap();
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('1'), "{count}");
    task.execute(&ferrofin_core::TaskProgress::default())
        .await
        .unwrap();
    let count = host.plugins()[0]
        .run_task_for_test("count".to_owned())
        .await
        .unwrap_err();
    assert!(count.contains('1'), "{count}");
    assert!(manager.registered_segment_providers().is_empty());
}
