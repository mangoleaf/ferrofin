//! `ChildCount` of the libraries, the views and the playlists folder, end to
//! end.
//!
//! Jellyfin 12.1 and later count these folders' children, where 12.0 made a
//! number up (jellyfin/jellyfin#17881, v12.2 `DtoService.GetChildCountBatch`,
//! `GetChildCountSourceIds`, `GetChildCount`):
//! - a library counts its children;
//! - the playlists folder counts its own;
//! - a view that stands for no library (the Playlists view, the folders view)
//!   counts what its own browse lists.
//!
//! Boots the real composition root over a temp data dir, with a movies library,
//! a home videos library, two playlists and the folder view on. Over real HTTP
//! (`tower::oneshot` against the real router), every row of `GET /UserViews`
//! and of the user root's listing must report the `ChildCount` its own browse,
//! `GET /Items?parentId=<row>`, totals.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "view-child-counts-pw";

const CLIENT_AUTH: &str =
    r#"MediaBrowser Client="Child Counts", Device="Test Rig", DeviceId="rig-cc", Version="1.0.0""#;

/// A booted server plus what the test drives it with.
struct Harness {
    wired: WiredApp,
    /// The movies library's and the home videos library's media directories.
    movies_dir: String,
    home_dir: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the admin.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-view-child-counts".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let movies_dir = temp.path().join("movies");
    for title in ["Film Alpha (1999)", "Film Beta (2004)", "Film Gamma (2010)"] {
        let dir = movies_dir.join(title);
        std::fs::create_dir_all(&dir).expect("movie dir");
        std::fs::write(dir.join(format!("{title}.mkv")), [0u8; 1024]).expect("movie file");
    }
    let home_dir = temp.path().join("home");
    std::fs::create_dir_all(&home_dir).expect("home dir");
    for clip in ["Clip One", "Clip Two"] {
        std::fs::write(home_dir.join(format!("{clip}.mkv")), [0u8; 1024]).expect("clip file");
    }

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
        movies_dir: movies_dir.to_string_lossy().into_owned(),
        home_dir: home_dir.to_string_lossy().into_owned(),
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

/// Authenticates the seeded admin; returns the token-bearing auth header and
/// the user id.
async fn authenticate(router: &axum::Router) -> (String, String) {
    let (status, auth) = send(
        router,
        CLIENT_AUTH,
        "POST",
        "/Users/AuthenticateByName",
        Some(serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin authenticates");
    let token = auth["AccessToken"].as_str().expect("token").to_owned();
    let user_id = auth["User"]["Id"].as_str().expect("user id").to_owned();
    (
        format!(
            r#"MediaBrowser Client="Child Counts", Device="Test Rig", DeviceId="rig-cc", Version="1.0.0", Token="{token}""#
        ),
        user_id,
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

/// Each row of `listing` as `(Type, Name, ChildCount, its browse's TotalRecordCount)`.
async fn counts_and_browses(
    router: &axum::Router,
    auth_header: &str,
    user_id: &str,
    listing: &str,
) -> Vec<(String, String, Option<i64>, Option<i64>)> {
    let (status, page) = send(router, auth_header, "GET", listing, None).await;
    assert_eq!(status, StatusCode::OK, "{listing}: {page}");
    let mut rows = Vec::new();
    for item in page["Items"].as_array().expect("items") {
        let id = item["Id"].as_str().expect("id");
        let (status, browse) = send(
            router,
            auth_header,
            "GET",
            &format!("/Items?userId={user_id}&parentId={id}&limit=0"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "browse of {item}: {browse}");
        rows.push((
            item["Type"].as_str().expect("type").to_owned(),
            item["Name"].as_str().expect("name").to_owned(),
            item["ChildCount"].as_i64(),
            browse["TotalRecordCount"].as_i64(),
        ));
    }
    rows.sort();
    rows
}

// One client flow, asserted step by step — splitting it would re-boot and
// re-scan the server per step for no extra coverage.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn libraries_views_and_the_playlists_folder_count_what_they_list() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = authenticate(&router).await;

    // The folders view, which lists the user root's children.
    let (status, mut configuration) =
        send(&router, &auth_header, "GET", "/System/Configuration", None).await;
    assert_eq!(status, StatusCode::OK);
    configuration["EnableFolderView"] = serde_json::Value::Bool(true);
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        "/System/Configuration",
        Some(configuration),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "folder view on: {body}");

    // No remote metadata or image fetchers: a scan that asks TMDb waits on it.
    for (name, collection_type, dir, kind) in [
        ("Movies", "movies", &harness.movies_dir, "Movie"),
        ("Home", "homevideos", &harness.home_dir, "Video"),
    ] {
        let path = urlencoding(dir);
        let (status, body) = send(
            &router,
            &auth_header,
            "POST",
            &format!(
                "/Library/VirtualFolders?name={name}&collectionType={collection_type}&paths={path}&refreshLibrary=false"
            ),
            Some(serde_json::json!({"LibraryOptions": {"TypeOptions": [
                {"Type": kind, "MetadataFetchers": [], "ImageFetchers": []}
            ]}})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "library {name} added: {body}"
        );
    }
    let (status, _) = send(&router, &auth_header, "POST", "/Library/Refresh", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "scan queued");
    wait_for("the five scanned videos", async || {
        let (_, items) = send(
            &router,
            &auth_header,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie,Video&limit=0",
            None,
        )
        .await;
        (items["TotalRecordCount"].as_i64() == Some(5)).then_some(())
    })
    .await;
    for name in ["Road Trip", "Late Night"] {
        let (status, body) = send(
            &router,
            &auth_header,
            "POST",
            "/Playlists",
            Some(serde_json::json!({"Name": name, "UserId": user_id, "MediaType": "Video"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "playlist {name} created: {body}");
    }

    let views = counts_and_browses(
        &router,
        &auth_header,
        &user_id,
        &format!("/UserViews?userId={user_id}&fields=ChildCount"),
    )
    .await;
    for (kind, name, child_count, browse) in &views {
        assert_eq!(
            child_count, browse,
            "{kind} '{name}' counts what its browse lists: {views:?}"
        );
    }
    let count_of = |rows: &[(String, String, Option<i64>, Option<i64>)], name: &str| {
        rows.iter()
            .find(|(_, row, _, _)| row == name)
            .and_then(|(_, _, count, _)| *count)
    };
    assert_eq!(count_of(&views, "Movies"), Some(3), "{views:?}");
    assert_eq!(count_of(&views, "Home"), Some(2), "{views:?}");
    assert_eq!(
        count_of(&views, "Playlists"),
        Some(2),
        "the Playlists view: {views:?}"
    );
    // The folders view lists the user root's children: both libraries and the
    // playlists folder.
    assert_eq!(
        count_of(&views, "Folders"),
        Some(3),
        "the folders view: {views:?}"
    );

    let root = counts_and_browses(
        &router,
        &auth_header,
        &user_id,
        &format!("/Items?userId={user_id}&fields=ChildCount"),
    )
    .await;
    for (kind, name, child_count, browse) in &root {
        assert_eq!(
            child_count, browse,
            "{kind} '{name}' counts what its browse lists: {root:?}"
        );
    }
    assert!(
        root.iter()
            .any(|(kind, _, count, _)| kind == "ManualPlaylistsFolder" && *count == Some(2)),
        "the playlists folder counts its two playlists: {root:?}"
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
