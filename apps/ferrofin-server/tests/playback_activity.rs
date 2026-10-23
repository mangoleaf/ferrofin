//! The dashboard's playback activity entries, end to end.
//!
//! Jellyfin writes "{user} is playing {item} on {device}" and "{user} has
//! finished playing …" from event consumers of the session manager's playback
//! events (`PlaybackStartLogger` / `PlaybackStopLogger`), so every route that
//! reports playback writes one. That includes the obsolete
//! `POST`/`DELETE /PlayingItems/{itemId}` forms.
//!
//! Boots the real composition root over a temp data dir with a movies library,
//! and over real HTTP (`tower::oneshot` against the real router) reports
//! playback through both route families, counting the entries
//! `GET /System/ActivityLog/Entries` lists after each report.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "playback-activity-pw";

const CLIENT_AUTH: &str =
    r#"MediaBrowser Client="Playback Log", Device="Test Rig", DeviceId="rig-pa", Version="2.5.0""#;

/// A booted server plus what the test drives it with.
struct Harness {
    wired: WiredApp,
    /// The movies library's media directory.
    media_dir: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the admin.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-playback-activity".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let media_dir = temp.path().join("media");
    let dir = media_dir.join("Film Alpha (1999)");
    std::fs::create_dir_all(&dir).expect("movie dir");
    std::fs::write(dir.join("Film Alpha (1999).mkv"), [0u8; 1024]).expect("movie file");

    std::fs::create_dir_all(&config.data_dir).expect("data dir");
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    Harness {
        wired,
        media_dir: media_dir.to_string_lossy().into_owned(),
        _temp: temp,
    }
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).expect("valid JSON body")
}

/// Sends `method uri` with the session token and an optional JSON body;
/// returns status + JSON body.
async fn send(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth_header);
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    (status, body_json(response).await)
}

/// Authenticates the seeded admin; returns the token-bearing auth header.
async fn authenticate(router: &axum::Router) -> String {
    let (status, auth) = send(
        router,
        CLIENT_AUTH,
        "POST",
        "/Users/AuthenticateByName",
        Some(serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin authenticates");
    let token = auth["AccessToken"].as_str().expect("token");
    format!(
        r#"MediaBrowser Client="Playback Log", Device="Test Rig", DeviceId="rig-pa", Version="2.5.0", Token="{token}""#
    )
}

/// Polls `probe` until it returns `Some`, for at most 60 s.
async fn wait_for<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The names of the activity entries of `type_`, newest first.
async fn entries(router: &axum::Router, auth_header: &str, type_: &str) -> Vec<String> {
    let (status, page) = send(
        router,
        auth_header,
        "GET",
        "/System/ActivityLog/Entries?limit=100",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["Items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|entry| entry["Type"].as_str() == Some(type_))
        .filter_map(|entry| entry["Name"].as_str().map(str::to_owned))
        .collect()
}

// One client flow, asserted step by step — splitting it would re-boot and
// re-scan the server per step for no extra coverage.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn every_playback_report_writes_one_activity_entry() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let auth_header = authenticate(&router).await;

    // No remote metadata or image fetchers: a scan that asks TMDb waits on it.
    let path = urlencoding(&harness.media_dir);
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Movies&collectionType=movies&paths={path}&refreshLibrary=false"
        ),
        Some(serde_json::json!({"LibraryOptions": {"TypeOptions": [
            {"Type": "Movie", "MetadataFetchers": [], "ImageFetchers": []}
        ]}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    let (status, _) = send(&router, &auth_header, "POST", "/Library/Refresh", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "scan queued");
    let movie = wait_for("the scanned movie", async || {
        let (_, items) = send(
            &router,
            &auth_header,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie",
            None,
        )
        .await;
        items["Items"][0]["Id"].as_str().map(str::to_owned)
    })
    .await;
    let playing = "admin is playing Film Alpha (1999) on Test Rig";
    let finished = "admin has finished playing Film Alpha (1999) on Test Rig";

    // The current reports: one entry each.
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        "/Sessions/Playing",
        Some(serde_json::json!({"ItemId": movie})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        entries(&router, &auth_header, "VideoPlayback").await,
        [playing]
    );
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        "/Sessions/Playing/Stopped",
        Some(serde_json::json!({"ItemId": movie, "PositionTicks": 600_000_000})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        entries(&router, &auth_header, "VideoPlaybackStopped").await,
        [finished]
    );

    // The obsolete `PlayingItems` forms: the same entries, once each.
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        &format!("/PlayingItems/{movie}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        entries(&router, &auth_header, "VideoPlayback").await,
        [playing, playing],
        "POST /PlayingItems writes one start entry"
    );
    let (status, body) = send(
        &router,
        &auth_header,
        "DELETE",
        &format!("/PlayingItems/{movie}?positionTicks=600000000"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        entries(&router, &auth_header, "VideoPlaybackStopped").await,
        [finished, finished],
        "DELETE /PlayingItems writes one stop entry"
    );

    // A report naming no item publishes its event (204) but writes no entry:
    // upstream's loggers bail on `MediaInfo is null`, where the old handler wrote
    // "admin is playing  on Test Rig". The count pins the start; the stop logger
    // writes nothing for an item with no media type either way.
    for route in ["/Sessions/Playing", "/Sessions/Playing/Stopped"] {
        let (status, body) = send(
            &router,
            &auth_header,
            "POST",
            route,
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{route}: {body}");
    }
    let (_, page) = send(
        &router,
        &auth_header,
        "GET",
        "/System/ActivityLog/Entries?limit=100",
        None,
    )
    .await;
    let playback_entries = page["Items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|entry| {
            entry["Type"]
                .as_str()
                .is_some_and(|t| t.contains("Playback"))
        })
        .count();
    assert_eq!(
        playback_entries, 4,
        "a report with no item writes no entry: {page}"
    );
}

/// Percent-encodes a path for a query string, keeping `/`.
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                // Writing to a `String` cannot fail.
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}
