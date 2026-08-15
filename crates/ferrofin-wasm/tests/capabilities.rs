//! Unit-level tests for the E2 host capabilities (`http-fetch`,
//! `query-items`, `write-media-segments`) — driven directly against the
//! capability functions with stub managers and a loopback HTTP listener.
//! No wasm involved: the guest-visible plumbing is covered by the fixture
//! and guest tests; these pin down the host-side semantics (caps, scheme
//! and type validation, provider-scoped replacement).

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use ferrofin_model::media_segments::MediaSegmentType;
use ferrofin_traits::library::LibraryManager;
use ferrofin_traits::media_segments::MediaSegmentManager;
use ferrofin_wasm::bindings::types::{HttpRequest, ItemQuery, MediaSegment};
use ferrofin_wasm::capabilities::{
    Collaborators, MAX_QUERY_ROWS, http_fetch, is_private_address, query_items,
    write_media_segments,
};

mod common;
use common::{EnabledStub, OneMovieLibrary, RecordingSegments, one_shot_http};

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
}

#[test]
fn http_fetch_round_trips_method_headers_and_body() {
    let (url, server) = one_shot_http("200 OK", b"pong");
    let response = http_fetch(
        &client(),
        "test-plugin",
        1024 * 1024,
        true, // loopback listener — this test exercises transport, not policy
        std::time::Duration::from_secs(5),
        &HttpRequest {
            method: "POST".to_owned(),
            url,
            headers: vec![("x-plugin".to_owned(), "hello".to_owned())],
            body: Some(b"ping".to_vec()),
        },
    )
    .expect("fetch succeeds");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"pong");
    assert!(
        response
            .headers
            .iter()
            .any(|(n, v)| n == "x-demo" && v == "yes"),
        "response headers surface to the guest"
    );
    let raw = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
    assert!(raw.starts_with("POST /hook"), "method+path sent: {raw}");
    assert!(raw.contains("x-plugin: hello"), "request header sent");
    assert!(raw.ends_with("ping"), "request body sent");
}

