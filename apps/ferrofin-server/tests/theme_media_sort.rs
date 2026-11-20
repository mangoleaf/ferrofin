//! The order of an item's theme songs over the real router and composition root.
//!
//! `sortBy` / `sortOrder` pair as `RequestHelpers.GetOrderBy` pairs them: a key
//! past the supplied orders takes the FIRST order, so
//! `sortBy=IsFolder,SortName&sortOrder=Descending` sorts the names descending.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "theme-sort-pw";
const CLIENT: &str = r#"Client="test", Device="d", DeviceId="theme-sort", Version="1""#;

/// Sends one request as `token` and returns status + JSON body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = match token {
        Some(t) => format!(r#"MediaBrowser Token="{t}", {CLIENT}"#),
        None => format!("MediaBrowser {CLIENT}"),
    };
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, "application/json");
    let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
    let res = router
        .clone()
        .oneshot(req.body(body).expect("request"))
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn theme_songs_pad_missing_sort_orders_with_the_first_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let movie = temp.path().join("media/Film Alpha (1999)");
    std::fs::create_dir_all(movie.join("theme-music")).expect("theme dir");
    std::fs::write(movie.join("Film Alpha (1999).mkv"), [0u8; 1024]).expect("movie");
    for song in ["Aria", "Bolero"] {
        std::fs::write(movie.join(format!("theme-music/{song}.mp3")), [0u8; 1024]).expect("song");
    }
    let config = Config {
        server_name: "ferrofin-theme-sort".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    std::fs::create_dir_all(&config.data_dir).expect("data dir");
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

    let (status, auth) = call(
        &router,
        "POST",
        "/Users/AuthenticateByName",
        None,
        Some(json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{auth}");
    let token = auth["AccessToken"].as_str().expect("token").to_owned();

    // No remote fetchers: a scan that asks TMDb or MusicBrainz waits on them.
    let media = temp.path().join("media");
    let (status, body) = call(
        &router,
        "POST",
        "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=false",
        Some(&token),
        Some(json!({"LibraryOptions": {
            "PathInfos": [{"Path": media.to_str().expect("utf-8 path")}],
            "EnableRealtimeMonitor": false,
            "TypeOptions": [
                {"Type": "Movie", "MetadataFetchers": [], "ImageFetchers": []},
                {"Type": "Audio", "MetadataFetchers": [], "ImageFetchers": []}
            ]
        }})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "library added: {body}");
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan")
    );
    let (_, movies) = call(
        &router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie",
        Some(&token),
        None,
    )
    .await;
    let movie_id = movies["Items"][0]["Id"].as_str().expect("movie id");

    let names = async |query: &str| {
        let uri = format!("/Items/{movie_id}/ThemeSongs?{query}");
        let (status, songs) = call(&router, "GET", &uri, Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {songs}");
        songs["Items"]
            .as_array()
            .expect("Items")
            .iter()
            .map(|song| song["Name"].as_str().expect("name").to_owned())
            .collect::<Vec<_>>()
    };
    // Control: with no order, every key sorts ascending.
    assert_eq!(names("sortBy=IsFolder,SortName").await, ["Aria", "Bolero"]);
    // `SortName` has no order of its own, so it takes the first, Descending.
    assert_eq!(
        names("sortBy=IsFolder,SortName&sortOrder=Descending").await,
        ["Bolero", "Aria"]
    );
}
