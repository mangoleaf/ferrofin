//! The library tree's synthetic top and the `Year` by-name items, end to end.
//!
//! Boots the real composition root ([`ferrofin_server::state::build_app_state`])
//! over a temp data dir + database, creates a movies library through
//! `POST /Library/VirtualFolders`, scans it through `POST /Library/Refresh`,
//! and then proves over real HTTP (`tower::oneshot` against the real router):
//!
//! - `GET /Items/Root` is `200` and is the `UserRootFolder` (`Media Folders`)
//!   — Jellyfin creates it lazily (`GetUserRootFolder()`), so it must resolve
//!   on a database that never had one.
//! - `GET /Items/{movie}/Ancestors` climbs past the library's
//!   `CollectionFolder` to that root, with and without a user in scope.
//! - The scan materializes one `Year` row per distinct `ProductionYear`,
//!   with the path-derived ids Jellyfin's `GetItemByNameId<Year>` produces,
//!   so `GET /Years` lists them and `GET /Years/{year}` returns one.
//! - `GET /Years/{year}` for a year no item carries still resolves
//!   (`GetYear` always creates), exactly as upstream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_core::item_repository::FerrofinItemRepository;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_db::Database;
use ferrofin_model::data::BaseItemKind;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "root-and-years-pw";

const CLIENT_AUTH: &str =
    r#"MediaBrowser Client="Root Years", Device="Test Rig", DeviceId="rig-ry", Version="1.0.0""#;

/// A booted server plus what the test drives it with.
struct Harness {
    wired: WiredApp,
    db: Database,
    /// The movies library's media directory.
    media_dir: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the admin.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-root-years".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    // Two movies with a release year in their folder/file names — the naming
    // rules the scan applies set `ProductionYear` from them.
    let media_dir = temp.path().join("media");
    for title in ["Film Alpha (1999)", "Film Beta (2004)"] {
        let dir = media_dir.join(title);
        std::fs::create_dir_all(&dir).expect("movie dir");
        std::fs::write(dir.join(format!("{title}.mkv")), [0u8; 1024]).expect("movie file");
    }

    std::fs::create_dir_all(&config.data_dir).expect("data dir");
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        filters: Vec::new(),
        encoders: Vec::new(),
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

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).expect("valid JSON body")
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
    (status, body_json(response).await)
}

