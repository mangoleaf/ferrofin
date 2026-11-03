//! What a refused login answers, over the real composition root.
//!
//! Upstream's `ExceptionMiddleware` writes `text/plain` "Error processing
//! request." for every exception outside Development, so its login routes give
//! a caller the status and nothing else. These tests pin that for each way a
//! login is refused, and that the status still separates the two cases clients
//! tell apart: `401` for bad credentials, `403` for an account that may not
//! sign in. They also pin the routes beside the login routes, which keep their
//! own bodies.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "login-failures-pw";
const VIEWER_PASSWORD: &str = "viewer-pw";
/// The account whose name must not come back from a refused login.
const DISABLED_USER: &str = "Disabled-Dana";
const DISABLED_PASSWORD: &str = "dana-pw";
/// What a client names itself with, beside its token.
const IDENTITY: &str = r#"Client="test", Device="d", DeviceId="login-failures", Version="1""#;
/// Written out rather than taken from the server, so a change to the body
/// fails here.
const REFUSAL_BODY: &str = "Error processing request.";

/// A booted composition root and the temp dir keeping its database alive.
struct Harness {
    wired: WiredApp,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the administrator.
async fn boot() -> Harness {
    boot_with(|config| config).await
}

/// [`boot`] with the settings `configure` changes.
async fn boot_with(configure: impl FnOnce(Config) -> Config) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    for dir in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(dir)).expect("dir");
    }
    let config = configure(Config {
        server_name: "ferrofin-login-failures".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    });
    let db = ferrofin_db::Database::connect(&config.database_url())
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
    Harness { wired, _temp: temp }
}

/// One answer, read whole.
struct Answer {
    status: StatusCode,
    content_type: Option<String>,
    body: String,
}

impl Answer {
    /// The body as JSON, for the routes that keep their JSON errors.
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// Sends a request as the client `token` (if any) belongs to.
async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let authorization = token.map_or_else(
        || format!("MediaBrowser {IDENTITY}"),
        |token| format!(r#"MediaBrowser Token="{token}", {IDENTITY}"#),
    );
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, authorization);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().expect("content type").to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    Answer {
        status,
        content_type,
        body: String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    }
}

async fn login(router: &axum::Router, username: &str, password: &str) -> Answer {
    send(
        router,
        "POST",
        "/Users/AuthenticateByName",
        None,
        Some(json!({ "Username": username, "Pw": password })),
    )
    .await
}

