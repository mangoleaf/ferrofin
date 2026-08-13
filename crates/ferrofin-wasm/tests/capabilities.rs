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
    Collaborators, MAX_QUERY_ROWS, http_fetch, query_items, write_media_segments,
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
                limit: Some(9999), // above the cap → clamped
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
