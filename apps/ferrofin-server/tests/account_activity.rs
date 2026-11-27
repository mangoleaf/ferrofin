//! The dashboard's account activity entries, end to end.
//!
//! Jellyfin writes "Password has been changed for user …" from a consumer of
//! the `UserPasswordChangedEventArgs` that every `UserManager.ChangePassword`
//! publishes, so each route that sets a password writes exactly one, the ones
//! that set it at creation included.
//!
//! Boots the real composition root over a temp data dir and drives the routes
//! over the real router (`tower::oneshot`), reading the entries back from
//! `GET /System/ActivityLog/Entries`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "account-activity-pw";

const CLIENT_AUTH: &str =
    r#"MediaBrowser Client="Account Log", Device="Test Rig", DeviceId="rig-aa", Version="2.5.0""#;

/// A booted server; the temp dir lives as long as it does.
struct Harness {
    wired: WiredApp,
    _temp: tempfile::TempDir,
}

/// Boots the composition root over a temp DB and seeds the admin with
/// `admin_password` (empty: the passwordless admin the wizard finishes).
async fn boot(admin_password: &str) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-account-activity".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: admin_password.to_owned(),
        ..Config::test_stub(temp.path())
    };
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
    Harness { wired, _temp: temp }
}

/// Sends `method uri` with `auth_header` and an optional JSON body; returns
/// the status and the JSON body (`Null` when empty).
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
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned().into())
    };
    (status, json)
}

/// Signs `name` in with `password`; returns the token-bearing auth header.
async fn authenticate(router: &axum::Router, name: &str, password: &str) -> String {
    let (status, auth) = send(
        router,
        CLIENT_AUTH,
        "POST",
        "/Users/AuthenticateByName",
        Some(serde_json::json!({ "Username": name, "Pw": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{name} authenticates: {auth}");
    let token = auth["AccessToken"].as_str().expect("token");
    format!("{CLIENT_AUTH}, Token=\"{token}\"")
}

/// Every activity entry, oldest first, as `(Type, Name, ShortOverview)`.
async fn entries(
    router: &axum::Router,
    auth_header: &str,
) -> Vec<(String, String, Option<String>)> {
    let (status, page) = send(
        router,
        auth_header,
        "GET",
        "/System/ActivityLog/Entries?limit=200",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let mut items = page["Items"].as_array().expect("items").clone();
    items.sort_by_key(|entry| entry["Id"].as_i64());
    items
        .iter()
        .map(|entry| {
            (
                entry["Type"].as_str().unwrap_or_default().to_owned(),
                entry["Name"].as_str().unwrap_or_default().to_owned(),
                entry["ShortOverview"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// The `(Type, Name)` of the entries written since `before` of them were listed.
async fn written_since(
    router: &axum::Router,
    auth_header: &str,
    before: usize,
) -> Vec<(String, String)> {
    entries(router, auth_header)
        .await
        .into_iter()
        .skip(before)
        .map(|(type_, name, _)| (type_, name))
        .collect()
}

fn entry(type_: &str, name: &str) -> (String, String) {
    (type_.to_owned(), name.to_owned())
}

/// `CreateUserByName` creates the user (`UserCreated`) and then sets the
/// password through `ChangePassword` (`UserPasswordChanged`), in that order;
/// a request without `Password` sets none.
#[tokio::test]
async fn a_user_created_with_a_password_logs_the_creation_then_the_password() {
    let harness = boot(ADMIN_PASSWORD).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let auth = authenticate(&router, ADMIN_USER, ADMIN_PASSWORD).await;

    let before = entries(&router, &auth).await.len();
    let (status, body) = send(
        &router,
        &auth,
        "POST",
        "/Users/New",
        Some(serde_json::json!({ "Name": "carol", "Password": "carol-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        written_since(&router, &auth, before).await,
        [
            entry("UserCreated", "User carol has been created"),
            entry(
                "UserPasswordChanged",
                "Password has been changed for user carol"
            ),
        ]
    );

    let before = entries(&router, &auth).await.len();
    let (status, body) = send(
        &router,
        &auth,
        "POST",
        "/Users/New",
        Some(serde_json::json!({ "Name": "dave" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        written_since(&router, &auth, before).await,
        [entry("UserCreated", "User dave has been created")]
    );
}

/// `UpdateUserPassword` changes or resets the password through
/// `ChangePassword`, which writes the one entry, on both of its routes. The
/// admin's seeded password wrote the first entry, as the wizard's does.
#[tokio::test]
async fn each_password_route_writes_one_entry() {
    let harness = boot(ADMIN_PASSWORD).await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let auth = authenticate(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let (type_, name, _) = entries(&router, &auth).await.remove(0);
    assert_eq!(
        (type_.as_str(), name.as_str()),
        (
            "UserPasswordChanged",
            "Password has been changed for user admin"
        ),
        "the seed's entry comes first"
    );
    let (status, carol) = send(
        &router,
        &auth,
        "POST",
        "/Users/New",
        Some(serde_json::json!({ "Name": "carol", "Password": "carol-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{carol}");
    let id = carol["Id"].as_str().expect("id");

    for (uri, body) in [
        (
            format!("/Users/Password?userId={id}"),
            serde_json::json!({ "NewPw": "carol-pw2" }),
        ),
        (
            format!("/Users/Password?userId={id}"),
            serde_json::json!({ "ResetPassword": true }),
        ),
        (
            format!("/Users/{id}/Password"),
            serde_json::json!({ "NewPw": "carol-pw3" }),
        ),
    ] {
        let before = entries(&router, &auth).await.len();
        let (status, answer) = send(&router, &auth, "POST", &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{uri} {body}: {answer}");
        assert_eq!(
            written_since(&router, &auth, before).await,
            [entry(
                "UserPasswordChanged",
                "Password has been changed for user carol"
            )],
            "{uri} {body}"
        );
    }
}

/// The startup wizard's `UpdateStartupUser` renames the first user, then
/// sets its password through `ChangePassword`, which writes the entry under
/// the new name (it reloads the user).
#[tokio::test]
async fn the_startup_wizard_logs_the_password_it_sets() {
    let harness = boot("").await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let (status, body) = send(
        &router,
        CLIENT_AUTH,
        "POST",
        "/Startup/User",
        Some(serde_json::json!({ "Name": "owner", "Password": "wizard-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let auth = authenticate(&router, "owner", "wizard-pw").await;
    let changes: Vec<_> = entries(&router, &auth)
        .await
        .into_iter()
        .filter(|(type_, _, _)| type_ == "UserPasswordChanged")
        .map(|(_, name, _)| name)
        .collect();
    assert_eq!(changes, ["Password has been changed for user owner"]);
}
