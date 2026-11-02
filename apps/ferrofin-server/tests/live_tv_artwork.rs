//! Live TV artwork over the real router and composition root, from the
//! playlist and guide files to the image routes.
//!
//! A tuner's or guide's artwork is kept only as an `http` or `https` URL. A
//! channel or programme row that is not a URL, stored before that rule, is
//! served only from the internal metadata directory, where the metadata editor
//! and an adopted Jellyfin database keep theirs.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "live-tv-artwork-admin-pw";
/// What the local image files hold.
const LOCAL_ART: &[u8] = b"local artwork";
/// `BaseItemImageInfos.ImageType` of a Primary image.
const PRIMARY: i32 = 0;
/// `BaseItemImageInfos.ImageType` of a Backdrop image.
const BACKDROP: i32 = 2;
/// How long a lineup may take to appear after its tuner is saved.
const LINEUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// A booted server and an admin session.
struct Fixture {
    wired: ferrofin_server::state::WiredApp,
    temp: tempfile::TempDir,
    db: Database,
    router: axum::Router,
    token: String,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-live-tv-artwork".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
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
    Fixture {
        wired,
        temp,
        db,
        router,
        token,
    }
}

impl Fixture {
    /// Configures the playlist `m3u` and the guide `xmltv` and refreshes the
    /// guide, through the routes the dashboard uses and the task's own call.
    async fn refresh_guide(&self, m3u: &str, xmltv: &str) {
        let playlist = self.temp.path().join("playlist.m3u");
        let guide = self.temp.path().join("guide.xml");
        std::fs::write(&playlist, m3u).expect("playlist");
        std::fs::write(&guide, xmltv).expect("guide");
        let (status, body) = self
            .admin(
                "POST",
                "/LiveTv/TunerHosts",
                Some(json!({ "Type": "m3u", "Url": playlist.to_string_lossy() })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        let (status, body) = self
            .admin(
                "POST",
                "/LiveTv/ListingProviders",
                Some(json!({
                    "Type": "xmltv",
                    "Path": guide.to_string_lossy(),
                    "EnableAllTuners": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        // Each save queued a refresh, and a later refresh supersedes an earlier
        // one's write. Once a lineup is listed they have all started, so the
        // refresh below is the newest and has finished when it returns.
        let deadline = std::time::Instant::now() + LINEUP_DEADLINE;
        while self.listed("/LiveTv/Channels").await.is_empty() {
            assert!(std::time::Instant::now() < deadline, "no lineup appeared");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        self.wired
            .state
            .live_tv
            .as_ref()
            .expect("live tv is wired")
            .refresh_guide()
            .await
            .expect("guide refresh");
    }

    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Bytes) {
        call(&self.router, method, uri, Some(&self.token), body).await
    }

    /// `(id, name)` of everything `route` lists.
    async fn listed(&self, route: &str) -> Vec<(String, String)> {
        let page = json_of(self.admin("GET", route, None).await);
        page["Items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| {
                (
                    item["Id"].as_str().expect("id").to_owned(),
                    item["Name"].as_str().expect("name").to_owned(),
                )
            })
            .collect()
    }

    /// Every stored image path, sorted.
    async fn stored_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItemImageInfos""#)
                .fetch_all(self.db.pool())
                .await
                .expect("image rows");
        paths.sort();
        paths
    }

    /// Makes `rows` the item's only image rows, in that order, as an earlier
    /// version may have stored them.
    async fn set_rows(&self, item: &str, rows: &[(i32, &str)]) {
        let id = guid_to_db(Uuid::parse_str(item).expect("id"));
        sqlx::query(r#"DELETE FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#)
            .bind(&id)
            .execute(self.db.pool())
            .await
            .expect("clear rows");
        for (image_type, path) in rows {
            sqlx::query(
                r#"INSERT INTO "BaseItemImageInfos" ("Id","ItemId","ImageType","Path","Width","Height")
                   VALUES (?1,?2,?3,?4,0,0)"#,
            )
            .bind(guid_to_db(Uuid::new_v4()))
            .bind(&id)
            .bind(image_type)
            .bind(path)
            .execute(self.db.pool())
            .await
            .expect("insert row");
        }
    }

    /// What `GET /Items/{item}/Images` lists.
    async fn image_infos(&self, item: &str) -> Vec<Value> {
        json_of(
            self.admin("GET", &format!("/Items/{item}/Images"), None)
                .await,
        )
        .as_array()
        .expect("image infos")
        .clone()
    }

    /// Asserts both request methods of the item's `image` route answer `404`.
    async fn assert_not_served(&self, item: &str, image: &str, context: &str) {
        let uri = format!("/Items/{item}/Images/{image}");
        for method in ["GET", "HEAD"] {
            let (status, body) = call(&self.router, method, &uri, None, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {image}: {context}");
            assert_ne!(&body[..], LOCAL_ART, "{method} {image}: {context}");
        }
    }
}

/// Tomorrow as XMLTV writes it, inside the refresh's window whatever time of day
/// the suite runs at.
fn guide_day() -> String {
    (chrono::Utc::now() + chrono::Duration::days(1))
        .format("%Y%m%d")
        .to_string()
}

/// One airing on `channel`, with `art` as its icon, still and backdrop.
fn programme(channel: &str, art: &str) -> String {
    let day = guide_day();
    format!(
        "<programme start=\"{day}060000 +0000\" stop=\"{day}070000 +0000\" channel=\"{channel}\">\
         <title>{channel} show</title><icon src=\"{art}\"/>\
         <image type=\"still\">{art}</image><image type=\"backdrop\">{art}</image></programme>"
    )
}

/// A value that is no URL, as a playlist and as a guide give it, on channels
/// and on their programmes, leaves no image and serves none.
#[tokio::test]
async fn a_value_that_is_no_url_leaves_no_image() {
    let fixture = fixture().await;
    let file = fixture.temp.path().join("art.png");
    std::fs::write(&file, LOCAL_ART).expect("art");
    let path = file.to_string_lossy();
    let values = [
        path.to_string(),
        format!("file://{path}"),
        format!("file:{path}"),
        "art.png".to_owned(),
    ];

    let mut m3u = String::from("#EXTM3U\n");
    let mut channels = String::new();
    let mut programmes = String::new();
    for (index, value) in values.iter().enumerate() {
        let (tuner, guide) = (format!("tuner{index}"), format!("guide{index}"));
        write!(
            m3u,
            "#EXTINF:-1 tvg-id=\"{tuner}\" tvg-chno=\"1{index}\" tvg-logo=\"{value}\",{tuner}\n\
             http://tuner.invalid/{tuner}\n\
             #EXTINF:-1 tvg-id=\"{guide}\" tvg-chno=\"2{index}\",{guide}\n\
             http://tuner.invalid/{guide}\n"
        )
        .expect("playlist");
        write!(
            channels,
            "<channel id=\"{tuner}\"><display-name>{tuner}</display-name></channel>\
             <channel id=\"{guide}\"><display-name>{guide}</display-name>\
             <icon src=\"{value}\"/></channel>"
        )
        .expect("guide");
        programmes.push_str(&programme(&tuner, value));
    }
    channels.push_str("<channel id=\"url\"><display-name>url</display-name></channel>");
    m3u.push_str(
        "#EXTINF:-1 tvg-id=\"url\" tvg-chno=\"30\" tvg-logo=\"http://art.invalid/logo.png\",url\n\
         http://tuner.invalid/url\n",
    );
    programmes.push_str(&programme("url", "HTTPS://art.invalid/show.png"));
    fixture
        .refresh_guide(&m3u, &format!("<tv>{channels}{programmes}</tv>"))
        .await;

    assert_eq!(
        fixture.stored_paths().await,
        [
            "HTTPS://art.invalid/show.png",
            "HTTPS://art.invalid/show.png",
            "HTTPS://art.invalid/show.png",
            "http://art.invalid/logo.png",
        ],
        "only the URLs became image rows"
    );
    let channels = fixture.listed("/LiveTv/Channels").await;
    let programmes = fixture.listed("/LiveTv/Programs").await;
    assert_eq!((channels.len(), programmes.len()), (9, 5));
    for (id, name) in channels.iter().chain(&programmes) {
        if !name.starts_with("url") {
            for image in ["Primary", "Thumb", "Backdrop"] {
                fixture.assert_not_served(id, image, name).await;
            }
        }
    }
}

/// A guide logo that is no URL does not replace the playlist's, which is one.
#[tokio::test]
async fn a_guide_value_that_is_no_url_keeps_the_playlists_logo() {
    let fixture = fixture().await;
    fixture
        .refresh_guide(
            "#EXTM3U\n\
             #EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\" tvg-logo=\"http://art.invalid/one.png\",one\n\
             http://tuner.invalid/one\n",
            "<tv><channel id=\"one\"><display-name>one</display-name>\
             <icon src=\"/art/one.png\"/></channel></tv>",
        )
        .await;
    assert_eq!(fixture.stored_paths().await, ["http://art.invalid/one.png"]);
}

/// A channel's or programme's stored row that is no URL is served from the
/// internal metadata directory and from nowhere else, judged by the path it
/// resolves to. A row outside the directory is not listed either, and keeps its
/// place in the numbering; one in the directory whose file is gone stays listed,
/// so the image editor can delete it.
#[tokio::test]
async fn a_stored_local_row_is_served_only_from_the_metadata_directory() {
    let fixture = fixture().await;
    fixture
        .refresh_guide(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",one\nhttp://tuner.invalid/one\n",
            &format!(
                "<tv><channel id=\"one\"><display-name>one</display-name></channel>{}</tv>",
                programme("one", "http://art.invalid/a.png")
            ),
        )
        .await;
    let (channel, _) = fixture.listed("/LiveTv/Channels").await.remove(0);
    let (show, _) = fixture.listed("/LiveTv/Programs").await.remove(0);

    let metadata = PathBuf::from(
        fixture
            .wired
            .state
            .config
            .application_paths()
            .internal_metadata_path(),
    );
    let owned = metadata.join("livetv").join("adopted").join("poster.png");
    std::fs::create_dir_all(owned.parent().expect("parent")).expect("dir");
    std::fs::write(&owned, LOCAL_ART).expect("art");
    let beside = metadata.parent().expect("parent").join("art.png");
    std::fs::write(&beside, LOCAL_ART).expect("art");
    let linked = owned.with_file_name("linked.png");
    std::os::unix::fs::symlink(&beside, &linked).expect("symlink");
    let resolves_beside = metadata
        .join("livetv")
        .join("..")
        .join("..")
        .join("art.png");
    let relative = PathBuf::from("art.png");
    let gone_outside = beside.with_file_name("gone.png");
    let gone_inside = owned.with_file_name("gone.png");

    for item in [&channel, &show] {
        for refused in [&beside, &resolves_beside, &linked, &relative, &gone_outside] {
            let refused = refused.to_string_lossy();
            fixture.set_rows(item, &[(PRIMARY, &refused)]).await;
            fixture.assert_not_served(item, "Primary", &refused).await;
            assert!(
                fixture.image_infos(item).await.is_empty(),
                "{refused} is listed"
            );
        }
        for served in [
            owned.to_string_lossy().into_owned(),
            "%MetadataPath%/livetv/adopted/poster.png".to_owned(),
        ] {
            fixture.set_rows(item, &[(PRIMARY, &served)]).await;
            let uri = format!("/Items/{item}/Images/Primary");
            let (status, body) = call(&fixture.router, "GET", &uri, None, None).await;
            assert_eq!((status, &body[..]), (StatusCode::OK, LOCAL_ART), "{served}");
            assert_eq!(fixture.image_infos(item).await.len(), 1, "{served}");
        }

        let gone = gone_inside.to_string_lossy();
        fixture.set_rows(item, &[(PRIMARY, &gone)]).await;
        fixture.assert_not_served(item, "Primary", &gone).await;
        let infos = fixture.image_infos(item).await;
        assert_eq!(infos.len(), 1, "{gone} stays listed");
        assert_eq!(infos[0]["Size"], 0);

        // Nothing is opened past a segment that does not exist, wherever the
        // path would lead if it did.
        let past_missing = metadata
            .join("livetv")
            .join("missing")
            .join("..")
            .join("..")
            .join("..")
            .join("art.png");
        let past_missing = past_missing.to_string_lossy();
        fixture.set_rows(item, &[(PRIMARY, &past_missing)]).await;
        fixture
            .assert_not_served(item, "Primary", &past_missing)
            .await;

        // Backdrops are numbered as the routes number them, listed or not.
        let (refused, served) = (beside.to_string_lossy(), owned.to_string_lossy());
        fixture
            .set_rows(item, &[(BACKDROP, &refused), (BACKDROP, &served)])
            .await;
        let infos = fixture.image_infos(item).await;
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert_eq!(infos[0]["ImageIndex"], 1);
        fixture
            .assert_not_served(item, "Backdrop/0", &refused)
            .await;
        let uri = format!("/Items/{item}/Images/Backdrop/1");
        let (status, body) = call(&fixture.router, "GET", &uri, None, None).await;
        assert_eq!((status, &body[..]), (StatusCode::OK, LOCAL_ART));
    }
}

/// A logo set in the metadata editor is a local file, and it is served.
#[tokio::test]
async fn a_logo_set_in_the_metadata_editor_is_served() {
    let fixture = fixture().await;
    fixture
        .refresh_guide(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",one\nhttp://tuner.invalid/one\n",
            "<tv></tv>",
        )
        .await;
    let (channel, _) = fixture.listed("/LiveTv/Channels").await.remove(0);
    let response = fixture
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{channel}/Images/Primary"))
                .header(header::AUTHORIZATION, authorization(Some(&fixture.token)))
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from("bG9jYWwgYXJ0d29yaw=="))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let stored = fixture.stored_paths().await;
    assert!(
        stored
            .iter()
            .all(|path| Path::new(path).is_absolute() || path.starts_with("%MetadataPath%")),
        "{stored:?}"
    );

    let uri = format!("/Items/{channel}/Images/Primary");
    let (status, body) = call(&fixture.router, "GET", &uri, None, None).await;
    assert_eq!((status, &body[..]), (StatusCode::OK, LOCAL_ART));
}

fn authorization(token: Option<&str>) -> String {
    let ident = r#"Client="test", Device="d", DeviceId="live-tv-artwork", Version="1""#;
    match token {
        Some(t) => format!(r#"MediaBrowser Token="{t}", {ident}"#),
        None => format!("MediaBrowser {ident}"),
    }
}

/// Sends one request as `token` and returns status + raw body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, authorization(token));
    let req = match body {
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string())),
        None => req.body(Body::empty()),
    };
    let resp = router
        .clone()
        .oneshot(req.expect("request"))
        .await
        .expect("response");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, body)
}

fn json_of((status, body): (StatusCode, Bytes)) -> Value {
    assert!(status.is_success(), "{status}: {body:?}");
    serde_json::from_slice(&body).expect("json")
}
