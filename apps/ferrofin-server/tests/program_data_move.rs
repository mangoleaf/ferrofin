//! A database whose program-data directory moved (gl-1165): a Debian
//! `/var/lib/jellyfin` adopted into `/var/lib/ferrofin`, or a relocated
//! `data_dir`. Its ids still match, because they derive from program-data
//! relative keys, but the stored paths still name the old directory. The next
//! start rewrites each path whose file moved, once, and the artwork and the
//! profile image are served again.
use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use ferrofin_db::{Database, store::guid_to_db};
use ferrofin_server::{config::Config, state::build_app_state};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const AUTH: &str = r#"MediaBrowser Client="program-data-move-test", Device="fixture", DeviceId="program-data-move-test", Version="1""#;

/// The status of `GET path` as the admin, and the body when it is not a 200.
async fn get(router: &axum::Router, token: &str, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, format!(r#"{AUTH}, Token="{token}""#))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        return (status, String::new());
    }
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// `method path` as the admin with a JSON `body`: the status and the JSON
/// answer (`Null` for an empty one).
async fn send(
    router: &axum::Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!(r#"{AUTH}, Token="{token}""#))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Logs the admin in: its access token and user id.
async fn login(router: &axum::Router) -> (String, Uuid) {
    let request = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(header::AUTHORIZATION, AUTH)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"Username": "admin", "Pw": "move-pw"}).to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let auth: Value = serde_json::from_slice(&bytes).unwrap();
    (
        auth["AccessToken"].as_str().unwrap().to_owned(),
        auth["User"]["Id"].as_str().unwrap().parse().unwrap(),
    )
}

fn config(root: &Path) -> Config {
    Config {
        admin_user: "admin".to_owned(),
        admin_password: "move-pw".to_owned(),
        ..Config::test_stub(root)
    }
}

fn ffmpeg() -> ferrofin_server::bootstrap::FfmpegPaths {
    ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    }
}

/// Builds the server over `db`, as a start does, and returns its router.
async fn start(db: &Database, root: &Path) -> axum::Router {
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(db, &config(root), &ffmpeg(), None, shutdown_tx)
        .await
        .unwrap();
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config(root))
        .await
        .unwrap();
    ferrofin_api::create_router(wired.state)
}

/// Every stored path the repair may rewrite, in a fixed order.
async fn stored_paths(db: &Database) -> Vec<String> {
    sqlx::query_scalar(
        r#"SELECT "Path" FROM "BaseItems" WHERE "Path" IS NOT NULL
           UNION ALL SELECT "Path" FROM "BaseItemImageInfos"
           UNION ALL SELECT "Path" FROM "ImageInfos"
           ORDER BY 1"#,
    )
    .fetch_all(db.pool())
    .await
    .unwrap()
}

fn png(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    image::RgbImage::new(2, 2).save(path).unwrap();
}

/// Where the moved database's program data used to be, from this server's.
#[derive(Debug, Clone, Copy)]
enum Previous {
    /// Somewhere else entirely, like Debian's `/var/lib/jellyfin`, copied with
    /// the originals left in place, as the documented migration does.
    Elsewhere,
    /// The parent of this one: `/srv/ferrofin` moved to `/srv/ferrofin/data`.
    Parent,
    /// A child of this one: `/var/lib/ferrofin/data` moved to `/var/lib/ferrofin`.
    Child,
}

impl Previous {
    fn dir(self, program_data: &Path) -> PathBuf {
        match self {
            Self::Elsewhere => program_data.parent().unwrap().join("moved-from"),
            Self::Parent => program_data.parent().unwrap().to_path_buf(),
            Self::Child => program_data.join("data"),
        }
    }
}

