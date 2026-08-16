//! Direct tests of the `host::Host` implementation on [`HostState`] — the
//! exact functions a guest reaches through the WIT boundary — without any
//! wasm involved. This keeps the host-side coverage independent of the
//! env-gated real-guest test (which exercises the same impl *through* a
//! component).

use std::sync::Arc;

use ferrofin_wasm::bindings::host::Host as _;
use ferrofin_wasm::bindings::{HostState, types};
use ferrofin_wasm::capabilities::Collaborators;

mod common;
use common::{EnabledStub, OneMovieLibrary, RecordingSegments, one_shot_http};

fn state(collaborators: Arc<std::sync::OnceLock<Collaborators>>) -> HostState {
    HostState {
        plugin_name: "host-impl-test".to_owned(),
        plugin_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff99".to_owned(),
        config_json: r#"{"k":"v"}"#.to_owned(),
        limits: wasmtime::StoreLimitsBuilder::new().build(),
        memory_limit_bytes: 1024 * 1024,
        http: Arc::new(
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        ),
        http_timeout: std::time::Duration::from_secs(5),
        state_path: None,
        state_total_cap: 8 * 1024 * 1024,
        egress: std::sync::Arc::new(ferrofin_wasm::capabilities::EgressPolicy::parse(&[
            "*".to_owned()
        ])),
        collaborators,
        private_http_allowed: true, // tests hit a loopback listener
        wasi: HostState::empty_wasi(),
        table: wasmtime::component::ResourceTable::new(),
    }
}

