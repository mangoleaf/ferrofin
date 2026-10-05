//! `DELETE /Items/{itemId}` and `DELETE /Items` end to end: who may delete,
//! as upstream's `LibraryController.DeleteItem`/`DeleteItems` decide it with
//! `item.CanDelete(user)`.
//!
//! Boots the real composition root ([`ferrofin_server::state::build_app_state`])
//! over a temp data dir and two movie libraries of small files, scans them,
//! and drives every step over the real router as three accounts:
//!
//! - `viewer`, a default account (nothing under "Allow media deletion
//!   from"): `401`, and the item stays; its DTO says `CanDelete: false`;
//! - `curator`, allowed to delete from one library only ("Allow media
//!   deletion from" with that library ticked): `204` there, `401` in the
//!   other, and a mixed `DELETE /Items?ids=` stops at the first refused id,
//!   the ones before it already deleted (upstream's per-item loop);
//! - the administrator ("All libraries"): `204`.
//!
//! A delete removes the item from the library database only: the media file
//! stays on disk. Upstream also deletes the files (`DeleteFileLocation =
//! true`) — an open work item, `brain/plans/PLAN_ITEM_FILE_DELETION.md`.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "deletion-admin-pw";

/// A booted server and the temp dir holding its data and the media it
/// scanned.
struct Harness {
    wired: WiredApp,
    temp: tempfile::TempDir,
}

impl Harness {
    fn path(&self, relative: &str) -> PathBuf {
        self.temp.path().join(relative)
    }
}

/// Writes a small file (and its directories).
fn touch(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, contents).expect("write");
}

async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-item-deletion".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    for (path, contents) in [
        ("movies/Alpha (1999)/Alpha (1999).mkv", String::new()),
        ("movies/Epsilon (2003)/Epsilon (2003).mkv", String::new()),
        ("more/Gamma (2001)/Gamma (2001).mkv", String::new()),
        ("more/Delta (2002)/Delta (2002).mkv", String::new()),
    ] {
        touch(&temp.path().join(path), &contents);
    }
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
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
    Harness { wired, temp }
}

/// Sends one request as `token` (on its own device) and returns status + body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let device = token.map_or("anonymous", |t| &t[..8.min(t.len())]);
    let ident = format!(r#"Client="test", Device="d", DeviceId="{device}", Version="1""#);
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Authenticates `name` and returns its token and user id.
async fn login(router: &axum::Router, name: &str, password: &str) -> (String, String) {
    let ident =
        format!(r#"MediaBrowser Client="test", Device="d", DeviceId="login-{name}", Version="1""#);
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, ident)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "Username": name, "Pw": password }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), StatusCode::OK, "{name} authenticates");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: Value = serde_json::from_slice(&bytes).expect("json");
    (
        v["AccessToken"].as_str().expect("token").to_owned(),
        v["User"]["Id"].as_str().expect("id").to_owned(),
    )
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

