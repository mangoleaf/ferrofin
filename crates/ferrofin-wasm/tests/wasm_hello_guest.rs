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

use ferrofin_traits::events::EventManager;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_wasm::capabilities::Collaborators;
use ferrofin_wasm::{WasmPluginHost, WasmSettings};

mod common;
use common::{EnabledStub, OneMovieLibrary, RecordingSegments, one_shot_http};

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
        // Only a genuinely-missing target/toolchain is an environment gap
        // worth a skip; every other failure (network, compile error in the
        // guest, WIT drift) is a real failure and must fail loudly.
        let missing_toolchain = stderr.contains("may not be installed")
            || stderr.contains("rustup target add wasm32-wasip2")
            || stderr.contains("toolchain '1.97.1' is not installed");
        assert!(
            missing_toolchain,
            "guest build FAILED (not a toolchain gap — this is a real error):\n{stderr}"
        );
        eprintln!("SKIP: wasm32-wasip2 toolchain unavailable:\n{stderr}");
        return None;
    }
    Some(guest_dir.join("target/wasm32-wasip2/release/ferrofin_wasm_hello.wasm"))
}

#[tokio::test]
// One linear flow (build → load → greet/events → analyze → RSS); splitting
// it would scatter the sequence the test proves.
#[allow(clippy::too_many_lines)]
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
    // The analyze task reports to a loopback listener, so the guest's id is
    // allowlisted for private HTTP — exercising the real grant path (the
    // deny path is covered in tests/capabilities.rs).
    let settings = WasmSettings {
        private_http_allow: vec!["3f9a2f60-88f1-4f52-b3f4-6f3a1c2d9e01".to_owned()],
        ..WasmSettings::default()
    };
    let host = tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || WasmPluginHost::load(&dir, &settings)
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
    assert_eq!(plugin.tasks.len(), 2, "greet + analyze advertised");

    // Deliver a couple of events, then run the real task through the real
    // ScheduledTask adapter (enabled-gate + config fetch + actor round-trip).
    let manager: Arc<dyn PluginManager> = Arc::new(EnabledStub(
        br#"{"Greeting":"Integration says hi","ReportUrl":""}"#.to_vec(),
    ));
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

    // ── E2: the analyze task drives all three capabilities ─────────────
    let segment_store = Arc::new(RecordingSegments::default());
    host.set_runtime_collaborators(Collaborators {
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
        library: Arc::new(OneMovieLibrary {
            seen: std::sync::Mutex::new(None),
        }),
        media_segments: segment_store.clone(),
        plugins: Arc::new(EnabledStub(b"{}".to_vec())),
    });
    let (report_url, report_server) = one_shot_http("200 OK", b"ok");
    let manager_with_report: Arc<dyn PluginManager> = Arc::new(EnabledStub(
        format!(r#"{{"Greeting":"hi","ReportUrl":"{report_url}"}}"#).into_bytes(),
    ));
    let tasks = host.scheduled_tasks(&manager_with_report);
    assert_eq!(tasks.len(), 2, "greet + analyze");
    tasks[1]
        .execute(&progress)
        .await
        .expect("analyze must succeed: query-items + write-media-segments + http-fetch");

    // query-items → the canned movie; write-media-segments → provider-scoped
    // replace with one Intro segment.
    let expected_item: uuid::Uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff01".parse().unwrap();
    let expected_provider = "wasm:3f9a2f60-88f1-4f52-b3f4-6f3a1c2d9e01";
    {
        let deleted = segment_store.deleted.lock().unwrap();
        assert_eq!(
            deleted.as_slice(),
            &[(expected_item, expected_provider.to_owned())]
        );
        let created = segment_store.created.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0.item_id, expected_item);
        assert_eq!(created[0].0.end_ticks, 30 * 10_000_000);
        assert_eq!(created[0].1, expected_provider);
    }
    // http-fetch → the report reached the loopback listener.
    let raw = String::from_utf8_lossy(&report_server.join().unwrap()).into_owned();
    assert!(raw.starts_with("POST /hook"), "report posted: {raw}");
    assert!(raw.contains("analyzed 1 movie(s)"), "report body: {raw}");

    // ── E3: the metadata-lookup export through the scanner's seam ──────
    let providers = host.metadata_providers();
    assert_eq!(providers.len(), 1);
    let offer = providers[0]
        .lookup(&ferrofin_traits::providers::DynamicMetadataLookup {
            item_id: uuid::Uuid::from_u128(0xB0B),
            kind: "Movie".to_owned(),
            name: "Big Buck Bunny".to_owned(),
            production_year: None,
            path: None,
            provider_ids: vec![("Imdb".to_owned(), "tt1254207".to_owned())],
        })
        .await
        .expect("lookup succeeds")
        .expect("the guest recognizes the demo title");
    assert_eq!(offer.production_year, Some(2008));
    assert_eq!(offer.community_rating, Some(7.9));
    assert_eq!(
        offer.genres,
        vec!["Animation".to_owned(), "Short".to_owned()]
    );
    assert_eq!(
        offer.provider_ids,
        vec![("HelloDb".to_owned(), "bbb-1".to_owned())]
    );
    assert!(offer.overview.unwrap_or_default().contains("WASM plugin"));
    // An unrecognized item is politely declined.
    let none = providers[0]
        .lookup(&ferrofin_traits::providers::DynamicMetadataLookup {
            kind: "Movie".to_owned(),
            name: "Some Other Film".to_owned(),
            ..Default::default()
        })
        .await
        .expect("lookup succeeds");
    assert!(none.is_none());

    // The first load pays one-time process costs (cranelift, engine, paging
    // wasmtime's code in). Loading a SECOND host with the same artifact
    // isolates an upper bound on the marginal cost of one more plugin (upper
    // because it also duplicates the engine, which real hosts share).
    let dir2 = tempfile::tempdir().unwrap();
    std::fs::copy(&artifact, dir2.path().join("hello-again.wasm")).unwrap();
    let rss_before_second = rss_kib();
    let _host2 = tokio::task::spawn_blocking({
        let dir = dir2.path().to_path_buf();
        move || WasmPluginHost::load(&dir, &WasmSettings::default())
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
