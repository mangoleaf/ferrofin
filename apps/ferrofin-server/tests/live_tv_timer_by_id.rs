//! `GET` and `DELETE /LiveTv/Timers/{timerId}` over the real composition root
//! and a real session WebSocket (gl-843), and the timer and series-timer DTOs
//! as every read builds them (gl-1745, gl-853).
//!
//! Upstream's `GetTimer` returns `LiveTvManager.GetTimer` unchecked, so an id
//! it does not list answers `200 null`; `CancelTimer` throws
//! `ResourceNotFoundException` for one, a `404`, and only a cancel that changed
//! the timer's status raises `TimerCancelled`.
//!
//! `LiveTvDtoService.GetTimerInfoDto` adds the guide programme
//! (`ProgramInfo`) and the channel's name and logo tag on each read, and
//! `FillImages` gives series timers, the new-timer defaults and a series
//! timer's programmes the artwork of the library series of that name.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::entities::ImageType;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::options::ItemImageInfo;
use ferrofin_traits::persistence::ItemPersistenceService as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const ADMIN_USER: &str = "admin";
const UNKNOWN: &str = "00000000000000000000000000000001";

/// A client side of the session WebSocket.
type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A served server whose administrator holds a socket.
struct Fixture {
    wired: WiredApp,
    db: Database,
    temp: tempfile::TempDir,
    router: axum::Router,
    token: String,
    socket: ClientSocket,
}

/// Boots the composition root with a passwordless administrator, signs them in
/// and opens their socket.
async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-live-tv-timer-by-id".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: String::new(),
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
    let token = login(&router).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let (socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/socket?ApiKey={token}"))
            .await
            .expect("socket upgrades");
    Fixture {
        wired,
        db,
        temp,
        router,
        token,
        socket,
    }
}

/// Signs the passwordless administrator in and returns its token.
async fn login(router: &axum::Router) -> String {
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    r#"MediaBrowser Client="test", Device="d", DeviceId="timer-by-id", Version="1""#,
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "Username": ADMIN_USER, "Pw": "" }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), StatusCode::OK, "the administrator signs in");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: Value = serde_json::from_slice(&bytes).expect("json");
    v["AccessToken"].as_str().expect("token").to_owned()
}

impl Fixture {
    /// Sends one request as the administrator and returns status + raw body.
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
        let token = &self.token;
        let req = Request::builder().method(method).uri(uri).header(
            header::AUTHORIZATION,
            format!(
                r#"MediaBrowser Token="{token}", Client="test", Device="d", DeviceId="timer-by-id", Version="1""#
            ),
        );
        let req = match body {
            Some(v) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&v).expect("body")))
                .expect("request"),
            None => req.body(Body::empty()).expect("request"),
        };
        let res = self.router.clone().oneshot(req).await.expect("response");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The `Data.Id` of each `TimerCancelled` frame the socket receives, up to
    /// and including the first naming `sentinel` (10 s cap).
    async fn cancelled_until(&mut self, sentinel: &str) -> Vec<String> {
        use futures_util::StreamExt as _;
        let mut ids = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = self
                    .socket
                    .next()
                    .await
                    .expect("a frame")
                    .expect("a readable frame");
                let Ok(text) = frame.to_text() else { continue };
                let Ok(json) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                if json["MessageType"] != "TimerCancelled" {
                    continue;
                }
                let id = json["Data"]["Id"].as_str().unwrap_or_default().to_owned();
                let done = id == sentinel;
                ids.push(id);
                if done {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no TimerCancelled naming {sentinel} in time: {ids:?}"));
        ids
    }
}