/// The library items by `Path`, as the administrator sees them.
async fn items_by_path(
    router: &axum::Router,
    token: &str,
) -> std::collections::HashMap<String, String> {
    let (status, items) = call(
        router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie&fields=Path",
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{items}");
    items["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .filter_map(|i| Some((i["Path"].as_str()?.to_owned(), i["Id"].as_str()?.to_owned())))
        .collect()
}

/// Creates an account and returns its id.
async fn create_user(router: &axum::Router, admin: &str, name: &str, password: &str) -> String {
    let (status, created) = call(
        router,
        "POST",
        "/Users/New",
        Some(admin),
        Some(json!({ "Name": name, "Password": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    created["Id"].as_str().expect("id").to_owned()
}

/// Whether `user` sees `CanDelete` on `item` — the field jellyfin-web shows
/// its "Delete" entry by.
async fn dto_can_delete(router: &axum::Router, token: &str, user: &str, item: &str) -> bool {
    let (status, dto) = call(
        router,
        "GET",
        &format!("/Users/{user}/Items/{item}"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{dto}");
    dto["CanDelete"].as_bool().expect("CanDelete is projected")
}

async fn exists(router: &axum::Router, admin: &str, item: &str) -> bool {
    call(router, "GET", &format!("/Items/{item}"), Some(admin), None)
        .await
        .0
        == StatusCode::OK
}

async fn scan(harness: &Harness) {
    assert!(
        harness
            .wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan"),
        "the scan ran to its end"
    );
}

// One client flow, asserted step by step: each step depends on the state
// the previous ones left, and re-booting and re-scanning per step would add
// nothing.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn deleting_items_over_http_asks_can_delete_and_keeps_the_files() {
    let h = boot().await;
    let router = ferrofin_api::create_router(h.wired.state.clone());
    let (admin, _) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;

    for (name, dir) in [("Movies", "movies"), ("More", "more")] {
        let path = encode(&h.path(dir).to_string_lossy());
        let (status, body) = call(
            &router,
            "POST",
            &format!(
                "/Library/VirtualFolders?name={name}&collectionType=movies&paths={path}&refreshLibrary=false"
            ),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{name}: {body}");
    }
    let (_, folders) = call(
        &router,
        "GET",
        "/Library/VirtualFolders",
        Some(&admin),
        None,
    )
    .await;
    let more_id = folders
        .as_array()
        .expect("folders")
        .iter()
        .find(|f| f["Name"] == "More")
        .and_then(|f| f["ItemId"].as_str())
        .expect("More library id")
        .to_owned();
    scan(&h).await;
    let ids = items_by_path(&router, &admin).await;
    let id = |relative: &str| -> String {
        ids.get(&h.path(relative).to_string_lossy().into_owned())
            .unwrap_or_else(|| panic!("{relative} scanned: {ids:?}"))
            .clone()
    };
    let alpha = id("movies/Alpha (1999)/Alpha (1999).mkv");
    let epsilon = id("movies/Epsilon (2003)/Epsilon (2003).mkv");
    let gamma = id("more/Gamma (2001)/Gamma (2001).mkv");
    let delta = id("more/Delta (2002)/Delta (2002).mkv");

    // ---- viewer: no deletion right ------------------------------------------
    let viewer_id = create_user(&router, &admin, "viewer", "viewer-pw").await;
    let (viewer, _) = login(&router, "viewer", "viewer-pw").await;
    assert!(!dto_can_delete(&router, &viewer, &viewer_id, &alpha).await);
    for uri in [format!("/Items/{alpha}"), format!("/Items?ids={alpha}")] {
        let (status, _) = call(&router, "DELETE", &uri, Some(&viewer), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    assert!(exists(&router, &admin, &alpha).await, "the row stays");
    assert!(h.path("movies/Alpha (1999)/Alpha (1999).mkv").is_file());

    // ---- curator: "Allow media deletion from" the More library only ----------
    let curator_id = create_user(&router, &admin, "curator", "curator-pw").await;
    let (status, user) = call(
        &router,
        "GET",
        &format!("/Users/{curator_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut policy = user["Policy"].clone();
    policy["EnableContentDeletion"] = json!(false);
    policy["EnableContentDeletionFromFolders"] = json!([more_id]);
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Users/{curator_id}/Policy"),
        Some(&admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (curator, _) = login(&router, "curator", "curator-pw").await;
    assert!(dto_can_delete(&router, &curator, &curator_id, &gamma).await);
    assert!(!dto_can_delete(&router, &curator, &curator_id, &alpha).await);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Items/{gamma}"),
        Some(&curator),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "allowed in its library");
    assert!(!exists(&router, &admin, &gamma).await, "the row is gone");
    assert!(
        h.path("more/Gamma (2001)/Gamma (2001).mkv").is_file(),
        "the file stays on disk"
    );
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Items/{alpha}"),
        Some(&curator),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "not in another library");
    assert!(exists(&router, &admin, &alpha).await);
    // Per item, in order: Delta goes, Alpha is refused, Epsilon is never reached.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Items?ids={delta},{alpha},{epsilon}"),
        Some(&curator),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !exists(&router, &admin, &delta).await,
        "deleted before the refusal"
    );
    for kept in [&alpha, &epsilon] {
        assert!(exists(&router, &admin, kept).await);
    }

    // ---- administrator: "All libraries" --------------------------------------
    assert!(dto_can_delete(&router, &admin, &admin_id(&router, &admin).await, &alpha).await);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Items/{alpha}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&router, &admin, &alpha).await, "the row is gone");
    assert!(
        h.path("movies/Alpha (1999)/Alpha (1999).mkv").is_file(),
        "the file stays on disk"
    );
}

/// The administrator's own id.
async fn admin_id(router: &axum::Router, admin: &str) -> String {
    let (status, me) = call(router, "GET", "/Users/Me", Some(admin), None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    me["Id"].as_str().expect("id").to_owned()
}
