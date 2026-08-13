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
        collaborators,
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
fn http_fetch_round_trips_through_the_trait() {
    let (url, server) = one_shot_http("204 No Content", b"");
    let mut s = state(Arc::new(std::sync::OnceLock::new()));
    let response = s
        .http_fetch(types::HttpRequest {
            method: "GET".to_owned(),
            url,
            headers: Vec::new(),
            body: None,
        })
        .expect("fetch succeeds");
    assert_eq!(response.status, 204);
    assert!(server.join().unwrap().starts_with(b"GET /hook"));
}

#[tokio::test(flavor = "multi_thread")]
async fn armed_capabilities_flow_through_the_trait_with_provider_scoping() {
    let segments = Arc::new(RecordingSegments::default());
    let cell = Arc::new(std::sync::OnceLock::new());
    cell.set(Collaborators {
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