#[test]
fn log_and_get_config_work_at_every_level() {
    let mut s = state(Arc::new(std::sync::OnceLock::new()));
    for level in [
        types::LogLevel::Trace,
        types::LogLevel::Debug,
        types::LogLevel::Info,
        types::LogLevel::Warn,
        types::LogLevel::Error,
    ] {
        s.log(level, "hello from the test guest".to_owned());
    }
    assert_eq!(s.get_config(), r#"{"k":"v"}"#);
}

#[test]
fn capability_calls_fail_cleanly_before_collaborators_are_armed() {
    let mut s = state(Arc::new(std::sync::OnceLock::new()));
    let err = s
        .query_items(types::ItemQuery {
            kinds: Vec::new(),
            parent_id: None,
            search_term: None,
            limit: None,
            user_id: None,
            is_played: None,
            is_favorite: None,
            is_resumable: None,
            genres: Vec::new(),
            sort_by: None,
            sort_descending: false,
            ids: Vec::new(),
        })
        .unwrap_err();
    assert!(err.contains("not available during plugin load"), "{err}");
    let err = s
        .write_media_segments(
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff01".to_owned(),
            Vec::new(),
        )
        .unwrap_err();
    assert!(err.contains("not available during plugin load"), "{err}");
}

#[test]
fn http_fetch_is_denied_during_plugin_load() {
    // Unarmed collaborators == inside a descriptor/default-config/tasks call
    // at load. http-fetch (the one outbound capability) must be refused there,
    // so a disabled plugin's load-time exports cannot phone home.
    let (url, _server) = one_shot_http("200 OK", b"leak");
    let mut s = state(Arc::new(std::sync::OnceLock::new()));
    let err = s
        .http_fetch(types::HttpRequest {
            method: "GET".to_owned(),
            url,
            headers: Vec::new(),
            body: None,
        })
        .unwrap_err();
    assert!(err.contains("not available during plugin load"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn http_fetch_round_trips_once_collaborators_are_armed() {
    let (url, server) = one_shot_http("204 No Content", b"");
    let cell = Arc::new(std::sync::OnceLock::new());
    cell.set(Collaborators {
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
        media_segments: Arc::new(RecordingSegments::default()),
        plugins: Arc::new(EnabledStub(b"{}".to_vec())),
    })
    .ok()
    .expect("fresh cell");
    // http-fetch uses a blocking client, so drive it off-runtime like the
    // plugin thread does.
    let response = tokio::task::spawn_blocking(move || {
        state(cell).http_fetch(types::HttpRequest {
            method: "GET".to_owned(),
            url,
            headers: Vec::new(),
            body: None,
        })
    })
    .await
    .unwrap()
    .expect("fetch succeeds");
    assert_eq!(response.status, 204);
    assert!(server.join().unwrap().starts_with(b"GET /hook"));
}

#[tokio::test(flavor = "multi_thread")]
async fn armed_capabilities_flow_through_the_trait_with_provider_scoping() {
    let segments = Arc::new(RecordingSegments::default());
    let cell = Arc::new(std::sync::OnceLock::new());
    cell.set(Collaborators {
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
        media_segments: segments.clone(),
        plugins: Arc::new(EnabledStub(b"{}".to_vec())),
    })
    .ok()
    .expect("fresh cell");

    let segments_for_assert = segments.clone();
    // Trait calls block on the runtime handle, so run them off-runtime the
    // way the plugin thread does.
    tokio::task::spawn_blocking(move || {
        let mut s = state(cell);
        let rows = s
            .query_items(types::ItemQuery {
                kinds: vec!["Movie".to_owned()],
                parent_id: None,
                search_term: None,
                limit: Some(5),
                user_id: None,
                is_played: None,
                is_favorite: None,
                is_resumable: None,
                genres: Vec::new(),
                sort_by: None,
                sort_descending: false,
                ids: Vec::new(),
            })
            .expect("query succeeds");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Big Buck Bunny");

        s.write_media_segments(
            rows[0].id.clone(),
            vec![types::MediaSegment {
                segment_type: "Intro".to_owned(),
                start_ticks: 0,
                end_ticks: 10,
            }],
        )
        .expect("write succeeds");
    })
    .await
    .unwrap();

    let created = segments_for_assert.created.lock().unwrap();
    assert_eq!(created.len(), 1);
    // The provider id is derived from the plugin id — never guest-supplied.
    assert_eq!(created[0].1, "wasm:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff99");
}

#[tokio::test(flavor = "multi_thread")]
async fn state_and_next_up_flow_through_the_host_trait() {
    // HostState holds a blocking reqwest client — build and drive it all on
    // a blocking thread (the same rule as the plugin runtime threads).
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        use ferrofin_wasm::bindings::host::Host as _;
        // Unarmed collaborators: next-up refuses during load, state works
        // (it is local, no exfil channel — allowed at load by design).
        let dir = tempfile::tempdir().unwrap();
        let mut s = state(Arc::new(std::sync::OnceLock::new()));
        s.state_path = Some(dir.path().join("p.state.json"));
        let err = s.next_up("00000000-0000-0000-0000-000000000001".into(), 5);
        assert!(err.unwrap_err().contains("during plugin load"));
        s.set_state("k".into(), Some(b"v".to_vec())).unwrap();
        assert_eq!(s.get_state("k".into()), Some(b"v".to_vec()));
        s.set_state("k".into(), None).unwrap();
        assert_eq!(s.get_state("k".into()), None);

        // Armed: next-up reaches the stub queue and enriches via user data.
        let cell = Arc::new(std::sync::OnceLock::new());
        cell.set(Collaborators {
            lyrics: std::sync::Arc::new(common::StubLyrics::default()),
            subtitles: std::sync::Arc::new(common::StubSubtitles::default()),
            collections: std::sync::Arc::new(common::StubCollections::default()),

            media_streams: std::sync::Arc::new(common::StubStreams),
            extractor: std::sync::Arc::new(common::StubExtractor::default()),
            analysis: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),

            users: std::sync::Arc::new(common::StubUsers),
            user_data: std::sync::Arc::new(common::StubUserData),
            tv: std::sync::Arc::new(common::StubTv),
            handle,
            library: Arc::new(OneMovieLibrary {
                seen: std::sync::Mutex::new(None),
            }),
            media_segments: Arc::new(RecordingSegments::default()),
            plugins: Arc::new(EnabledStub(b"{}".to_vec())),
        })
        .ok()
        .unwrap();
        let mut s = state(cell);
        let items = s
            .next_up("00000000-0000-0000-0000-000000000001".into(), 5)
            .expect("next-up through the trait");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Big Buck Bunny");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_reserved_state_keys_are_invisible_to_guests() {
    tokio::task::spawn_blocking(move || {
        use ferrofin_wasm::bindings::host::Host as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.state.json");
        // The host writes its watermark…
        ferrofin_wasm::capabilities::set_state(
            Some(&path),
            "host:scan-watermark",
            Some(b"42".to_vec()),
        )
        .unwrap();
        let mut s = state(Arc::new(std::sync::OnceLock::new()));
        s.state_path = Some(path);
        // …the guest can neither read nor rewrite nor delete it.
        assert_eq!(s.get_state("host:scan-watermark".into()), None);
        let err = s
            .set_state("host:scan-watermark".into(), Some(b"0".to_vec()))
            .unwrap_err();
        assert!(err.contains("reserved"), "{err}");
        let err = s.set_state("host:anything".into(), None).unwrap_err();
        assert!(err.contains("reserved"), "{err}");
    })
    .await
    .unwrap();
}