impl Fixture {
    /// Creates a timer from `body` and returns the id the server minted.
    async fn create(&self, mut body: Value) -> String {
        body["StartDate"] = json!("2099-01-01T00:00:00Z");
        body["EndDate"] = json!("2099-01-01T01:00:00Z");
        let name = body["Name"].as_str().expect("a name").to_owned();
        let (status, text) = self.call("POST", "/LiveTv/Timers", Some(body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
        let (_, list) = self.call("GET", "/LiveTv/Timers", None).await;
        let list: Value = serde_json::from_str(&list).expect("json");
        list["Items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|t| t["Name"] == name.as_str())
            .and_then(|t| t["Id"].as_str())
            .expect("the timer")
            .to_owned()
    }
}

/// An unknown id: `GET` answers `200 null` and `DELETE` a `404` with no push.
/// A Completed timer, which `GetTimers` leaves out, is unknown too. An already
/// cancelled timer of a series is cancelled again with no push. A
/// live timer is found whatever the case of its id, is cancelled with one
/// push, and is then unknown in turn.
#[tokio::test]
async fn an_unknown_timer_is_200_null_to_get_and_404_to_delete_with_no_push() {
    let mut f = fixture().await;

    let uri = format!("/LiveTv/Timers/{UNKNOWN}");
    assert_eq!(
        f.call("GET", &uri, None).await,
        (StatusCode::OK, "null".to_owned())
    );
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A series keeps its cancelled timers (`AddOrUpdate`), so this one stays.
    let cancelled = f
        .create(json!({ "Name": "cancelled", "Status": "Cancelled", "SeriesTimerId": "s" }))
        .await;
    let uri = format!("/LiveTv/Timers/{cancelled}");
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = f.call("GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("json")["Status"],
        "Cancelled"
    );

    // A recording that finished: only the capture marks a timer Completed.
    let done = f.create(json!({ "Name": "done" })).await;
    sqlx::query(
        r#"UPDATE "FerrofinLiveTvTimers" SET "Status" = 'Completed',
           "Data" = json_set("Data", '$.Status', 'Completed') WHERE "Id" = ?1"#,
    )
    .bind(&done)
    .execute(f.db.pool())
    .await
    .expect("complete the timer");
    let uri = format!("/LiveTv/Timers/{done}");
    assert_eq!(
        f.call("GET", &uri, None).await,
        (StatusCode::OK, "null".to_owned())
    );
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let id = f.create(json!({ "Name": "probe" })).await;

    let upper = format!("/LiveTv/Timers/{}", id.to_ascii_uppercase());
    let (status, body) = f.call("GET", &upper, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).expect("json")["Id"],
        id.as_str()
    );
    let (status, body) = f.call("DELETE", &upper, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        f.cancelled_until(&id).await,
        std::slice::from_ref(&id),
        "one push, for the live timer, by its own id: none for the unknown, the \
         Completed or the already cancelled one"
    );

    let uri = format!("/LiveTv/Timers/{id}");
    assert_eq!(
        f.call("GET", &uri, None).await,
        (StatusCode::OK, "null".to_owned())
    );
    let (status, body) = f.call("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// How long a lineup may take to appear after its tuner is saved.
const LINEUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
/// The six fields `FillImages` writes.
const PARENT_ART: [&str; 6] = [
    "ParentThumbItemId",
    "ParentThumbImageTag",
    "ParentBackdropItemId",
    "ParentBackdropImageTags",
    "ParentPrimaryImageItemId",
    "ParentPrimaryImageTag",
];

impl Fixture {
    /// `GET uri` as the administrator, as JSON.
    async fn get(&self, uri: &str) -> Value {
        let (status, body) = self.call("GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        serde_json::from_str(&body).expect("json")
    }

    /// Configures a playlist whose channel `one` has a logo and a guide airing
    /// `one show` on it tomorrow (an episode, with an icon), refreshes the
    /// guide, and returns the channel's and the programme's ids.
    async fn guide(&self) -> (String, String) {
        let day = (chrono::Utc::now() + chrono::Duration::days(1))
            .format("%Y%m%d")
            .to_string();
        let playlist = self.temp.path().join("playlist.m3u");
        let guide = self.temp.path().join("guide.xml");
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\" \
             tvg-logo=\"http://art.invalid/logo.png\",one\nhttp://tuner.invalid/one\n",
        )
        .expect("playlist");
        std::fs::write(
            &guide,
            format!(
                "<tv><channel id=\"one\"><display-name>one</display-name></channel>\
                 <programme start=\"{day}060000 +0000\" stop=\"{day}070000 +0000\" \
                 channel=\"one\"><title>one show</title>\
                 <episode-num system=\"xmltv_ns\">0.1.</episode-num>\
                 <icon src=\"http://art.invalid/show.png\"/></programme></tv>"
            ),
        )
        .expect("guide");
        let (status, body) = self
            .call(
                "POST",
                "/LiveTv/TunerHosts",
                Some(json!({ "Type": "m3u", "Url": playlist.to_string_lossy() })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = self
            .call(
                "POST",
                "/LiveTv/ListingProviders",
                Some(json!({
                    "Type": "xmltv",
                    "Path": guide.to_string_lossy(),
                    "EnableAllTuners": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // Each save queued a refresh, and a later one supersedes an earlier
        // one's write: once the lineup is listed they have all started, so the
        // refresh below is the newest and has finished when it returns.
        let deadline = std::time::Instant::now() + LINEUP_DEADLINE;
        while self.get("/LiveTv/Channels").await["Items"]
            .as_array()
            .is_none_or(Vec::is_empty)
        {
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
        let id = |page: Value| page["Items"][0]["Id"].as_str().expect("an id").to_owned();
        (
            id(self.get("/LiveTv/Channels").await),
            id(self.get("/LiveTv/Programs").await),
        )
    }

    /// Stores a library series named `name` with a Thumb, a Backdrop and a
    /// Primary image, and returns its id.
    async fn library_series(&self, name: &str) -> String {
        let id = Uuid::new_v4();
        let persistence = ferrofin_core::FerrofinItemPersistenceService::new(self.db.clone());
        persistence
            .save_items(&[BaseItemEntity {
                id: guid_to_db(id),
                type_: "MediaBrowser.Controller.Entities.TV.Series".into(),
                name: Some(name.into()),
                sort_name: Some(name.into()),
                clean_name: Some(name.into()),
                is_folder: true,
                ..Default::default()
            }])
            .await
            .expect("save the series");
        // A day apart, so each image has a tag of its own.
        let images: Vec<ItemImageInfo> =
            [ImageType::Thumb, ImageType::Backdrop, ImageType::Primary]
                .into_iter()
                .zip(1..)
                .map(|(image_type, day)| ItemImageInfo {
                    path: format!("http://art.invalid/series-{day}.png"),
                    image_type,
                    date_modified: chrono::DateTime::from_timestamp(
                        1_767_225_600 + day * 86_400,
                        0,
                    )
                    .expect("a date"),
                    width: 2,
                    height: 2,
                    ..ItemImageInfo::default()
                })
                .collect();
        persistence
            .save_item_images(id, &images)
            .await
            .expect("store the images");
        id.simple().to_string()
    }
}

/// Each read builds the timer from the guide as `GetTimerInfoDto` does: the
/// programme as `ProgramInfo` (with the timer's id and status while it is
/// live), and the channel's current name and logo tag (gl-1745).
#[tokio::test]
async fn a_timer_carries_its_programme_and_channel_as_they_are_now() {
    let f = fixture().await;
    let (channel, program) = f.guide().await;
    let (status, body) = f
        .call(
            "POST",
            "/LiveTv/Timers",
            Some(json!({ "ProgramId": program, "ChannelId": channel })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let listed = f.get("/LiveTv/Timers").await["Items"][0].clone();
    let id = listed["Id"].as_str().expect("the timer").to_owned();
    let logo = f.get(&format!("/LiveTv/Channels/{channel}")).await["ImageTags"]["Primary"].clone();
    assert!(logo.is_string(), "the channel has a logo");

    let by_id = f.get(&format!("/LiveTv/Timers/{id}")).await;
    for timer in [&listed, &by_id] {
        let info = &timer["ProgramInfo"];
        assert_eq!(info["Id"], program.as_str(), "{timer}");
        assert_eq!(info["Name"], "one show", "{timer}");
        assert_eq!(info["Type"], "Program", "{timer}");
        assert_eq!(info["TimerId"], id.as_str(), "{timer}");
        assert_eq!(info["Status"], "New", "{timer}");
        assert!(info.get("SeriesTimerId").is_none(), "{timer}");
        for key in PARENT_ART {
            assert!(
                info.get(key).is_none(),
                "no series timer, no artwork: {timer}"
            );
        }
        assert_eq!(timer["ChannelName"], "one", "{timer}");
        assert_eq!(timer["ChannelPrimaryImageTag"], logo, "{timer}");
    }

    // The channel's name is read with the timer, not copied into it.
    sqlx::query(r#"UPDATE "FerrofinLiveTvChannels" SET "Name" = 'renamed'"#)
        .execute(f.db.pool())
        .await
        .expect("rename the channel");
    assert_eq!(
        f.get(&format!("/LiveTv/Timers/{id}")).await["ChannelName"],
        "renamed"
    );
}

/// `FillImages`: the defaults for a programme, a series timer and the
/// timers it schedules carry the library series' Thumb and Backdrop, and a
/// Primary: the series' for the first two, the guide programme's for a
/// timer's `ProgramInfo` (gl-853). A cancelled timer's programme keeps its
/// series timer but loses the timer's id and status.
#[tokio::test]
async fn series_timers_and_their_programmes_carry_the_library_series_artwork() {
    let f = fixture().await;
    let (_, program) = f.guide().await;
    let series = f.library_series("one show").await;
    let art = f.get(&format!("/Items/{series}")).await;
    let thumb = art["ImageTags"]["Thumb"].clone();
    let backdrop = art["BackdropImageTags"].clone();
    let primary = art["ImageTags"]["Primary"].clone();
    assert!(thumb.is_string() && primary.is_string() && thumb != primary);
    let program_primary =
        f.get(&format!("/LiveTv/Programs/{program}")).await["ImageTags"]["Primary"].clone();
    assert!(program_primary.is_string(), "the programme has its icon");
    let series_art = |dto: &Value| {
        assert_eq!(dto["ParentThumbItemId"], series.as_str(), "{dto}");
        assert_eq!(dto["ParentThumbImageTag"], thumb, "{dto}");
        assert_eq!(dto["ParentBackdropItemId"], series.as_str(), "{dto}");
        assert_eq!(dto["ParentBackdropImageTags"], backdrop, "{dto}");
    };

    let defaults = f
        .get(&format!("/LiveTv/Timers/Defaults?programId={program}"))
        .await;
    series_art(&defaults);
    assert_eq!(defaults["ParentPrimaryImageItemId"], series.as_str());
    assert_eq!(defaults["ParentPrimaryImageTag"], primary);

    // A client posts the body back with values only the read may set.
    let mut posted = defaults;
    posted["ChannelPrimaryImageTag"] = json!("stale");
    posted["ImageTags"] = json!({ "Primary": "stale" });
    let (status, body) = f.call("POST", "/LiveTv/SeriesTimers", Some(posted)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let listed = f.get("/LiveTv/SeriesTimers").await["Items"][0].clone();
    let series_timer = listed["Id"].as_str().expect("the series timer").to_owned();
    // `GetSeriesTimer` matches the listed ids ignoring case.
    let upper = series_timer.to_ascii_uppercase();
    let by_id = f.get(&format!("/LiveTv/SeriesTimers/{upper}")).await;
    for dto in [&listed, &by_id] {
        series_art(dto);
        assert_eq!(dto["ParentPrimaryImageItemId"], series.as_str(), "{dto}");
        assert_eq!(dto["ParentPrimaryImageTag"], primary, "{dto}");
        assert_eq!(dto["ChannelName"], "one", "{dto}");
        assert!(dto.get("ChannelPrimaryImageTag").is_none(), "{dto}");
        assert_eq!(dto["ImageTags"], json!({}), "{dto}");
    }

    let timer = f
        .get(&format!("/LiveTv/Timers?seriesTimerId={series_timer}"))
        .await["Items"][0]
        .clone();
    let id = timer["Id"].as_str().expect("a scheduled airing").to_owned();
    let info = &timer["ProgramInfo"];
    series_art(info);
    assert_eq!(
        info["ParentPrimaryImageItemId"],
        program.as_str(),
        "{timer}"
    );
    assert_eq!(info["ParentPrimaryImageTag"], program_primary, "{timer}");
    assert_eq!(info["SeriesTimerId"], series_timer.as_str(), "{timer}");
    assert_eq!(info["TimerId"], id.as_str(), "{timer}");

    let (status, body) = f
        .call("DELETE", &format!("/LiveTv/Timers/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let cancelled = f.get(&format!("/LiveTv/Timers/{id}")).await;
    let info = &cancelled["ProgramInfo"];
    assert_eq!(cancelled["Status"], "Cancelled", "{cancelled}");
    assert_eq!(info["SeriesTimerId"], series_timer.as_str(), "{cancelled}");
    assert!(info.get("TimerId").is_none(), "{cancelled}");
    assert!(info.get("Status").is_none(), "{cancelled}");
}

/// Without a programme the defaults have no name, and upstream's `FillImages`
/// then matches any library series. Whether to port that waits on the owner
/// (gl-1790), so until then the defaults carry no artwork.
#[tokio::test]
async fn the_defaults_without_a_programme_carry_no_artwork() {
    let f = fixture().await;
    f.library_series("one show").await;
    let defaults = f.get("/LiveTv/Timers/Defaults").await;
    for key in PARENT_ART {
        assert!(defaults.get(key).is_none(), "{defaults}");
    }
}