/// Authenticates the seeded admin; returns the token-bearing auth header and
/// the user id.
async fn authenticate(router: &axum::Router) -> (String, String) {
    let auth = router
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
    assert_eq!(auth.status(), StatusCode::OK, "admin authenticates");
    let auth = body_json(auth).await;
    let token = auth["AccessToken"].as_str().expect("token").to_owned();
    let user_id = auth["User"]["Id"].as_str().expect("user id").to_owned();
    (
        format!(
            r#"MediaBrowser Client="Root Years", Device="Test Rig", DeviceId="rig-ry", Version="1.0.0", Token="{token}""#
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

// One client flow, asserted step by step — splitting it would re-boot and
// re-scan the server per step for no extra coverage.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn items_root_ancestors_and_years_over_real_http() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (auth_header, user_id) = authenticate(&router).await;

    // ---- Items/Root resolves before any library exists (lazy creation) ----
    let (status, root) = send(&router, &auth_header, "GET", "/Items/Root").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Items/Root is 200 on a fresh install: {root}"
    );
    assert_eq!(root["Type"].as_str(), Some("UserRootFolder"));
    assert_eq!(root["Name"].as_str(), Some("Media Folders"));
    assert_eq!(root["IsFolder"].as_bool(), Some(true));
    let root_id = root["Id"].as_str().expect("root id").to_owned();
    // Same id Jellyfin derives: case-sensitive, data-dir-relative `root\default`.
    let expected_root = ferrofin_core::item_type_lookup::user_root_folder_id(
        &ferrofin_core::item_type_lookup::IdDerivation::Jellyfin {
            program_data_path: Some("/config".to_owned()),
        },
        "/config/root/default",
    )
    .expect("derived");
    // `.simple()`: ids go out in Jellyfin's dashless "N" spelling
    // (`JsonGuidConverter`), so compare in that spelling.
    assert!(
        root_id.eq_ignore_ascii_case(&expected_root.simple().to_string()),
        "root id {root_id} is the Jellyfin-derived {expected_root}"
    );

    // ---- a movies library, created + scanned through the real endpoints ----
    let media = urlencoding(&harness.media_dir);
    let (status, body) = send(
        &router,
        &auth_header,
        "POST",
        &format!("/Library/VirtualFolders?name=Movies&collectionType=movies&paths={media}&refreshLibrary=false"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    let (status, folders) = send(&router, &auth_header, "GET", "/Library/VirtualFolders").await;
    assert_eq!(status, StatusCode::OK);
    let library_id = folders[0]["ItemId"]
        .as_str()
        .expect("library ItemId")
        .to_owned();
    let (status, _) = send(&router, &auth_header, "POST", "/Library/Refresh").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "scan queued");

    // The scan runs in the background; wait for both movies to land.
    let movies = wait_for("the two scanned movies", async || {
        let (_, items) = send(
            &router,
            &auth_header,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie&fields=ProductionYear",
        )
        .await;
        let list = items["Items"].as_array()?.clone();
        (list.len() == 2).then_some(list)
    })
    .await;
    let years: Vec<i64> = movies
        .iter()
        .filter_map(|m| m["ProductionYear"].as_i64())
        .collect();
    assert!(
        years.contains(&1999) && years.contains(&2004),
        "years parsed: {years:?}"
    );

    // ---- the scan's year pass materialized the rows (no /Years call yet) ----
    let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
        harness.db.clone(),
        Arc::new(ItemTypeLookup::new()),
    ));
    let year_rows = wait_for("the scan's Year rows", async || {
        let mut rows = items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Year],
                ..InternalItemsQuery::default()
            })
            .await
            .ok()?;
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        (rows.len() == 2).then_some(rows)
    })
    .await;
    let paths = harness.wired.state.config.application_paths();
    let mode = ferrofin_core::item_type_lookup::IdDerivation::Jellyfin {
        program_data_path: Some(paths.program_data_path()),
    };
    for row in &year_rows {
        let (id, name, path) = (&row.id, row.name.as_deref().expect("year name"), &row.path);
        let expected =
            ferrofin_core::item_type_lookup::year_item_id(&mode, &paths.year_path(), name)
                .expect("derived");
        assert!(
            id.eq_ignore_ascii_case(&expected.to_string()),
            "Year {name} has the path-derived id ({id} vs {expected})"
        );
        assert_eq!(
            path.as_deref(),
            Some(format!("{}/{name}", paths.year_path()).as_str())
        );
    }

    // ---- GET /Years lists them; GET /Years/{year} returns one ----
    let (status, years) = send(&router, &auth_header, "GET", "/Years").await;
    assert_eq!(status, StatusCode::OK, "{years}");
    let listed: Vec<&str> = years["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .filter_map(|y| y["Name"].as_str())
        .collect();
    assert_eq!(listed, vec!["1999", "2004"], "{years}");
    assert!(
        years["Items"]
            .as_array()
            .expect("Items")
            .iter()
            .all(|y| y["Type"] == "Year"),
        "{years}"
    );
    let (status, year) = send(&router, &auth_header, "GET", "/Years/1999").await;
    assert_eq!(status, StatusCode::OK, "{year}");
    assert_eq!(year["Name"].as_str(), Some("1999"));
    assert_eq!(year["Type"].as_str(), Some("Year"));
    // The row id is stored hyphenated; the wire spelling is Jellyfin's dashless
    // "N" form, so compare the hex digits.
    let scanned = year_rows[0].id.replace('-', "");
    assert!(
        year["Id"]
            .as_str()
            .is_some_and(|id| id.eq_ignore_ascii_case(&scanned)),
        "/Years/1999 is the scanned row: {year}"
    );
    // A year no item carries is created on demand (`GetYear` always creates).
    let (status, year) = send(&router, &auth_header, "GET", "/Years/1850").await;
    assert_eq!(status, StatusCode::OK, "{year}");
    assert_eq!(year["Name"].as_str(), Some("1850"));
    let (status, _) = send(&router, &auth_header, "GET", "/Years/0").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-positive years are invalid"
    );

    // ---- Ancestors climb library → root, with and without a user ----
    let movie_id = movies[0]["Id"].as_str().expect("movie id");
    for uri in [
        format!("/Items/{movie_id}/Ancestors"),
        format!("/Items/{movie_id}/Ancestors?userId={user_id}"),
    ] {
        let (status, ancestors) = send(&router, &auth_header, "GET", &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {ancestors}");
        let chain: Vec<(&str, &str)> = ancestors
            .as_array()
            .expect("array")
            .iter()
            .map(|a| {
                (
                    a["Type"].as_str().unwrap_or(""),
                    a["Id"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(chain.len(), 2, "{uri}: {ancestors}");
        assert_eq!(chain[0].0, "CollectionFolder", "{uri}: {ancestors}");
        assert!(
            chain[0].1.eq_ignore_ascii_case(&library_id),
            "{uri}: {ancestors}"
        );
        assert_eq!(chain[1].0, "UserRootFolder", "{uri}: {ancestors}");
        assert!(
            chain[1].1.eq_ignore_ascii_case(&root_id),
            "{uri}: {ancestors}"
        );
    }
    // The library's own parent is the root too.
    let (status, ancestors) = send(
        &router,
        &auth_header,
        "GET",
        &format!("/Items/{library_id}/Ancestors"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ancestors.as_array().map(Vec::len), Some(1), "{ancestors}");
    assert_eq!(ancestors[0]["Type"].as_str(), Some("UserRootFolder"));
}

/// Percent-encodes a path for a query-string value.
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