#[rstest]
#[case::moved_elsewhere(Previous::Elsewhere, false)]
#[case::already_booted_by_v1_4(Previous::Elsewhere, true)]
#[case::into_a_subdirectory(Previous::Parent, false)]
#[case::out_of_a_subdirectory(Previous::Child, false)]
#[tokio::test]
async fn a_start_after_the_program_data_directory_moved_rewrites_the_moved_paths(
    #[case] previous: Previous,
    #[case] aggregate_already_repathed: bool,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let program_data = config(root).data_dir;
    std::fs::create_dir_all(&program_data).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    let db = Database::connect(&config(root).database_url())
        .await
        .unwrap();
    db.run_migrations().await.unwrap();
    let router = start(&db, root).await;
    let (token, admin) = login(&router).await;
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/Library/VirtualFolders?name=Media&refreshLibrary=false")
        .header(header::AUTHORIZATION, format!(r#"{AUTH}, Token="{token}""#))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"LibraryOptions":{"PathInfos":[{"Path":media.to_str().unwrap()}],"EnableRealtimeMonitor":false,"TypeOptions":[]}})
                .to_string(),
        ))
        .unwrap();
    let status = router.clone().oneshot(request).await.unwrap().status();
    assert!(status.is_success(), "{status}");
    let library: String = sqlx::query_scalar(
        r#"SELECT "Id" FROM "BaseItems"
           WHERE "Type" = 'MediaBrowser.Controller.Entities.CollectionFolder' AND "Name" = 'Media'"#,
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let library: Uuid = library.parse().unwrap();

    // The artwork and the profile image this server has, at its own paths.
    let metadata = program_data.join("metadata");
    let config_dir = root.join("config");
    png(&metadata.join("library/me/poster.png"));
    png(&config_dir.join("users/admin/profile.png"));

    // The database as it was written under the previous directories: every
    // program-data path under `old`, the profile image under an old
    // configuration directory, and one image whose file never moved.
    let old = previous.dir(&program_data);
    // The originals left where they were, which must not stop the move.
    let old_profile = root.join("moved-from-etc/users/admin/profile.png");
    png(&old_profile);
    if matches!(previous, Previous::Elsewhere) {
        png(&old.join("metadata/library/me/poster.png"));
    }
    let (old, pd) = (old.to_str().unwrap(), program_data.to_str().unwrap());
    sqlx::query(
        r#"UPDATE "BaseItems" SET "Path" = ?1 || substr("Path", length(?2) + 1)
           WHERE substr("Path", 1, length(?2) + 1) = ?2 || '/'"#,
    )
    .bind(old)
    .bind(pd)
    .execute(db.writer())
    .await
    .unwrap();
    for (image_type, path) in [
        (0, format!("{old}/metadata/library/me/poster.png")),
        (2, format!("{old}/metadata/library/gone/backdrop.png")),
    ] {
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
               ("Id", "ItemId", "Path", "ImageType", "DateModified", "Width", "Height")
               VALUES (?1, ?2, ?3, ?4, '2026-01-01 00:00:00', 2, 2)"#,
        )
        .bind(guid_to_db(Uuid::new_v4()))
        .bind(guid_to_db(library))
        .bind(path)
        .bind(image_type)
        .execute(db.writer())
        .await
        .unwrap();
    }
    sqlx::query(r#"INSERT INTO "ImageInfos" ("LastModified", "Path", "UserId") VALUES ('2026-01-01 00:00:00', ?1, ?2)"#)
        .bind(old_profile.to_str().unwrap())
        .bind(guid_to_db(admin))
        .execute(db.writer())
        .await
        .unwrap();
    if aggregate_already_repathed {
        // What a v1.4.x start left: `ensure()` re-pathed the aggregate root, and
        // nothing else.
        sqlx::query(
            r#"UPDATE "BaseItems" SET "Path" = ?1
               WHERE "Type" = 'MediaBrowser.Controller.Entities.AggregateFolder'"#,
        )
        .bind(format!("{pd}/root"))
        .execute(db.writer())
        .await
        .unwrap();
    }

    let snapshot = program_data.join("ferrofin.db.pre-program-data-move");
    assert!(!snapshot.exists(), "a start that did not move takes none");

    let router = start(&db, root).await;

    assert!(snapshot.is_file(), "the rewrite takes a snapshot first");
    let mut expected = vec![
        format!("{old}/metadata/library/gone/backdrop.png"),
        // stored as upstream's save writes it, so a move leaves it alone (gl-1581)
        "%AppDataPath%/playlists".to_owned(),
        format!("{pd}/metadata/library/me/poster.png"),
        format!("{pd}/root"),
        format!("{pd}/root/default"),
        format!("{pd}/root/default/Media"),
        config_dir
            .join("users/admin/profile.png")
            .to_str()
            .unwrap()
            .to_owned(),
    ];
    expected.sort();
    assert_eq!(stored_paths(&db).await, expected);
    let image = format!("/Items/{}/Images/Primary", library.simple());
    assert_eq!(get(&router, &token, &image).await.0, StatusCode::OK);
    let profile = format!("/Users/{}/Images/Primary", admin.simple());
    let (status, body) = get(&router, &token, &profile).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    drop(start(&db, root).await);
    assert_eq!(
        stored_paths(&db).await,
        expected,
        "a second start rewrites nothing"
    );
}

/// gl-941: a box set and a playlist made through the API store their folders
/// as `%AppDataPath%/…`, as Jellyfin's save does, so after the program-data
/// directory moves they read back under the new one with nothing rewritten.
#[tokio::test]
async fn a_box_set_and_a_playlist_follow_a_move_with_no_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let (before, after) = (temp.path().join("before"), temp.path().join("after"));
    for root in [&before, &after] {
        std::fs::create_dir_all(config(root).data_dir).unwrap();
        std::fs::create_dir_all(root.join("config")).unwrap();
    }
    let db = Database::connect(&config(&before).database_url())
        .await
        .unwrap();
    db.run_migrations().await.unwrap();
    let router = start(&db, &before).await;
    let (token, admin) = login(&router).await;

    let (status, box_set) = send(&router, &token, "POST", "/Collections?name=Heist", None).await;
    assert_eq!(status, StatusCode::OK, "{box_set}");
    let (status, playlist) = send(
        &router,
        &token,
        "POST",
        "/Playlists",
        Some(json!({"Name": "Road Trip", "UserId": admin.simple().to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{playlist}");
    let made = [
        (
            box_set["Id"].as_str().unwrap().to_owned(),
            "%AppDataPath%/collections/Heist [boxset]",
            "data/collections/Heist [boxset]",
        ),
        (
            playlist["Id"].as_str().unwrap().to_owned(),
            "%AppDataPath%/playlists/Road Trip",
            "data/playlists/Road Trip",
        ),
    ];
    let stored = |id: String| {
        let db = db.clone();
        async move {
            let id: Uuid = id.parse().unwrap();
            sqlx::query_scalar::<_, Option<String>>(
                r#"SELECT "Path" FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(id))
            .fetch_one(db.pool())
            .await
            .unwrap()
        }
    };
    let served = |router: axum::Router, id: String| {
        let token = token.clone();
        async move {
            let (status, item) = send(
                &router,
                &token,
                "GET",
                &format!("/Users/{admin}/Items/{id}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{item}");
            item["Path"].as_str().unwrap().to_owned()
        }
    };
    let program_data = |root: &Path| config(root).data_dir;
    for (id, token_path, under) in &made {
        assert_eq!(stored(id.clone()).await.as_deref(), Some(*token_path));
        let path = program_data(&before).join(under);
        assert!(path.is_dir(), "{} is made", path.display());
        assert_eq!(
            served(router.clone(), id.clone()).await,
            path.to_str().unwrap()
        );
    }

    // The folders move; the database stays where the handle opened it.
    for folder in ["data", "root"] {
        std::fs::rename(
            program_data(&before).join(folder),
            program_data(&after).join(folder),
        )
        .unwrap();
    }
    let router = start(&db, &after).await;
    for (id, token_path, under) in &made {
        assert_eq!(
            stored(id.clone()).await.as_deref(),
            Some(*token_path),
            "nothing rewrites it"
        );
        let path = program_data(&after).join(under);
        assert_eq!(
            served(router.clone(), id.clone()).await,
            path.to_str().unwrap()
        );
    }
}
