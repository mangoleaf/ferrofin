//! `GET /Persons` paging against the real composition root.
//!
//! Jellyfin 12.2's `GetPeopleItems` sets `MustHaveItem`: a credit with no
//! by-name `Person` item is neither returned nor counted, so `TotalRecordCount`
//! is the number of people a client can actually page through. This boots the
//! server over a temp DB, scans a movie whose NFO credits five actors, deletes
//! two of their `Person` items, and checks over real HTTP that every page
//! reports the three that remain and the pages tile exactly that set.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "persons-pw";

const CLIENT_AUTH: &str =
    r#"MediaBrowser Client="Persons", Device="Test Rig", DeviceId="rig-persons", Version="1.0.0""#;

const ACTORS: [&str; 5] = ["Ada", "Ben", "Cy", "Dee", "Eli"];

/// A booted server plus what the test drives it with.
struct Harness {
    wired: WiredApp,
    db: Database,
    /// The movies library's media directory.
    media_dir: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB with one movie crediting
/// [`ACTORS`] in its NFO, and seeds the admin.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-persons".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let media_dir = temp.path().join("media");
    let movie = media_dir.join("Credits Film (2001)");
    std::fs::create_dir_all(&movie).expect("movie dir");
    std::fs::write(movie.join("Credits Film (2001).mkv"), [0u8; 1024]).expect("movie file");
    let actors = ACTORS
        .map(|name| format!("<actor><name>{name}</name><type>Actor</type></actor>"))
        .concat();
    std::fs::write(
        movie.join("movie.nfo"),
        format!("<movie><title>Credits Film</title>{actors}</movie>"),
    )
    .expect("nfo");

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
        db,
        media_dir: media_dir.to_string_lossy().into_owned(),
        _temp: temp,
    }
}

/// Sends `method uri` with the session token; returns status + JSON body.
async fn send(
    router: &axum::Router,
    auth_header: &str,
    method: &str,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, auth_header)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("valid JSON body")
    };
    (status, body)
}

/// Authenticates the seeded admin; returns the token-bearing auth header.
async fn authenticate(router: &axum::Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, CLIENT_AUTH)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK, "admin authenticates");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let auth: serde_json::Value = serde_json::from_slice(&bytes).expect("auth JSON");
    let token = auth["AccessToken"].as_str().expect("token");
    format!(
        r#"MediaBrowser Client="Persons", Device="Test Rig", DeviceId="rig-persons", Version="1.0.0", Token="{token}""#
    )
}

/// The `Name`s of a `/Persons` page.
fn names(page: &serde_json::Value) -> Vec<String> {
    page["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .map(|p| p["Name"].as_str().expect("Name").to_owned())
        .collect()
}

/// Percent-encodes a filesystem path for a query string.
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

#[tokio::test]
async fn persons_total_counts_only_people_with_an_item_and_pages_tile_them() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let auth = authenticate(&router).await;

    let media = urlencoding(&harness.media_dir);
    let (status, body) = send(
        &router,
        &auth,
        "POST",
        &format!("/Library/VirtualFolders?name=Movies&collectionType=movies&paths={media}&refreshLibrary=false"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    let refresh_end = async || {
        let (_, tasks) = send(&router, &auth, "GET", "/ScheduledTasks").await;
        let task = tasks
            .as_array()
            .expect("tasks")
            .iter()
            .find(|task| task["Key"] == "RefreshLibrary")
            .expect("RefreshLibrary task")
            .clone();
        (task["State"] == "Idle", task["LastExecutionResult"].clone())
    };
    let (_, previous) = refresh_end().await;
    let (status, _) = send(&router, &auth, "POST", "/Library/Refresh").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "scan queued");

    // Wait for the whole scan, so nothing it writes later can re-create the
    // Person items deleted below.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let (idle, result) = refresh_end().await;
        if idle && result["EndTimeUtc"] != previous["EndTimeUtc"] {
            assert_eq!(result["Status"], "Completed", "{result}");
            break;
        }
        assert!(Instant::now() < deadline, "scan did not finish: {result}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The scan credits every actor and materializes each one's Person item.
    let (_, page) = send(&router, &auth, "GET", "/Persons").await;
    assert_eq!(page["TotalRecordCount"], ACTORS.len(), "{page}");
    assert_eq!(names(&page), ACTORS);

    // Two credits lose their by-name item: they stay credited on the movie but
    // can no longer be handed back as a Person.
    let deleted = sqlx::query(
        r#"DELETE FROM "BaseItems"
           WHERE "Type" = 'MediaBrowser.Controller.Entities.Person' AND "Name" IN ('Ben', 'Dee')"#,
    )
    .execute(harness.db.writer())
    .await
    .expect("delete person items");
    assert_eq!(deleted.rows_affected(), 2);

    // Every page reports the three returnable people, and the pages tile them.
    let mut tiled = Vec::new();
    for start in [0, 2, 4] {
        let (status, page) = send(
            &router,
            &auth,
            "GET",
            &format!("/Persons?startIndex={start}&limit=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["TotalRecordCount"], 3, "page at {start}: {page}");
        assert_eq!(page["StartIndex"], start, "{page}");
        tiled.extend(names(&page));
    }
    assert_eq!(tiled, ["Ada", "Cy", "Eli"]);

    let (_, all) = send(&router, &auth, "GET", "/Persons").await;
    assert_eq!(all["TotalRecordCount"], 3, "{all}");
    assert_eq!(names(&all), ["Ada", "Cy", "Eli"]);

    // Listing never re-creates the missing items: the filtered names never
    // reach the by-name resolver (12.2's `GetPerson` only reads).
    let recreated: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "BaseItems"
           WHERE "Type" = 'MediaBrowser.Controller.Entities.Person' AND "Name" IN ('Ben', 'Dee')"#,
    )
    .fetch_one(harness.db.pool())
    .await
    .expect("count person items");
    assert_eq!(recreated, 0);
}