/// Creates a user and returns its id.
async fn create_user(router: &axum::Router, token: &str, name: &str, password: &str) -> String {
    let created = send(
        router,
        "POST",
        "/Users/New",
        Some(token),
        Some(json!({ "Name": name, "Password": password })),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    created.json()["Id"].as_str().expect("id").to_owned()
}

/// Sets `field` of the user's policy as an administrator would.
async fn set_policy(router: &axum::Router, token: &str, user_id: &str, field: &str, value: Value) {
    let uri = format!("/Users/{user_id}");
    let mut policy = send(router, "GET", &uri, Some(token), None).await.json()["Policy"].take();
    policy[field] = value;
    let saved = send(
        router,
        "POST",
        &format!("/Users/{user_id}/Policy"),
        Some(token),
        Some(policy),
    )
    .await;
    assert_eq!(saved.status, StatusCode::NO_CONTENT, "{}", saved.body);
}

/// The answer upstream gives: the status, `text/plain` and the fixed body.
fn assert_refused(answer: &Answer, status: StatusCode, what: &str) {
    assert_eq!(answer.status, status, "{what}: {}", answer.body);
    assert_eq!(answer.body, REFUSAL_BODY, "{what}");
    assert_eq!(answer.content_type.as_deref(), Some("text/plain"), "{what}");
}

#[tokio::test]
async fn refused_logins_answer_a_status_and_upstreams_plain_body() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let token = admin.json()["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned();
    let viewer = create_user(&router, &token, "viewer", VIEWER_PASSWORD).await;
    let disabled = create_user(&router, &token, DISABLED_USER, DISABLED_PASSWORD).await;
    set_policy(&router, &token, &disabled, "IsDisabled", json!(true)).await;

    let unknown = login(&router, "nobody-here", "guess").await;
    let wrong = login(&router, "viewer", "guess").await;
    assert_refused(&unknown, StatusCode::UNAUTHORIZED, "unknown user");
    assert_refused(&wrong, StatusCode::UNAUTHORIZED, "wrong password");

    // A disabled account answers 403 whether or not the password was right, as
    // upstream's does, and never names the account.
    for (what, password) in [("right", DISABLED_PASSWORD), ("wrong", "guess")] {
        let answer = login(&router, DISABLED_USER, password).await;
        assert_refused(&answer, StatusCode::FORBIDDEN, what);
        assert!(!answer.body.contains(DISABLED_USER), "{what}");
    }

    // The obsolete by-id route shares the login path.
    let by_id = |id: &str, pw: &str| format!("/Users/{id}/Authenticate?pw={pw}");
    let wrong_by_id = send(&router, "POST", &by_id(&viewer, "guess"), None, None).await;
    assert_refused(&wrong_by_id, StatusCode::UNAUTHORIZED, "by id, wrong");
    let disabled_by_id = send(
        &router,
        "POST",
        &by_id(&disabled, DISABLED_PASSWORD),
        None,
        None,
    )
    .await;
    assert_refused(&disabled_by_id, StatusCode::FORBIDDEN, "by id, disabled");

    // A refusal that comes after the password was accepted looks the same.
    let bound = create_user(&router, &token, "device-bound", "bound-pw").await;
    set_policy(&router, &token, &bound, "EnableAllDevices", json!(false)).await;
    let device = login(&router, "device-bound", "bound-pw").await;
    assert_refused(&device, StatusCode::FORBIDDEN, "device not allowed");

    let capped = create_user(&router, &token, "capped", "capped-pw").await;
    set_policy(&router, &token, &capped, "MaxActiveSessions", json!(1)).await;
    let first = login(&router, "capped", "capped-pw").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let second = login(&router, "capped", "capped-pw").await;
    assert_refused(&second, StatusCode::FORBIDDEN, "session limit");

    // A login that succeeds is untouched.
    let signed_in = login(&router, "viewer", VIEWER_PASSWORD).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    assert!(signed_in.json()["AccessToken"].is_string());
}

/// Quick Connect's exchange is a login too: with the feature off it answers
/// the same `401` body. `/QuickConnect/Initiate`, refused for the same reason,
/// is not a login and keeps its JSON.
#[tokio::test]
async fn quick_connect_exchange_is_refused_like_a_login() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let token = admin.json()["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned();
    let mut config = send(&router, "GET", "/System/Configuration", Some(&token), None)
        .await
        .json();
    config["QuickConnectAvailable"] = json!(false);
    let saved = send(
        &router,
        "POST",
        "/System/Configuration",
        Some(&token),
        Some(config),
    )
    .await;
    assert_eq!(saved.status, StatusCode::NO_CONTENT, "{}", saved.body);

    let exchange = send(
        &router,
        "POST",
        "/Users/AuthenticateWithQuickConnect",
        None,
        Some(json!({ "Secret": "no-such-secret" })),
    )
    .await;
    assert_refused(&exchange, StatusCode::UNAUTHORIZED, "quick connect off");

    let initiate = send(&router, "POST", "/QuickConnect/Initiate", None, None).await;
    assert_eq!(initiate.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        initiate.json()["error"],
        "unauthorized: quick connect is not active on this server"
    );
}

/// The routes beside the login routes keep the error bodies they had: only
/// `401` and `403` from a login change.
#[tokio::test]
async fn neighbouring_failures_keep_their_bodies() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let token = admin.json()["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned();
    create_user(&router, &token, "viewer", VIEWER_PASSWORD).await;
    let viewer = login(&router, "viewer", VIEWER_PASSWORD).await;
    let viewer_token = viewer.json()["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned();

    let empty_username = login(&router, "", "guess").await;
    assert_eq!(empty_username.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        empty_username.json()["error"],
        "invalid input: username must not be empty"
    );

    let unknown_id = send(
        &router,
        "POST",
        "/Users/00000000-0000-0000-0000-000000000123/Authenticate?pw=x",
        None,
        None,
    )
    .await;
    assert_eq!(unknown_id.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown_id.json()["error"], "not found: User not found");

    let no_token = send(&router, "GET", "/Users/Me", None, None).await;
    assert_eq!(no_token.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        no_token.json()["error"],
        "unauthorized: Request does not contain valid authentication credentials."
    );

    let wrong_current = send(
        &router,
        "POST",
        "/Users/Password",
        Some(&viewer_token),
        Some(json!({ "CurrentPw": "guess", "NewPw": "new" })),
    )
    .await;
    assert_eq!(wrong_current.status, StatusCode::FORBIDDEN);
    assert_eq!(
        wrong_current.json()["error"],
        "forbidden: invalid user or password entered"
    );
}

/// A login by name from the client at `address`.
async fn login_from(
    router: &axum::Router,
    address: [u8; 4],
    username: &str,
    password: &str,
) -> StatusCode {
    let peer = std::net::SocketAddr::from((address, 40_000));
    let request = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(header::AUTHORIZATION, format!("MediaBrowser {IDENTITY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .extension(axum::extract::ConnectInfo(peer))
        .body(Body::from(
            json!({ "Username": username, "Pw": password }).to_string(),
        ))
        .expect("request");
    router
        .clone()
        .oneshot(request)
        .await
        .expect("response")
        .status()
}

/// Ferrofin's login backoff (Jellyfin has none), through the composition
/// root and real client addresses. Past the allowed failures an account's and
/// an address's next logins wait. An address whose attempt would wait past
/// the maximum is refused as a wrong password, without its password being
/// checked; an account only waits, and is never disabled.
///
/// The account has no password, so a check costs no hashing and every wait
/// measured is the backoff's.
#[tokio::test]
async fn repeated_failures_make_logins_wait_and_never_lock_the_account() {
    use std::time::{Duration, Instant};

    let harness = boot_with(|config| Config {
        login_backoff: true,
        login_backoff_attempts: 3,
        login_backoff_max_delay_secs: 5,
        ..config
    })
    .await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let token = admin.json()["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned();
    let user_id = create_user(&router, &token, "no-password", "").await;
    let guesser = [192, 0, 2, 1];
    let other = [198, 51, 100, 7];

    // Each wait is measured from a request sent before the one that waits,
    // so a slow host can only lengthen what the assertions see.
    let mut third_started = Instant::now();
    for n in 1..=3 {
        third_started = Instant::now();
        let status = login_from(&router, guesser, "no-password", "guess").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "failure {n}");
    }
    // The fourth runs no earlier than 1 s after the third was let in.
    let fourth_started = Instant::now();
    let status = login_from(&router, guesser, "no-password", "guess").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        third_started.elapsed() >= Duration::from_secs(1),
        "the fourth did not wait"
    );

    // The account waits from any address (2 s after the fourth), and is not
    // locked: its right password signs in once the wait is over, which
    // clears it.
    let status = login_from(&router, other, "no-password", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        fourth_started.elapsed() >= Duration::from_secs(2),
        "the account did not wait"
    );
    let user = send(
        &router,
        "GET",
        &format!("/Users/{user_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(user.json()["Policy"]["IsDisabled"], json!(false));

    // The guesser's address still counts its failures: one more try at
    // another name makes its next attempt wait 4 s. Of four at once, one is
    // let wait and three would wait past the 5 s maximum, so they are refused
    // at once.
    let status = login_from(&router, guesser, "someone-else", "guess").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let mut burst = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let router = router.clone();
        burst.spawn(async move { login_from(&router, guesser, "someone-else", "guess").await });
    }
    for _ in 0..3 {
        let refused = tokio::time::timeout(Duration::from_secs(2), burst.join_next())
            .await
            .expect("refused at once")
            .expect("a task")
            .expect("joined");
        assert_eq!(refused, StatusCode::UNAUTHORIZED);
    }
    // Now even the right password is refused from the guesser's address,
    // though the account itself was cleared.
    let status = login_from(&router, guesser, "no-password", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the address is paced");
    burst.abort_all();
}
