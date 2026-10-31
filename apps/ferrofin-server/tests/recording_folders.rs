//! The recording folders as libraries, end to end: `GET
//! /LiveTv/Recordings/Folders`, `GET /Library/VirtualFolders` and the `livetv`
//! save that links them (`RecordingsManager.OnNamedConfigurationUpdated`),
//! over the real composition root and a data directory that survives a restart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "recording-folders-pw";

/// Boots the real server over `root`.
async fn boot(root: &Path) -> WiredApp {
    for directory in ["config", "data", "cache"] {
        std::fs::create_dir_all(root.join(directory)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-recording-folders".to_owned(),
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
    wired
}

/// Sends one request and returns status + body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let ident = r#"Client="test", Device="d", DeviceId="recording-folders", Version="1""#;
    let request = Request::builder().method(method).uri(uri).header(
        header::AUTHORIZATION,
        token.map_or_else(
            || format!("MediaBrowser {ident}"),
            |token| format!(r#"MediaBrowser Token="{token}", {ident}"#),
        ),
    );
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(body).expect("body"))),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn login(router: &axum::Router, name: &str, password: &str) -> String {
    let (status, body) = call(
        router,
        "POST",
        "/Users/AuthenticateByName",
        None,
        Some(&json!({ "Username": name, "Pw": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{name} authenticates: {body}");
    body["AccessToken"].as_str().expect("token").to_owned()
}

/// Saves `changes` over the stored `livetv` document, as the dashboard does.
async fn save_live_tv(router: &axum::Router, token: &str, changes: &Value) {
    let (status, mut document) = call(
        router,
        "GET",
        "/System/Configuration/livetv",
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (member, value) in changes.as_object().expect("an object") {
        document[member] = value.clone();
    }
    let (status, body) = call(
        router,
        "POST",
        "/System/Configuration/livetv",
        Some(token),
        Some(&document),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// `(Name, Locations, CollectionType)` of each library, by name.
async fn libraries(router: &axum::Router, token: &str) -> Vec<(String, Vec<String>, Value)> {
    let (status, body) = call(router, "GET", "/Library/VirtualFolders", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
    let mut rows: Vec<_> = body
        .as_array()
        .expect("a list")
        .iter()
        .map(|folder| {
            (
                folder["Name"].as_str().expect("name").to_owned(),
                serde_json::from_value(folder["Locations"].clone()).expect("locations"),
                folder["CollectionType"].clone(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// The `Name` of each folder the route answers, in its order.
async fn recording_folders(router: &axum::Router, token: &str) -> Vec<String> {
    let (status, body) = call(
        router,
        "GET",
        "/LiveTv/Recordings/Folders",
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["Items"].as_array().expect("items");
    assert_eq!(body["TotalRecordCount"], items.len());
    items
        .iter()
        .map(|item| {
            assert_eq!(item["Type"], "CollectionFolder", "{item}");
            item["Name"].as_str().expect("name").to_owned()
        })
        .collect()
}

fn made(root: &Path, name: &str) -> String {
    let path = root.join(name);
    std::fs::create_dir_all(&path).expect("dir");
    path.display().to_string()
}

/// `MediaLocationsCreated` as stored.
fn media_locations(wired: &WiredApp) -> Value {
    let directory = wired
        .state
        .config
        .application_paths()
        .user_configuration_directory_path();
    let saved: Value = serde_json::from_slice(
        &std::fs::read(Path::new(&directory).join("named/livetv.json")).expect("livetv.json"),
    )
    .expect("json");
    saved["MediaLocationsCreated"].clone()
}

#[tokio::test]
async fn a_livetv_save_makes_the_libraries_and_nothing_adds_them_twice() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let value = recording_folders(&router, &token).await;
    assert!(value.is_empty(), "{value:?}");

    let default = made(temp.path(), "dvr");
    let movies = made(temp.path(), "dvr-movies");
    let shows = made(temp.path(), "dvr-shows");
    let paths = json!({
        "RecordingPath": default,
        "MovieRecordingPath": movies,
        "SeriesRecordingPath": shows,
    });
    save_live_tv(&router, &token, &paths).await;

    let expected = vec![
        (
            "Recorded Movies".to_owned(),
            vec![movies.clone()],
            json!("movies"),
        ),
        (
            "Recorded Shows".to_owned(),
            vec![shows.clone()],
            json!("tvshows"),
        ),
        ("Recordings".to_owned(), vec![default.clone()], Value::Null),
    ];
    assert_eq!(libraries(&router, &token).await, expected);
    assert_eq!(
        recording_folders(&router, &token).await,
        ["Recorded Movies", "Recorded Shows", "Recordings"]
    );
    assert_eq!(media_locations(&wired), json!([default, movies, shows]));

    // The same save again adds nothing.
    save_live_tv(&router, &token, &paths).await;
    assert_eq!(libraries(&router, &token).await, expected);
    assert_eq!(recording_folders(&router, &token).await.len(), 3);

    // A restart: `RecordingsHost` runs the same pass.
    drop(router);
    drop(wired);
    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(libraries(&router, &token).await, expected);
    assert_eq!(media_locations(&wired), json!([default, movies, shows]));

    // A folder that stops being one leaves the library again, and nothing else.
    save_live_tv(&router, &token, &json!({ "MovieRecordingPath": null })).await;
    let names: Vec<String> = libraries(&router, &token)
        .await
        .into_iter()
        .map(|(name, ..)| name)
        .collect();
    assert_eq!(names, ["Recorded Shows", "Recordings"]);
    assert_eq!(media_locations(&wired), json!([default, shows]));
}

/// A location under a larger library is not that library's location, so it
/// gets a library of its own, and the route answers both, as
/// `GetCollectionFolders` does for a `Folder` under a library.
#[tokio::test]
async fn a_folder_inside_a_library_is_answered_with_that_library() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let media = made(temp.path(), "media");
    let dvr = made(temp.path(), "media/dvr");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Library/VirtualFolders?name=Media&paths={media}&refreshLibrary=false"),
        Some(&token),
        Some(&json!({ "LibraryOptions": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    save_live_tv(&router, &token, &json!({ "RecordingPath": dvr })).await;

    assert_eq!(
        recording_folders(&router, &token).await,
        ["Media", "Recordings"]
    );
}

/// `DELETE /Items/{id}` of a movie in its own folder takes the folder, which
/// must end the recording too, where nothing else would: no file and no
/// folder is what an unmounted share looks like.
#[tokio::test]
async fn a_library_delete_ends_the_recording() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let movies = made(temp.path(), "dvr");
    let file = temp.path().join("dvr/Evening Movie/Evening Movie.ts");
    std::fs::create_dir_all(file.parent().expect("a folder")).expect("dir");
    std::fs::write(&file, b"recorded").expect("file");
    let (status, body) = call(
        &router,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Films&collectionType=movies&paths={movies}\
             &refreshLibrary=false"
        ),
        Some(&token),
        Some(&json!({ "LibraryOptions": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let db = Database::connect(&Config::test_stub(temp.path()).database_url())
        .await
        .expect("open db");
    let id = uuid::Uuid::new_v4();
    let info = ferrofin_livetv::TimerRecordingInfo {
        id: "timer".to_owned(),
        name: "Evening Movie".to_owned(),
        ..ferrofin_livetv::TimerRecordingInfo::default()
    };
    let created = "2026-10-30T00:00:00Z".parse().expect("a date");
    ferrofin_livetv::dvr_repository::insert_recording(&db, id, &info, &file, created)
        .await
        .expect("insert");
    ferrofin_livetv::dvr_repository::finish_recording(
        &db,
        id,
        ferrofin_model::live_tv::RecordingStatus::Completed,
        file.to_str(),
    )
    .await
    .expect("finish");

    let recordings = async |router: &axum::Router| -> usize {
        let (status, body) = call(router, "GET", "/LiveTv/Recordings", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["Items"].as_array().expect("items").len()
    };
    assert_eq!(recordings(&router).await, 1);

    assert!(
        wired
            .state
            .library
            .run_library_scan(ferrofin_traits::library::ScanTrigger::Api, None)
            .await
            .expect("scan"),
        "the scan ran to its end"
    );
    let (status, body) = call(
        &router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item = body["Items"][0]["Id"].as_str().expect("the scanned item");

    let (status, body) = call(
        &router,
        "DELETE",
        &format!("/Items/{item}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert!(!file.exists());
    assert_eq!(recordings(&router).await, 0);
}

/// `RecordingsHost.StartAsync`: the pass runs at boot, for a directory that
/// appeared while the server was down.
#[tokio::test]
async fn a_boot_makes_the_library_of_a_directory_that_appeared() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let value = libraries(&router, &token).await;
    assert!(value.is_empty(), "{value:?}");
    let data = wired.state.config.application_paths().data_path();
    let default = made(Path::new(&data), "livetv/recordings");

    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;

    let mut rows = Vec::new();
    for _ in 0..200 {
        rows = libraries(&router, &token).await;
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "Recordings");
    assert_eq!(rows[0].1, std::slice::from_ref(&default));
    assert_eq!(media_locations(&wired), json!([default]));
}

/// The route only lists libraries the user may see, and a location that is
/// already in a library is that library, under its own name.
#[tokio::test]
async fn a_folder_is_answered_as_the_library_that_holds_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let wired = boot(temp.path()).await;
    let router = ferrofin_api::create_router(wired.state.clone());
    let token = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let default = made(temp.path(), "dvr");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Library/VirtualFolders?name=My%20Shows&paths={default}&refreshLibrary=false"),
        Some(&token),
        Some(&json!({ "LibraryOptions": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    save_live_tv(&router, &token, &json!({ "RecordingPath": default })).await;

    let rows = libraries(&router, &token).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "My Shows");
    assert_eq!(media_locations(&wired), json!([]));
    assert_eq!(recording_folders(&router, &token).await, ["My Shows"]);

    // A user without access to that library gets none of it.
    let (status, viewer) = call(
        &router,
        "POST",
        "/Users/New",
        Some(&token),
        Some(&json!({ "Name": "viewer", "Password": "pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{viewer}");
    let viewer_id = viewer["Id"].as_str().expect("id");
    let mut policy = viewer["Policy"].clone();
    policy["EnableAllFolders"] = json!(false);
    policy["EnabledFolders"] = json!([]);
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        Some(&token),
        Some(&policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let viewer_token = login(&router, "viewer", "pw").await;
    let value = recording_folders(&router, &viewer_token).await;
    assert!(value.is_empty(), "{value:?}");
}
