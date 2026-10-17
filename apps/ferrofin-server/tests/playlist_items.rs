//! What a playlist create writes, over the real router, database and
//! playlist manager.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::Database;
use ferrofin_server::{config::Config, state::build_app_state};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const PLAYLIST_TYPE: &str = "MediaBrowser.Controller.Playlists.Playlist";

async fn call(
    router: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let auth = format!(
        r#"MediaBrowser Client="playlist-items-test", Device="fixture", DeviceId="playlist-items-test", Version="1", Token="{token}""#
    );
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, auth)
        .header(header::CONTENT_TYPE, "application/json");
    let request = match body {
        Some(body) => request.body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// The server, logged in as its admin.
struct Fixture {
    _temp: tempfile::TempDir,
    db: Database,
    router: axum::Router,
    token: String,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        admin_user: "admin".to_owned(),
        admin_password: "playlist-pw".to_owned(),
        ..Config::test_stub(temp.path())
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let db = Database::connect(&config.database_url()).await.unwrap();
    db.run_migrations().await.unwrap();
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .unwrap();
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .unwrap();
    let router = ferrofin_api::create_router(wired.state.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(
            header::AUTHORIZATION,
            r#"MediaBrowser Client="playlist-items-test", Device="fixture", DeviceId="playlist-items-test", Version="1""#,
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"Username": "admin", "Pw": "playlist-pw"}).to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let auth: Value = serde_json::from_slice(&bytes).unwrap();
    let token = auth["AccessToken"].as_str().unwrap().to_owned();

    Fixture {
        _temp: temp,
        db,
        router,
        token,
    }
}

impl Fixture {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        call(&self.router, &self.token, method, path, body).await
    }

    async fn playlist_count(&self) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "BaseItems" WHERE "Type" = ?1"#)
            .bind(PLAYLIST_TYPE)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

/// A create keeps its media type and its shares side by side in `Data`,
/// which Jellyfin reads both back from.
#[tokio::test]
async fn a_create_keeps_its_media_type_and_shares_in_data() {
    let fx = fixture().await;
    let sharee = Uuid::new_v4();
    let (status, created) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({
                "Name": "Shared",
                "MediaType": "Video",
                "Users": [{"UserId": sharee, "CanEdit": true}],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id: Uuid = created["Id"].as_str().unwrap().parse().unwrap();
    let data: String = sqlx::query_scalar(r#"SELECT "Data" FROM "BaseItems" WHERE "Id" = ?1"#)
        .bind(ferrofin_db::store::guid_to_db(id))
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    let data: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(data["PlaylistMediaType"], "Video", "{data}");
    let share = &data["Shares"][0];
    assert_eq!(
        share["UserId"].as_str().unwrap().parse::<Uuid>().unwrap(),
        sharee,
        "{data}"
    );
    assert_eq!(share["CanEdit"], true, "{data}");
}

/// A create that fails part-way leaves nothing behind — no playlist row
/// without its owner row, which every caller would open as its owner.
#[tokio::test]
async fn a_create_that_fails_part_way_leaves_no_playlist() {
    let fx = fixture().await;
    sqlx::query(
        r#"CREATE TRIGGER "fail_playlist_owner" BEFORE INSERT ON "FerrofinPlaylists"
           BEGIN SELECT RAISE(ABORT, 'injected failure'); END"#,
    )
    .execute(fx.db.writer())
    .await
    .unwrap();

    let (status, body) = fx
        .call("POST", "/Playlists", Some(json!({"Name": "Doomed"})))
        .await;
    assert!(status.is_server_error(), "{status} {body}");
    assert_eq!(
        fx.playlist_count().await,
        0,
        "the BaseItems row rolled back"
    );
}

/// The same when the last write, the `Data` sync, fails: the owner row and
/// the shares roll back with the playlist row.
#[tokio::test]
async fn a_create_whose_last_write_fails_leaves_nothing() {
    let fx = fixture().await;
    sqlx::query(
        r#"CREATE TRIGGER "fail_playlist_data" BEFORE UPDATE OF "Data" ON "BaseItems"
           WHEN old."Type" = 'MediaBrowser.Controller.Playlists.Playlist'
           BEGIN SELECT RAISE(ABORT, 'injected failure'); END"#,
    )
    .execute(fx.db.writer())
    .await
    .unwrap();

    let (status, body) = fx
        .call(
            "POST",
            "/Playlists",
            Some(json!({
                "Name": "Doomed",
                "Users": [{"UserId": Uuid::new_v4(), "CanEdit": false}],
            })),
        )
        .await;
    assert!(status.is_server_error(), "{status} {body}");
    assert_eq!(fx.playlist_count().await, 0);
    assert_eq!(
        fx.count(r#"SELECT COUNT(*) FROM "FerrofinPlaylists""#)
            .await,
        0
    );
    assert_eq!(
        fx.count(r#"SELECT COUNT(*) FROM "FerrofinPlaylistShares""#)
            .await,
        0
    );
}
