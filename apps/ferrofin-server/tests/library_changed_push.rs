//! The per-user `LibraryChanged` push over the real composition root and a
//! real session WebSocket: what each signed-in user is told after a change.
//!
//! An administrator and a viewer who may open only the "Allowed" library hold
//! sockets on a loopback server. `LibraryUpdateDuration` is 0, so each batch is
//! pushed as soon as it is recorded. A user who is not told about a change
//! simply gets no frame for it, so every check makes a later change both users
//! may see (a sentinel) and reads frames up to it.

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
const ADMIN_PASSWORD: &str = "push-admin-pw";

/// A client side of the session WebSocket.
type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Writes a small file (and its directories).
fn touch(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, contents).expect("write");
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

/// Sends one request as `token` and returns status + body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let device = &token[..8.min(token.len())];
    let auth = format!(
        r#"MediaBrowser Token="{token}", Client="test", Device="d", DeviceId="{device}", Version="1""#
    );
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth);
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

/// Opens the session socket as `token`.
async fn open_socket(addr: std::net::SocketAddr, token: &str) -> ClientSocket {
    let (socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/socket?ApiKey={token}"))
            .await
            .expect("socket upgrades");
    socket
}

/// The `LibraryChanged` payloads `socket` receives up to and including the
/// first that names `sentinel` in any of its lists (10 s cap).
async fn pushes_until(socket: &mut ClientSocket, sentinel: &str) -> Vec<Value> {
    use futures_util::StreamExt as _;
    let sentinel = sentinel.replace('-', "").to_lowercase();
    let mut pushes = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let frame = socket
                .next()
                .await
                .expect("a frame")
                .expect("a readable frame");
            let Ok(text) = frame.to_text() else { continue };
            let Ok(json) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if json["MessageType"] != "LibraryChanged" {
                continue;
            }
            let data = json["Data"].clone();
            let done = names(&data, &sentinel);
            pushes.push(data);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no LibraryChanged naming {sentinel} in time: {pushes:?}"));
    pushes
}

/// Whether any list of a `LibraryUpdateInfo` names `id` (`N` format).
fn names(info: &Value, id: &str) -> bool {
    let id = id.replace('-', "").to_lowercase();
    [
        "ItemsAdded",
        "ItemsUpdated",
        "ItemsRemoved",
        "FoldersAddedTo",
        "FoldersRemovedFrom",
    ]
    .iter()
    .any(|list| {
        info[list]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|v| v.as_str() == Some(id.as_str())))
    })
}

/// A booted, served server with two libraries and two signed-in users.
struct Fixture {
    _wired: WiredApp,
    _temp: tempfile::TempDir,
    router: axum::Router,
    admin: String,
    admin_id: String,
    admin_socket: ClientSocket,
    viewer_socket: ClientSocket,
    /// A movie in the library both users can open.
    allowed: String,
}

impl Fixture {
    /// Renames the shared movie, a change both users are told about.
    async fn touch_sentinel(&self) {
        let uri = format!("/Items/{}", self.allowed);
        let (status, mut item) = call(&self.router, "GET", &uri, &self.admin, None).await;
        assert_eq!(status, StatusCode::OK, "{item}");
        let name = format!("{} again", item["Name"].as_str().unwrap_or_default());
        item["Name"] = json!(name);
        let (status, body) = call(&self.router, "POST", &uri, &self.admin, Some(item)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
}

#[allow(clippy::too_many_lines)] // boot, two libraries, a restricted user, two sockets
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-library-changed-push".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    for path in [
        "movies/Alpha (1999)/Alpha (1999).mkv",
        "more/Gamma (2001)/Gamma (2001).mkv",
    ] {
        touch(&temp.path().join(path), "");
    }
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
    let (admin, admin_id) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;

    // Every batch goes out as soon as it is recorded.
    let (_, mut configuration) = call(&router, "GET", "/System/Configuration", &admin, None).await;
    configuration["LibraryUpdateDuration"] = json!(0);
    let (status, body) = call(
        &router,
        "POST",
        "/System/Configuration",
        &admin,
        Some(configuration),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    for (name, dir) in [("Allowed", "movies"), ("Hidden", "more")] {
        let path = encode(&temp.path().join(dir).to_string_lossy());
        let (status, _) = call(
            &router,
            "POST",
            &format!(
                "/Library/VirtualFolders?name={name}&collectionType=movies&paths={path}&refreshLibrary=false"
            ),
            &admin,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan")
    );
    let (_, folders) = call(&router, "GET", "/Library/VirtualFolders", &admin, None).await;
    let allowed_library = folders
        .as_array()
        .expect("folders")
        .iter()
        .find(|f| f["Name"] == "Allowed")
        .expect("Allowed")["ItemId"]
        .as_str()
        .expect("id")
        .to_owned();
    let (_, items) = call(
        &router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie&fields=Path",
        &admin,
        None,
    )
    .await;
    let allowed = items["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .find(|i| {
            i["Path"]
                .as_str()
                .is_some_and(|p| p.ends_with("Alpha (1999).mkv"))
        })
        .unwrap_or_else(|| panic!("Alpha in {items}"))["Id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, created) = call(
        &router,
        "POST",
        "/Users/New",
        &admin,
        Some(json!({ "Name": "viewer", "Password": "viewer-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let viewer_id = created["Id"].as_str().expect("id").to_owned();
    let (_, user) = call(&router, "GET", &format!("/Users/{viewer_id}"), &admin, None).await;
    let mut policy = user["Policy"].clone();
    policy["EnableAllFolders"] = json!(false);
    policy["EnabledFolders"] = json!([allowed_library]);
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        &admin,
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (viewer, _) = login(&router, "viewer", "viewer-pw").await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let admin_socket = open_socket(addr, &admin).await;
    let viewer_socket = open_socket(addr, &viewer).await;
    Fixture {
        _wired: wired,
        _temp: temp,
        router,
        admin,
        admin_id,
        admin_socket,
        viewer_socket,
        allowed,
    }
}

/// gl-580: a private playlist lives under the playlists folder every user can
/// see, but `IsVisibleStandalone` admits only its owner and sharees, so only
/// they are told it changed.
#[tokio::test]
async fn another_users_private_playlist_is_never_announced_to_a_viewer() {
    let mut f = fixture().await;
    let (status, created) = call(
        &f.router,
        "POST",
        "/Playlists",
        &f.admin,
        Some(json!({
            "Name": "Private",
            "Ids": [f.allowed],
            "UserId": f.admin_id,
            "IsPublic": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let playlist = created["Id"].as_str().expect("playlist id").to_owned();
    // Its owner hears of it.
    pushes_until(&mut f.admin_socket, &playlist).await;

    f.touch_sentinel().await;
    let pushes = pushes_until(&mut f.viewer_socket, &f.allowed).await;
    assert!(
        pushes.iter().all(|info| !names(info, &playlist)),
        "the viewer was told about the admin's private playlist: {pushes:?}"
    );
}
