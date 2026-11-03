//! The `UserUpdated` push over the real composition root and a real session
//! WebSocket (gl-1179).
//!
//! Upstream's `UserManager.RenameUser` and `UpdatePolicyAsync` publish
//! `UserUpdatedEventArgs`, so every caller's rename or policy save tells the
//! user's sessions, the startup wizard's included, and a configuration save
//! tells nobody. A frame that should not come simply never arrives, so each
//! check ends with a rename to a sentinel name and reads frames up to it.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";

/// A client side of the session WebSocket.
type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A served server whose administrator holds a socket.
struct Fixture {
    _wired: WiredApp,
    _temp: tempfile::TempDir,
    router: axum::Router,
    token: String,
    user_id: String,
    socket: ClientSocket,
}

/// Boots the composition root with a passwordless administrator, so the
/// startup wizard's user step is still open, signs them in and opens their
/// socket.
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-user-updated-push".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: String::new(),
        ..Config::test_stub(temp.path())
    };
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
    let (token, user_id) = login(&router).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let (socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/socket?ApiKey={token}"))
            .await
            .expect("socket upgrades");
    Fixture {
        _wired: wired,
        _temp: temp,
        router,
        token,
        user_id,
        socket,
    }
}

/// Signs the passwordless administrator in and returns its token and id.
async fn login(router: &axum::Router) -> (String, String) {
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    r#"MediaBrowser Client="test", Device="d", DeviceId="user-updated", Version="1""#,
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "Username": ADMIN_USER, "Pw": "" }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), StatusCode::OK, "the administrator signs in");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: Value = serde_json::from_slice(&bytes).expect("json");
    (
        v["AccessToken"].as_str().expect("token").to_owned(),
        v["User"]["Id"].as_str().expect("id").to_owned(),
    )
}

impl Fixture {
    /// Sends one request as the administrator and returns status + body.
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let token = &self.token;
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                header::AUTHORIZATION,
                format!(
                    r#"MediaBrowser Token="{token}", Client="test", Device="d", DeviceId="user-updated", Version="1""#
                ),
            );
        let req = match body {
            Some(v) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&v).expect("body")))
                .expect("request"),
            None => req.body(Body::empty()).expect("request"),
        };
        let res = self.router.clone().oneshot(req).await.expect("response");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Renames the administrator through `POST /Users`.
    async fn rename(&self, name: &str) {
        let uri = format!("/Users?userId={}", self.user_id);
        let (status, body) = self
            .call("POST", &uri, Some(json!({ "Name": name, "Policy": null })))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// The user names of the `UserUpdated` frames the socket receives, up to
    /// and including the first that names `sentinel` (10 s cap).
    async fn pushes_until(&mut self, sentinel: &str) -> Vec<String> {
        use futures_util::StreamExt as _;
        let mut names = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = self
                    .socket
                    .next()
                    .await
                    .expect("a frame")
                    .expect("a readable frame");
                let Ok(text) = frame.to_text() else { continue };
                let Ok(json) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                if json["MessageType"] != "UserUpdated" {
                    continue;
                }
                let name = json["Data"]["Name"].as_str().unwrap_or_default().to_owned();
                let done = name == sentinel;
                names.push(name);
                if done {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no UserUpdated naming {sentinel} in time: {names:?}"));
        names
    }
}

/// `StartupController.UpdateStartupUser` renames through `RenameUser`, which
/// publishes `UserUpdatedEventArgs`: the wizard's rename reaches the user's
/// sessions like any other.
#[tokio::test]
async fn the_startup_wizards_rename_pushes_user_updated() {
    let mut f = fixture().await;
    let (status, body) = f
        .call(
            "POST",
            "/Startup/User",
            Some(json!({ "Name": "wizard-renamed", "Password": "wizard-pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(f.pushes_until("wizard-renamed").await, ["wizard-renamed"]);
}

/// Each rename and each policy save pushes exactly once, whoever calls the
/// user manager. A configuration save (gl-864) and a `POST /Users` that keeps
/// the name, which `UpdateUser` does not rename, push nothing.
#[tokio::test]
async fn a_rename_and_a_policy_save_push_once_and_a_configuration_save_none() {
    let mut f = fixture().await;
    f.rename("renamed").await;
    f.rename("renamed").await;

    let (_, user) = f.call("GET", &format!("/Users/{}", f.user_id), None).await;
    let uri = format!("/Users/{}/Policy", f.user_id);
    let (status, body) = f.call("POST", &uri, Some(user["Policy"].clone())).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let uri = format!("/Users/Configuration?userId={}", f.user_id);
    let (status, body) = f
        .call("POST", &uri, Some(user["Configuration"].clone()))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    f.rename("sentinel").await;
    assert_eq!(
        f.pushes_until("sentinel").await,
        ["renamed", "renamed", "sentinel"],
        "the rename, nothing for the kept name, the policy save, nothing for the \
         configuration, the sentinel"
    );
}
