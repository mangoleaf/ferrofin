//! Renaming a device, end to end: `POST /Devices/Options` renames the device's
//! live sessions at once, as upstream's `DeviceOptionsUpdated` handler
//! (`SessionManager.OnDeviceManagerDeviceOptionsUpdated`) does.
//!
//! Boots the real composition root over a fresh temp database, because the
//! rename crosses two managers through the handler: an API test over fakes
//! would pass with the handler's call removed.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "rename-pw";

/// Sends one request from `device_id` (reporting the device name
/// `device_name`), optionally bearing `token`; returns status + JSON body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    (device_id, device_name): (&str, &str),
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let ident =
        format!(r#"Client="test", Device="{device_name}", DeviceId="{device_id}", Version="1""#);
    let auth = match token {
        Some(t) => format!(r#"MediaBrowser Token="{t}", {ident}"#),
        None => format!("MediaBrowser {ident}"),
    };
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth);
    let req = match body {
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string())),
        None => req.body(Body::empty()),
    };
    let res = router
        .clone()
        .oneshot(req.expect("request"))
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON body")
    };
    (status, json)
}

/// Boots the composition root over a temp DB and seeds the administrator;
/// returns the temp dir keeping its database alive (bound first, so dropped
/// last) and the wired app.
async fn boot() -> (tempfile::TempDir, WiredApp) {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-device-rename".to_owned(),
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
    (temp, wired)
}

#[tokio::test]
async fn renaming_a_device_renames_its_live_session() {
    let (_temp, wired) = boot().await;
    let router = ferrofin_api::create_router(wired.state.clone());

    let login = |device| {
        let router = router.clone();
        async move {
            let credentials = serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD });
            let (status, body) = call(
                &router,
                "POST",
                "/Users/AuthenticateByName",
                device,
                None,
                Some(credentials),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            body["AccessToken"].as_str().expect("token").to_owned()
        }
    };
    let admin = ("rename-admin", "Admin Rig");
    let tv = ("rename-tv", "Living Room TV");
    let admin_token = login(admin).await;
    let tv_token = login(tv).await;
    let (r, admin_token) = (&router, admin_token.as_str());
    let tv_session = move || async move {
        let (status, sessions) = call(r, "GET", "/Sessions", admin, Some(admin_token), None).await;
        assert_eq!(status, StatusCode::OK);
        let session = sessions
            .as_array()
            .expect("session list")
            .iter()
            .find(|s| s["DeviceId"] == tv.0)
            .expect("the TV's session")
            .clone();
        (
            session["DeviceName"].clone(),
            session["HasCustomDeviceName"].clone(),
        )
    };
    let rename = move |name: Option<&'static str>| async move {
        let (status, _) = call(
            r,
            "POST",
            "/Devices/Options?id=rename-tv",
            admin,
            Some(admin_token),
            Some(serde_json::json!({ "CustomName": name })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    };

    assert_eq!(tv_session().await, ("Living Room TV".into(), false.into()));

    rename(Some("Den")).await;
    assert_eq!(
        tv_session().await,
        ("Den".into(), true.into()),
        "renamed at once"
    );

    rename(None).await;
    assert_eq!(
        tv_session().await,
        ("Den".into(), false.into()),
        "clearing the name only clears the mark"
    );
    let (status, _) = call(
        &router,
        "POST",
        "/Sessions/Capabilities?playableMediaTypes=Video",
        tv,
        Some(&tv_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        tv_session().await,
        ("Living Room TV".into(), false.into()),
        "the device's next request reports its own name"
    );
}