#[test]
fn http_fetch_rejects_non_http_schemes_and_oversized_bodies() {
    let err = http_fetch(
        &client(),
        "test-plugin",
        1024,
        true,
        std::time::Duration::from_secs(5),
        &HttpRequest {
            method: "GET".to_owned(),
            url: "file:///etc/passwd".to_owned(),
            headers: Vec::new(),
            body: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("not allowed"), "scheme rejected: {err}");

    // A 64-byte body against an 8-byte cap is refused, not truncated.
    let (url, _server) = one_shot_http("200 OK", &[0x41; 64]);
    let err = http_fetch(
        &client(),
        "test-plugin",
        8,
        true,
        std::time::Duration::from_secs(5),
        &HttpRequest {
            method: "GET".to_owned(),
            url,
            headers: Vec::new(),
            body: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("exceeds"), "oversized body refused: {err}");
}

fn collaborators(
    library: Arc<dyn LibraryManager>,
    segments: Arc<dyn MediaSegmentManager>,
) -> Collaborators {
    Collaborators {
        handle: tokio::runtime::Handle::current(),
        library,
        media_segments: segments,
        plugins: Arc::new(EnabledStub(b"{}".to_vec())),
        users: std::sync::Arc::new(common::StubUsers),
        user_data: std::sync::Arc::new(common::StubUserData),
        tv: std::sync::Arc::new(common::StubTv),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn query_items_maps_filters_in_and_the_projection_out() {
    let library = Arc::new(OneMovieLibrary {
        seen: Mutex::new(None),
    });
    let cx = collaborators(library.clone(), Arc::new(RecordingSegments::default()));

    // Run on a plain thread like the real plugin runtime (block_on inside).
    let rows = tokio::task::spawn_blocking(move || {
        query_items(
            &cx,
            &ItemQuery {
                kinds: vec!["Movie".to_owned()],
                parent_id: None,
                search_term: Some("bunny".to_owned()),
                limit: Some(9999),
                user_id: None,
                is_played: None,
                is_favorite: None,
                is_resumable: None,
                genres: Vec::new(),
                sort_by: None,
                sort_descending: false,
                ids: Vec::new(), // above the cap → clamped
            },
        )
    })
    .await
    .unwrap()
    .expect("query succeeds");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeff01");
    assert_eq!(rows[0].name, "Big Buck Bunny");
    assert_eq!(rows[0].kind, "Movie");
    assert_eq!(rows[0].path.as_deref(), Some("/media/movies/bbb.mkv"));
    assert_eq!(rows[0].run_time_ticks, Some(5_000_000_000));

    let seen = library
        .seen
        .lock()
        .unwrap()
        .clone()
        .expect("query recorded");
    assert_eq!(
        seen.include_item_types,
        vec![ferrofin_model::data::BaseItemKind::Movie]
    );
    assert_eq!(seen.search_term.as_deref(), Some("bunny"));
    assert_eq!(seen.limit, Some(i32::try_from(MAX_QUERY_ROWS).unwrap()));
}

#[tokio::test(flavor = "multi_thread")]
async fn query_items_rejects_unknown_kinds() {
    let cx = collaborators(
        Arc::new(OneMovieLibrary {
            seen: Mutex::new(None),
        }),
        Arc::new(RecordingSegments::default()),
    );
    let err = tokio::task::spawn_blocking(move || {
        query_items(
            &cx,
            &ItemQuery {
                kinds: vec!["Blockbuster".to_owned()],
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
            },
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(err.contains("unknown item kind"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn write_media_segments_replaces_only_this_providers_rows() {
    let segments = Arc::new(RecordingSegments::default());
    let cx = collaborators(
        Arc::new(OneMovieLibrary {
            seen: Mutex::new(None),
        }),
        segments.clone(),
    );
    let item = Uuid::from_u128(0xF00D);

    tokio::task::spawn_blocking(move || {
        write_media_segments(
            &cx,
            "wasm:test-plugin",
            &item.to_string(),
            &[
                MediaSegment {
                    segment_type: "Intro".to_owned(),
                    start_ticks: 0,
                    end_ticks: 100,
                },
                MediaSegment {
                    segment_type: "Outro".to_owned(),
                    start_ticks: 500,
                    end_ticks: 900,
                },
            ],
        )
    })
    .await
    .unwrap()
    .expect("write succeeds");

    let deleted = segments.deleted.lock().unwrap();
    assert_eq!(
        deleted.as_slice(),
        &[(item, "wasm:test-plugin".to_owned())],
        "delete is scoped to the plugin's provider id"
    );
    let created = segments.created.lock().unwrap();
    assert_eq!(created.len(), 2);
    assert_eq!(created[0].0.type_, MediaSegmentType::Intro);
    assert_eq!(created[0].1, "wasm:test-plugin");
    assert_eq!(created[1].0.type_, MediaSegmentType::Outro);
}

#[tokio::test(flavor = "multi_thread")]
async fn write_media_segments_validates_types_and_ranges() {
    let cx = collaborators(
        Arc::new(OneMovieLibrary {
            seen: Mutex::new(None),
        }),
        Arc::new(RecordingSegments::default()),
    );
    let cx2 = collaborators(
        Arc::new(OneMovieLibrary {
            seen: Mutex::new(None),
        }),
        Arc::new(RecordingSegments::default()),
    );
    let item = Uuid::from_u128(0xF00D).to_string();
    let item2 = item.clone();

    let err = tokio::task::spawn_blocking(move || {
        write_media_segments(
            &cx,
            "wasm:p",
            &item,
            &[MediaSegment {
                segment_type: "Advertisement".to_owned(),
                start_ticks: 0,
                end_ticks: 1,
            }],
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(err.contains("unknown segment-type"), "got: {err}");

    let err = tokio::task::spawn_blocking(move || {
        write_media_segments(
            &cx2,
            "wasm:p",
            &item2,
            &[MediaSegment {
                segment_type: "Intro".to_owned(),
                start_ticks: 100,
                end_ticks: 100, // empty range
            }],
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(err.contains("invalid segment range"), "got: {err}");
}

#[test]
fn http_fetch_denies_private_destinations_unless_allowlisted() {
    // Loopback, link-local (cloud metadata), RFC1918, and v4-mapped v6 are
    // all private; public addresses are not.
    for private in [
        "127.0.0.1",
        "169.254.169.254", // cloud metadata / link-local
        "10.1.2.3",        // RFC1918
        "172.16.0.1",
        "192.168.1.1",
        "100.64.0.1",      // CGNAT (Tailscale)
        "100.100.100.100", // CGNAT interior
        "198.18.0.5",      // benchmarking
        "192.0.0.1",       // IETF protocol assignments
        "240.0.0.1",       // reserved
        "224.0.0.1",       // multicast
        "::1",
        "fe80::1",
        "fd00::1",
        "ff02::1",           // v6 multicast
        "::ffff:10.0.0.1",   // v4-mapped RFC1918
        "::ffff:100.64.0.1", // v4-mapped CGNAT
    ] {
        assert!(
            is_private_address(private.parse().unwrap()),
            "{private} must be private"
        );
    }
    for public in [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "100.63.255.255", // just below CGNAT
        "100.128.0.0",    // just above CGNAT
        "2606:4700:4700::1111",
    ] {
        assert!(
            !is_private_address(public.parse().unwrap()),
            "{public} must be public"
        );
    }

    // A non-allowlisted plugin cannot reach the loopback listener; the
    // denial names the address class and the knob that grants access.
    let (url, _server) = one_shot_http("200 OK", b"nope");
    let err = http_fetch(
        &client(),
        "untrusted-plugin",
        1024,
        false,
        std::time::Duration::from_secs(5),
        &HttpRequest {
            method: "GET".to_owned(),
            url,
            headers: Vec::new(),
            body: None,
        },
    )
    .unwrap_err();
    assert!(
        err.contains("private/loopback") && err.contains("FERROFIN_WASM_PRIVATE_HTTP_ALLOW"),
        "got: {err}"
    );
}

#[test]
fn state_kv_round_trips_caps_and_deletes() {
    use ferrofin_wasm::capabilities::{get_state, set_state};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plugin.state.json");

    // Unset key → None; no path → clean error.
    assert_eq!(get_state(Some(&path), "missing"), None);
    assert!(
        set_state(None, "k", Some(b"v".to_vec()))
            .unwrap_err()
            .contains("not available")
    );

    // Round trip + overwrite + delete.
    set_state(Some(&path), "cursor", Some(b"42".to_vec())).unwrap();
    assert_eq!(get_state(Some(&path), "cursor"), Some(b"42".to_vec()));
    set_state(Some(&path), "cursor", Some(b"43".to_vec())).unwrap();
    assert_eq!(get_state(Some(&path), "cursor"), Some(b"43".to_vec()));
    set_state(Some(&path), "cursor", None).unwrap();
    assert_eq!(get_state(Some(&path), "cursor"), None);

    // Caps: oversized key / value / total are refused, state intact.
    let big_key = "k".repeat(257);
    assert!(
        set_state(Some(&path), &big_key, Some(b"v".to_vec()))
            .unwrap_err()
            .contains("key")
    );
    assert!(
        set_state(Some(&path), "big", Some(vec![0u8; 1024 * 1024 + 1]))
            .unwrap_err()
            .contains("value")
    );
    for i in 0..7 {
        set_state(
            Some(&path),
            &format!("blob{i}"),
            Some(vec![0u8; 1024 * 1024]),
        )
        .unwrap();
    }
    // The 8th megabyte (plus key bytes) crosses the 8 MiB total cap.
    assert!(
        set_state(Some(&path), "blob7", Some(vec![0u8; 1024 * 1024]))
            .unwrap_err()
            .contains("total")
    );
    // A corrupt file reads as empty rather than erroring.
    std::fs::write(&path, b"not json").unwrap();
    assert_eq!(get_state(Some(&path), "cursor"), None);
}
