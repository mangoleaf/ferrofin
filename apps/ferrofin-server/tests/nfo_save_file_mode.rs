//! An NFO the metadata saver writes into a media folder has ordinary file
//! permissions, over the real router and composition root: a new one honours
//! the umask and a replaced one keeps its mode, as upstream's `BaseNfoSaver`
//! (a plain `FileStream`) does. Kodi, tinyMediaManager or a share running as
//! another user must still read it.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "nfo-mode-admin-pw";

#[tokio::test]
async fn saved_nfos_keep_ordinary_file_permissions() {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let movies = temp.path().join("movies");
    let kept = movies.join("Kept Film (2001)");
    let fresh = movies.join("Fresh Film (2002)");
    for (folder, name) in [(&kept, "Kept Film (2001)"), (&fresh, "Fresh Film (2002)")] {
        std::fs::create_dir_all(folder).expect("dir");
        std::fs::write(folder.join(format!("{name}.mkv")), b"").expect("movie");
    }
    let kept_nfo = kept.join("movie.nfo");
    std::fs::write(
        &kept_nfo,
        "<movie><title>Kept Film</title><year>2001</year></movie>",
    )
    .expect("nfo");
    std::fs::set_permissions(&kept_nfo, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    // What the process umask gives an ordinary new file.
    let probe = temp.path().join("probe");
    std::fs::write(&probe, b"").expect("probe");
    let umask_mode = mode(&probe);

    let (_wired, router, token) = scanned_server(temp.path(), &movies).await;
    let items = json_of(
        call(
            &router,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie",
            Some(&token),
            None,
        )
        .await,
    );
    for movie in items["Items"].as_array().expect("Items") {
        let id = movie["Id"].as_str().expect("id");
        let name = movie["Name"].as_str().expect("name");
        let (status, body) = call(
            &router,
            "POST",
            &format!("/Items/{id}"),
            Some(&token),
            Some(json!({ "Name": name, "Overview": "Edited." })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    let kept_text = std::fs::read_to_string(&kept_nfo).expect("kept nfo");
    assert!(kept_text.contains("Edited."), "the edit rewrote it");
    assert_eq!(mode(&kept_nfo), 0o644, "a replaced NFO keeps its mode");
    let fresh_nfo = fresh.join("movie.nfo");
    assert!(
        std::fs::read_to_string(&fresh_nfo)
            .expect("fresh nfo")
            .contains("Edited."),
        "the edit created it"
    );
    assert_eq!(mode(&fresh_nfo), umask_mode, "a new NFO honours the umask");
}

/// Boots the server on `root`, adds `movies` as a library that saves NFOs,
/// scans it, and signs in: the app (kept alive), the router and a token.
async fn scanned_server(
    root: &Path,
    movies: &Path,
) -> (ferrofin_server::state::WiredApp, axum::Router, String) {
    let config = Config {
        server_name: "ferrofin-nfo-file-mode".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(root)
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
    let login = json_of(
        call(
            &router,
            "POST",
            "/Users/AuthenticateByName",
            None,
            Some(json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
        )
        .await,
    );
    let token = login["AccessToken"].as_str().expect("token").to_owned();
    let path = encode(&movies.to_string_lossy());
    let (status, _) = call(
        &router,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Movies&collectionType=movies&paths={path}&refreshLibrary=false"
        ),
        Some(&token),
        Some(json!({ "LibraryOptions": { "MetadataSavers": ["Nfo"] } })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan")
    );
    (wired, router, token)
}

/// The permission bits of `path`.
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Sends one request as `token` and returns status + raw body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let ident = r#"Client="test", Device="d", DeviceId="nfo-mode", Version="1""#;
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
    (status, bytes)
}

/// The JSON body of a `200`.
fn json_of((status, bytes): (StatusCode, Bytes)) -> Value {
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("json")
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
