//! Elevation policy — the admin-only surface, end to end.
//!
//! Boots the real composition root over a fresh temp database, seeds the
//! administrator, has the admin create an *ordinary* account, and then drives
//! the elevation-gated routes as that ordinary account.
//!
//! This exists because Ferrofin shipped with no elevation gate at all. Several
//! controllers carried a comment saying the policy was "applied at the
//! composition root's auth layer" — a layer that was never built — so every
//! route upstream marks `[Authorize(Policy = Policies.RequiresElevation)]` was
//! reachable by any authenticated account. Two consequences were confirmed
//! against a running server:
//!
//! - `POST /Users/{ownId}/Policy` with `IsAdministrator: true` returned `204`,
//!   and the caller was an administrator from the next request onward.
//! - `GET /Devices` returned every device row, each carrying a plaintext
//!   `AccessToken` — the administrator's live token among them.
//!
//! A unit test cannot catch a regression here: the gate lives in the route
//! wiring, so it has to be exercised through the real router with a real
//! non-administrator token. Every assertion below is `403` for the ordinary
//! account and a success status for the administrator, so the test fails both
//! if the gate is removed and if it is applied too widely.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

/// The seeded administrator's credentials.
const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "elevation-pw";
/// The ordinary (non-administrator) account the admin creates.
const USER_NAME: &str = "ordinary";
const USER_PASSWORD: &str = "ordinary-pw";

/// A booted server plus the temp dir keeping its database alive.
struct Harness {
    wired: WiredApp,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the administrator.
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    // `database_path()` lives under `data/`; SQLite will not create the parent.
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-elevation".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };

    let db = ferrofin_db::Database::connect(&config.database_url())
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
    let wired = build_app_state(&db, &config, &ffmpeg, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");

    Harness { wired, _temp: temp }
}

/// Sends one request, optionally bearing `token`, and returns status + body.
///
/// `device` scopes the `DeviceId`: two callers sharing one `DeviceId` share a
/// session, so the administrator and the ordinary account must not reuse it.
async fn call_as(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    device: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    // Every authenticated route needs the client identity; `AuthenticateByName`
    // needs it too, with no `Token=` yet, or it is a `400`.
    let ident = format!(r#"Client="test", Device="d", DeviceId="{device}", Version="1""#);
    req = req.header(
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
        .expect("body")
        .to_vec();
    (status, bytes)
}

/// Authenticates `name`/`password` on its own device and returns the token.
async fn login(router: &axum::Router, name: &str, password: &str, device: &str) -> String {
    let (status, body) = call_as(
        router,
        "POST",
        "/Users/AuthenticateByName",
        None,
        device,
        Some(serde_json::json!({ "Username": name, "Pw": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{name} must authenticate");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("auth json");
    v["AccessToken"]
        .as_str()
        .expect("AccessToken present")
        .to_owned()
}

#[tokio::test]
async fn an_ordinary_account_cannot_reach_the_elevated_surface() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());

    let admin_token = login(&router, ADMIN_USER, ADMIN_PASSWORD, "elev-admin").await;

    // The administrator creates an ordinary account — itself an elevated route.
    let (status, body) = call_as(
        &router,
        "POST",
        "/Users/New",
        Some(&admin_token),
        "elev-admin",
        Some(serde_json::json!({ "Name": USER_NAME, "Password": USER_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin may create a user");
    let created: serde_json::Value = serde_json::from_slice(&body).expect("user json");
    let user_id = created["Id"].as_str().expect("new user id").to_owned();

    let user_token = login(&router, USER_NAME, USER_PASSWORD, "elev-user").await;

    // Sanity: the new account really is not an administrator, so the 403s below
    // are the gate rejecting a non-admin and not some unrelated failure.
    let (status, body) = call_as(
        &router,
        "GET",
        "/Users/Me",
        Some(&user_token),
        "elev-user",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let me: serde_json::Value = serde_json::from_slice(&body).expect("me json");
    assert_eq!(
        me["Policy"]["IsAdministrator"],
        serde_json::Value::Bool(false),
        "the created account must be an ordinary user"
    );

    // Self-promotion: the single worst case. This returned 204 before the gate,
    // and the caller was an administrator from the next request onward.
    let (status, _) = call_as(
        &router,
        "POST",
        &format!("/Users/{user_id}/Policy"),
        Some(&user_token),
        "elev-user",
        Some(serde_json::json!({ "IsAdministrator": true })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an ordinary user must not rewrite a policy — this is privilege escalation"
    );

    // And it must not have taken effect.
    let (_, body) = call_as(
        &router,
        "GET",
        "/Users/Me",
        Some(&user_token),
        "elev-user",
        None,
    )
    .await;
    let me: serde_json::Value = serde_json::from_slice(&body).expect("me json");
    assert_eq!(
        me["Policy"]["IsAdministrator"],
        serde_json::Value::Bool(false),
        "the rejected policy write must not have been applied"
    );

    // Device rows carry plaintext access tokens, including the admin's.
    let (status, _) = call_as(
        &router,
        "GET",
        "/Devices",
        Some(&user_token),
        "elev-user",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "device rows expose plaintext access tokens — never to a non-admin"
    );

    let (status, _) = call_as(
        &router,
        "POST",
        "/Users/New",
        Some(&user_token),
        "elev-user",
        Some(serde_json::json!({ "Name": "interloper", "Password": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "only an admin creates users");

    let (status, _) = call_as(
        &router,
        "DELETE",
        &format!("/Users/{user_id}"),
        Some(&user_token),
        "elev-user",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "only an admin deletes users");
}

/// The gate must not be so wide that it locks the administrator out — a test
/// that only asserted `403` would pass with every route permanently denied.
#[tokio::test]
async fn the_administrator_still_reaches_the_elevated_surface() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin_token = login(&router, ADMIN_USER, ADMIN_PASSWORD, "elev-admin").await;

    let (status, _) = call_as(
        &router,
        "GET",
        "/Devices",
        Some(&admin_token),
        "elev-admin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the admin may list devices");

    let (status, body) = call_as(
        &router,
        "POST",
        "/Users/New",
        Some(&admin_token),
        "elev-admin",
        Some(serde_json::json!({ "Name": "second-admin", "Password": "pw-123456" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let created: serde_json::Value = serde_json::from_slice(&body).expect("user json");
    let id = created["Id"].as_str().expect("id");

    let (status, _) = call_as(
        &router,
        "POST",
        &format!("/Users/{id}/Policy"),
        Some(&admin_token),
        "elev-admin",
        Some(serde_json::json!({ "IsAdministrator": true })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the admin may grant administrator"
    );

    let (status, _) = call_as(
        &router,
        "DELETE",
        &format!("/Users/{id}"),
        Some(&admin_token),
        "elev-admin",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the admin may delete a user"
    );
}

/// An anonymous caller must be rejected as unauthenticated (`401`), not as
/// unelevated (`403`) — the extractor authenticates before it checks policy.
#[tokio::test]
async fn an_anonymous_caller_is_unauthorized_not_forbidden() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (status, _) = call_as(&router, "GET", "/Devices", None, "elev-anon", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
