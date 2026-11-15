//! `GET` and `DELETE /LiveTv/Timers/{timerId}` over the real composition root
//! and a real session WebSocket (gl-843).
//!
//! Upstream's `GetTimer` returns `LiveTvManager.GetTimer` unchecked, so an id
//! it does not list answers `200 null`; `CancelTimer` throws
//! `ResourceNotFoundException` for one, a `404`, and only a cancel that changed
//! the timer's status raises `TimerCancelled`.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const UNKNOWN: &str = "00000000000000000000000000000001";

/// A client side of the session WebSocket.
type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A served server whose administrator holds a socket.
struct Fixture {
    _wired: WiredApp,
    db: Database,
    _temp: tempfile::TempDir,
    router: axum::Router,
    token: String,
    socket: ClientSocket,
}

/// Boots the composition root with a passwordless administrator, signs them in
/// and opens their socket.
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-live-tv-timer-by-id".to_owned(),
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
    let token = login(&router).await;

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
        db,
        _temp: temp,
        router,
        token,
        socket,
    }
}

/// Signs the passwordless administrator in and returns its token.
async fn login(router: &axum::Router) -> String {
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    r#"MediaBrowser Client="test", Device="d", DeviceId="timer-by-id", Version="1""#,
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
    v["AccessToken"].as_str().expect("token").to_owned()
}

impl Fixture {
    /// Sends one request as the administrator and returns status + raw body.
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
        let token = &self.token;
        let req = Request::builder().method(method).uri(uri).header(
            header::AUTHORIZATION,
            format!(
                r#"MediaBrowser Token="{token}", Client="test", Device="d", DeviceId="timer-by-id", Version="1""#
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
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The `Data.Id` of each `TimerCancelled` frame the socket receives, up to
    /// and including the first naming `sentinel` (10 s cap).
    async fn cancelled_until(&mut self, sentinel: &str) -> Vec<String> {
        use futures_util::StreamExt as _;
        let mut ids = Vec::new();
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
                if json["MessageType"] != "TimerCancelled" {
                    continue;
                }
                let id = json["Data"]["Id"].as_str().unwrap_or_default().to_owned();
                let done = id == sentinel;
                ids.push(id);
                if done {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no TimerCancelled naming {sentinel} in time: {ids:?}"));
        ids
    }
}

impl Fixture {
    /// Creates a timer from `body` and returns the id the server minted.
    async fn create(&self, mut body: Value) -> String {
        body["StartDate"] = json!("2099-01-01T00:00:00Z");
        body["EndDate"] = json!("2099-01-01T01:00:00Z");
        let name = body["Name"].as_str().expect("a name").to_owned();
        let (status, text) = self.call("POST", "/LiveTv/Timers", Some(body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
        let (_, list) = self.call("GET", "/LiveTv/Timers", None).await;
        let list: Value = serde_json::from_str(&list).expect("json");
        list["Items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|t| t["Name"] == name.as_str())
            .and_then(|t| t["Id"].as_str())
            .expect("the timer")
            .to_owned()
    }
}

/// An unknown id: `GET` answers `200 null` and `DELETE` a `404` with no push.
/// A Completed timer, which `GetTimers` leaves out, is unknown too. An already
/// cancelled timer of a series is cancelled again with no push. A
/// live timer is found whatever the case of its id, is cancelled with one
/// push, and is then unknown in turn.
#[tokio::test]
async fn an_unknown_timer_is_200_null_to_get_and_404_to_delete_with_no_push() {
    let mut f = fixture().await;

    let uri = format!("/LiveTv/Timers/{UNKNOWN}");
    assert_eq!(
        f.call("GET", &uri, None).await,
        (StatusCode::OK, "null".to_owned())
    );
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A series keeps its cancelled timers (`AddOrUpdate`), so this one stays.
    let cancelled = f
        .create(json!({ "Name": "cancelled", "Status": "Cancelled", "SeriesTimerId": "s" }))
        .await;
    let uri = format!("/LiveTv/Timers/{cancelled}");
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = f.call("GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("json")["Status"],
        "Cancelled"
    );

    // A recording that finished: only the capture marks a timer Completed.
    let done = f.create(json!({ "Name": "done" })).await;
    sqlx::query(
        r#"UPDATE "FerrofinLiveTvTimers" SET "Status" = 'Completed',
           "Data" = json_set("Data", '$.Status', 'Completed') WHERE "Id" = ?1"#,
    )
    .bind(&done)
    .execute(f.db.pool())
    .await
    .expect("complete the timer");
    let uri = format!("/LiveTv/Timers/{done}");
    assert_eq!(
        f.call("GET", &uri, None).await,
        (StatusCode::OK, "null".to_owned())
    );
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let id = f.create(json!({ "Name": "probe" })).await;

    let upper = format!("/LiveTv/Timers/{}", id.to_ascii_uppercase());
    let (status, body) = f.call("GET", &upper, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("json")["Id"],
        id.as_str()
    );
    let (status, body) = f.call("DELETE", &upper, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        f.cancelled_until(&id).await,
        std::slice::from_ref(&id),
        "one push, for the live timer, by its own id: none for the unknown, the \
         Completed or the already cancelled one"
    );

    let uri = format!("/LiveTv/Timers/{id}");
    assert_eq!(
        f.call("GET", &uri, None).await,
        (StatusCode::OK, "null".to_owned())
    );
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}
