//! Token revocation, end to end: a user's other signed-in devices lose their
//! tokens when the password changes, the user is deleted or disabled.
//!
//! Boots the real composition root over a fresh temp database, so the session
//! manager, the device manager and the shared auth cache are the ones the
//! server wires. Revocation is spread across the three, and a fake in any one
//! of them would let a broken composition pass: the API tests only check that
//! `revoke_user_tokens` was called.
//!
//! Each case signs the same user in on two devices and asserts which of the two
//! tokens still works afterwards.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

/// The seeded administrator's credentials.
const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "revocation-pw";
/// The administrator's own device, distinct from the user's two.
const ADMIN_DEVICE: &str = "revoke-admin";
/// The ordinary account each case signs in twice.
const USER_NAME: &str = "viewer";
const USER_PASSWORD: &str = "viewer-pw";
/// The user's two devices.
const DEVICE_A: &str = "revoke-a";
const DEVICE_B: &str = "revoke-b";

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
        server_name: "ferrofin-revocation".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };

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

/// Sends one request from `device`, optionally bearing `token`, and returns
/// status + body.
async fn call_as(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    device: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
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

/// Authenticates `name`/`password` on `device` and returns the token.
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
    assert_eq!(
        status,
        StatusCode::OK,
        "{name} must authenticate on {device}"
    );
    let v: serde_json::Value = serde_json::from_slice(&body).expect("auth json");
    v["AccessToken"]
        .as_str()
        .expect("AccessToken present")
        .to_owned()
}

/// The status `GET /Users/Me` answers for `token` on `device`.
async fn me_status(router: &axum::Router, token: &str, device: &str) -> StatusCode {
    call_as(router, "GET", "/Users/Me", Some(token), device, None)
        .await
        .0
}

/// A server with an administrator token and an ordinary user signed in on
/// [`DEVICE_A`] and [`DEVICE_B`].
struct SignedIn {
    router: axum::Router,
    admin: String,
    user_id: String,
    token_a: String,
    token_b: String,
    _harness: Harness,
}

