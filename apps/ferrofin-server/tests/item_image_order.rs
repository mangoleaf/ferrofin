//! An item's images list in one order everywhere, over the real router and
//! composition root: `BackdropImageTags[i]` (and `/Items/{id}/Images`) name
//! the file `/Items/{id}/Images/Backdrop/{i}` serves.
//!
//! Upstream reads an item's images through one `Include(e => e.Images!.
//! OrderBy(i => i.Id))` (v12.2 `BaseItemRepository.QueryBuilding.cs:274`), so
//! its DTO and its by-index routes cannot disagree. Ferrofin's readers are
//! separate queries; they must share the order (type, then `rowid`), or rows
//! whose ids do not sort in insertion order (adopted Jellyfin rows, uploads)
//! put one file's tag on another file's index.

use std::path::PathBuf;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "image-order-admin-pw";
const MOVIE: &str = "movies/Crimson Square (1963)/Crimson Square (1963).mkv";
/// `BaseItemImageInfos.ImageType` of a backdrop.
const BACKDROP: i32 = 2;

/// A booted server that scanned one movie, its database, and a session.
struct Fixture {
    _wired: ferrofin_server::state::WiredApp,
    temp: tempfile::TempDir,
    db: Database,
    router: axum::Router,
    token: String,
    user: String,
    movie: String,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-item-image-order".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let movie_path = temp.path().join(MOVIE);
    std::fs::create_dir_all(movie_path.parent().expect("parent")).expect("dir");
    std::fs::write(&movie_path, b"").expect("movie");
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: PathBuf::from("ffmpeg"),
        ffprobe: PathBuf::from("ffprobe"),
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
    let router = ferrofin_api::create_router(wired.state.clone());
    let login = json_of(
        call(
            &router,
            "POST",
            "/Users/AuthenticateByName",
            None,
            Some(json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
        )
        .await,
    );
    let token = login["AccessToken"].as_str().expect("token").to_owned();
    let user = login["User"]["Id"].as_str().expect("user id").to_owned();
    let path = encode(&temp.path().join("movies").to_string_lossy());
    let (status, _) = call(
        &router,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Movies&collectionType=movies&paths={path}&refreshLibrary=false"
        ),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan")
    );
    let items = json_of(
        call(
            &router,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie",
            Some(&token),
            None,
        )
        .await,
    );
    let movie = items["Items"][0]["Id"]
        .as_str()
        .expect("the scanned movie")
        .to_owned();
    Fixture {
        _wired: wired,
        temp,
        db,
        router,
        token,
        user,
        movie,
    }
}

#[tokio::test]
async fn backdrop_tags_name_the_files_served_by_index() {
    // `_wired` stays bound: dropping it would stop the server's services.
    let Fixture {
        _wired,
        temp,
        db,
        router,
        token,
        user,
        movie,
    } = fixture().await;

    // Inserted a, b, c; their ids sort b, c, a, as adopted rows' may.
    let mut files = Vec::new();
    for (row_id, name) in [(3_u128, "a"), (1, "b"), (2, "c")] {
        let file = temp.path().join(format!("{name}.jpg"));
        std::fs::write(&file, name).expect("backdrop");
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos" ("Id","ItemId","ImageType","Path","Width","Height")
               VALUES (?1,?2,?3,?4,0,0)"#,
        )
        .bind(guid_to_db(Uuid::from_u128(row_id)))
        .bind(guid_to_db(Uuid::parse_str(&movie).expect("id")))
        .bind(BACKDROP)
        .bind(file.to_string_lossy().as_ref())
        .execute(db.pool())
        .await
        .expect("insert backdrop");
        files.push(file.to_string_lossy().into_owned());
    }

    let mut served = Vec::new();
    for index in 0..3 {
        let (status, bytes) = call(
            &router,
            "GET",
            &format!("/Items/{movie}/Images/Backdrop/{index}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        served.push(String::from_utf8(bytes.to_vec()).expect("utf-8"));
    }
    assert_eq!(served, ["a", "b", "c"], "by index, in insertion order");

    let infos = json_of(
        call(
            &router,
            "GET",
            &format!("/Items/{movie}/Images"),
            Some(&token),
            None,
        )
        .await,
    );
    let backdrops: Vec<&Value> = infos
        .as_array()
        .expect("image infos")
        .iter()
        .filter(|info| info["ImageType"] == "Backdrop")
        .collect();
    let paths: Vec<&str> = backdrops
        .iter()
        .map(|info| info["Path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, files, "/Images lists them in served order");
    let tags: Vec<&str> = backdrops
        .iter()
        .map(|info| info["ImageTag"].as_str().expect("tag"))
        .collect();

    let detail = json_of(
        call(
            &router,
            "GET",
            &format!("/Users/{user}/Items/{movie}"),
            Some(&token),
            None,
        )
        .await,
    );
    assert_eq!(detail["BackdropImageTags"], json!(tags), "item detail");
    let page = json_of(
        call(
            &router,
            "GET",
            &format!("/Items?ids={movie}"),
            Some(&token),
            None,
        )
        .await,
    );
    assert_eq!(
        page["Items"][0]["BackdropImageTags"],
        json!(tags),
        "item page"
    );
}

/// Sends one request as `token` and returns status + raw body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let ident = r#"Client="test", Device="d", DeviceId="image-order", Version="1""#;
    let req = Request::builder().method(method).uri(uri).header(
        header::AUTHORIZATION,
        match token {
            Some(t) => format!(r#"MediaBrowser Token="{t}", {ident}"#),
            None => format!("MediaBrowser {ident}"),
        },
    );
    let req = match body {
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&v).expect("body")))
            .expect("request"),
        None => req.body(Body::empty()).expect("request"),
    };
    let res = router.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, bytes)
}

/// The JSON body of a `200`.
fn json_of((status, bytes): (StatusCode, Bytes)) -> Value {
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("json")
}

/// Percent-encodes a path for a query string.
fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}