/// Boots, creates the ordinary user and signs it in on both devices.
async fn signed_in_twice() -> SignedIn {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());
    let admin = login(&router, ADMIN_USER, ADMIN_PASSWORD, ADMIN_DEVICE).await;
    let (status, body) = call_as(
        &router,
        "POST",
        "/Users/New",
        Some(&admin),
        ADMIN_DEVICE,
        Some(serde_json::json!({ "Name": USER_NAME, "Password": USER_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin may create a user");
    let created: serde_json::Value = serde_json::from_slice(&body).expect("user json");
    let user_id = created["Id"].as_str().expect("new user id").to_owned();

    let token_a = login(&router, USER_NAME, USER_PASSWORD, DEVICE_A).await;
    let token_b = login(&router, USER_NAME, USER_PASSWORD, DEVICE_B).await;
    assert_ne!(token_a, token_b, "each device gets its own token");
    assert_eq!(me_status(&router, &token_a, DEVICE_A).await, StatusCode::OK);
    assert_eq!(me_status(&router, &token_b, DEVICE_B).await, StatusCode::OK);
    SignedIn {
        router,
        admin,
        user_id,
        token_a,
        token_b,
        _harness: harness,
    }
}

/// The user's policy as `GET /Users/{id}` reports it, for a save that changes
/// only the fields a case names, as the dashboard does.
async fn policy_of(s: &SignedIn) -> serde_json::Value {
    let (status, body) = call_as(
        &s.router,
        "GET",
        &format!("/Users/{}", s.user_id),
        Some(&s.admin),
        ADMIN_DEVICE,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user: serde_json::Value = serde_json::from_slice(&body).expect("user json");
    user["Policy"].clone()
}

/// Saves `policy` for the user as the administrator.
async fn save_policy(s: &SignedIn, policy: serde_json::Value) {
    let (status, body) = call_as(
        &s.router,
        "POST",
        &format!("/Users/{}/Policy", s.user_id),
        Some(&s.admin),
        ADMIN_DEVICE,
        Some(policy),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );
}

/// The ids of the devices the user is signed in on: the `GET /Devices` rows
/// whose `LastUserId` is theirs. (`?userId=` filters by the devices a user
/// may use, `GetDevicesForUser`'s `CanAccessDevice`, not by whose they are.)
async fn device_ids_of(s: &SignedIn) -> Vec<String> {
    let (status, body) = call_as(
        &s.router,
        "GET",
        "/Devices",
        Some(&s.admin),
        ADMIN_DEVICE,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let devices: serde_json::Value = serde_json::from_slice(&body).expect("devices json");
    let user = s.user_id.replace('-', "").to_lowercase();
    let mut ids: Vec<String> = devices["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .filter(|d| {
            d["LastUserId"]
                .as_str()
                .is_some_and(|id| id.replace('-', "").to_lowercase() == user)
        })
        .map(|d| d["Id"].as_str().expect("device Id").to_owned())
        .collect();
    ids.sort();
    ids
}

/// `UserController.UpdateUserPassword` → `RevokeUserTokens(user.Id, currentToken)`:
/// the device the change was made from keeps its token, every other one loses it.
#[tokio::test]
async fn a_password_change_signs_out_the_users_other_devices() {
    let s = signed_in_twice().await;

    let (status, body) = call_as(
        &s.router,
        "POST",
        "/Users/Password",
        Some(&s.token_a),
        DEVICE_A,
        Some(serde_json::json!({ "CurrentPw": USER_PASSWORD, "NewPw": "changed-pw" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );

    assert_eq!(
        me_status(&s.router, &s.token_a, DEVICE_A).await,
        StatusCode::OK,
        "the device that changed the password stays signed in"
    );
    assert_eq!(
        me_status(&s.router, &s.token_b, DEVICE_B).await,
        StatusCode::UNAUTHORIZED,
        "the other device's token is revoked"
    );
    assert_eq!(device_ids_of(&s).await, vec![DEVICE_A.to_owned()]);
}

/// An administrator changing the user's password keeps only the
/// administrator's own token: none of the user's devices is the caller's.
#[tokio::test]
async fn an_administrator_setting_the_password_signs_out_every_device_of_the_user() {
    let s = signed_in_twice().await;

    let (status, body) = call_as(
        &s.router,
        "POST",
        &format!("/Users/Password?userId={}", s.user_id),
        Some(&s.admin),
        ADMIN_DEVICE,
        Some(serde_json::json!({ "NewPw": "set-by-admin" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );

    for (token, device) in [(&s.token_a, DEVICE_A), (&s.token_b, DEVICE_B)] {
        assert_eq!(
            me_status(&s.router, token, device).await,
            StatusCode::UNAUTHORIZED,
            "{device} is signed out"
        );
    }
    assert_eq!(
        me_status(&s.router, &s.admin, ADMIN_DEVICE).await,
        StatusCode::OK,
        "the administrator's own token is untouched"
    );
    assert!(device_ids_of(&s).await.is_empty());
}

/// `UserController.DeleteUser` → `RevokeUserTokens(user.Id, null)`: every device
/// row of the user goes, not only the user row.
#[tokio::test]
async fn deleting_a_user_revokes_every_token_and_device_of_theirs() {
    let s = signed_in_twice().await;
    assert_eq!(
        device_ids_of(&s).await,
        vec![DEVICE_A.to_owned(), DEVICE_B.to_owned()]
    );

    let (status, body) = call_as(
        &s.router,
        "DELETE",
        &format!("/Users/{}", s.user_id),
        Some(&s.admin),
        ADMIN_DEVICE,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );

    for (token, device) in [(&s.token_a, DEVICE_A), (&s.token_b, DEVICE_B)] {
        assert_eq!(
            me_status(&s.router, token, device).await,
            StatusCode::UNAUTHORIZED,
            "{device} is signed out"
        );
    }
    assert!(
        device_ids_of(&s).await.is_empty(),
        "the deleted user's devices are gone"
    );
}

/// `UserController.UpdateUserPolicy` revokes a newly disabled user's tokens. A
/// disabled account is refused on every request anyway, so the proof that the
/// tokens are gone, and not merely refused, is that they stay refused once the
/// account is enabled again.
#[tokio::test]
async fn disabling_a_user_revokes_their_tokens_for_good() {
    let s = signed_in_twice().await;
    let mut policy = policy_of(&s).await;

    policy["IsDisabled"] = serde_json::Value::Bool(true);
    save_policy(&s, policy.clone()).await;
    assert!(device_ids_of(&s).await.is_empty());

    policy["IsDisabled"] = serde_json::Value::Bool(false);
    save_policy(&s, policy).await;
    for (token, device) in [(&s.token_a, DEVICE_A), (&s.token_b, DEVICE_B)] {
        assert_eq!(
            me_status(&s.router, token, device).await,
            StatusCode::UNAUTHORIZED,
            "{device}'s old token stays revoked after the user is enabled again"
        );
    }
    login(&s.router, USER_NAME, USER_PASSWORD, DEVICE_A).await;
}
