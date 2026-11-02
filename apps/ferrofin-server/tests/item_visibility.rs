//! Per-user item visibility over the real router and composition root.
//! Fixtures use disposable databases and files; policy changes go through HTTP.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "visibility-admin-pw";

/// A booted server and the temp dir holding its data and the media it
/// scanned.
struct Harness {
    wired: WiredApp,
    db: Database,
    temp: tempfile::TempDir,
}

impl Harness {
    fn path(&self, relative: &str) -> PathBuf {
        self.temp.path().join(relative)
    }
}

/// Writes a small file (and its directories).
fn touch(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, contents).expect("write");
}

async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-item-visibility".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    for (path, contents) in [
        ("movies/Alpha (1999)/Alpha (1999).mkv", String::new()),
        ("movies/Epsilon (2003)/Epsilon (2003).mkv", String::new()),
        ("more/Gamma (2001)/Gamma (2001).mkv", String::new()),
        ("more/Delta (2002)/Delta (2002).mkv", String::new()),
    ] {
        touch(&temp.path().join(path), &contents);
    }
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
    Harness { wired, db, temp }
}

/// Sends one request as `token` (on its own device) and returns status + body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let device = token.map_or("anonymous", |t| &t[..8.min(t.len())]);
    let ident = format!(r#"Client="test", Device="d", DeviceId="{device}", Version="1""#);
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Authenticates `name` and returns its token and user id.
async fn login(router: &axum::Router, name: &str, password: &str) -> (String, String) {
    let ident =
        format!(r#"MediaBrowser Client="test", Device="d", DeviceId="login-{name}", Version="1""#);
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, ident)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "Username": name, "Pw": password }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), StatusCode::OK, "{name} authenticates");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: Value = serde_json::from_slice(&bytes).expect("json");
    (
        v["AccessToken"].as_str().expect("token").to_owned(),
        v["User"]["Id"].as_str().expect("id").to_owned(),
    )
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

/// The library items by `Path`, as the administrator sees them.
async fn items_by_path(
    router: &axum::Router,
    token: &str,
) -> std::collections::HashMap<String, String> {
    let (status, items) = call(
        router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie&fields=Path",
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{items}");
    items["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .filter_map(|i| Some((i["Path"].as_str()?.to_owned(), i["Id"].as_str()?.to_owned())))
        .collect()
}

/// Creates an account and returns its id.
async fn create_user(router: &axum::Router, admin: &str, name: &str, password: &str) -> String {
    let (status, created) = call(
        router,
        "POST",
        "/Users/New",
        Some(admin),
        Some(json!({ "Name": name, "Password": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    created["Id"].as_str().expect("id").to_owned()
}

struct Fixture {
    h: Harness,
    router: axum::Router,
    admin: String,
    viewer: String,
    viewer_id: String,
    allowed: String,
    hidden: String,
}

async fn fixture() -> Fixture {
    let h = boot().await;
    let router = ferrofin_api::create_router(h.wired.state.clone());
    let (admin, _) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    for (name, dir) in [("Allowed", "movies"), ("Hidden", "more")] {
        let path = encode(&h.path(dir).to_string_lossy());
        let (status, _) = call(&router, "POST", &format!(
            "/Library/VirtualFolders?name={name}&collectionType=movies&paths={path}&refreshLibrary=false"
        ), Some(&admin), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert!(
        h.wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .unwrap()
    );
    let (_, folders) = call(
        &router,
        "GET",
        "/Library/VirtualFolders",
        Some(&admin),
        None,
    )
    .await;
    let folder = |name| {
        folders
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["Name"] == name)
            .unwrap()["ItemId"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let allowed_library = folder("Allowed");
    let items = items_by_path(&router, &admin).await;
    let allowed = items[&h
        .path("movies/Alpha (1999)/Alpha (1999).mkv")
        .to_string_lossy()
        .into_owned()]
        .clone();
    let hidden = items[&h
        .path("more/Gamma (2001)/Gamma (2001).mkv")
        .to_string_lossy()
        .into_owned()]
        .clone();
    let viewer_id = create_user(&router, &admin, "viewer", "viewer-pw").await;
    let (_, user) = call(
        &router,
        "GET",
        &format!("/Users/{viewer_id}"),
        Some(&admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["EnableAllFolders"] = json!(false);
    policy["EnabledFolders"] = json!([allowed_library]);
    policy["EnableContentDeletion"] = json!(true);
    // The operation permission must pass before these tests reach visibility.
    policy["EnableSubtitleManagement"] = json!(true);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        Some(&admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (viewer, _) = login(&router, "viewer", "viewer-pw").await;
    Fixture {
        h,
        router,
        admin,
        viewer,
        viewer_id,
        allowed,
        hidden,
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn hidden_items_return_404_before_direct_reads_and_writes() {
    let f = fixture().await;
    let id = &f.hidden;
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Items/{}", f.allowed),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    // Shared JSON binding still runs before the per-user visibility check:
    // quoted numbers bind with mixed-case keys, while padded integers are 400.
    for (item_id, bitrate, expected) in [
        (&f.allowed, "140000000", StatusCode::OK),
        (&f.hidden, "140000000", StatusCode::NOT_FOUND),
        (&f.allowed, " 140000000", StatusCode::BAD_REQUEST),
        (&f.hidden, " 140000000", StatusCode::BAD_REQUEST),
    ] {
        let uri = format!("/Items/{item_id}/PlaybackInfo");
        let (status, body) = call(
            &f.router,
            "POST",
            &uri,
            Some(&f.viewer),
            Some(json!({"mAxStReAmInGbItRaTe": bitrate})),
        )
        .await;
        assert_eq!(status, expected, "{uri}, bitrate {bitrate:?}: {body}");
    }
    for uri in [
        format!("/Items/{id}"),
        format!("/Users/{}/Items/{id}", f.viewer_id),
        format!("/Items/{id}/PlaybackInfo"),
        format!("/Items/{id}/Download"),
        format!("/Items/{id}/File"),
        format!("/Items/{id}/SpecialFeatures"),
        format!("/Items/{id}/LocalTrailers"),
        format!("/Items/{id}/Intros"),
        format!("/Items/{id}/Similar"),
        format!("/Shows/{id}/Similar"),
        format!("/Items/{id}/ThemeSongs"),
        format!("/Items/{id}/ThemeVideos"),
        format!("/UserItems/{id}/UserData"),
        format!("/Items/{id}/InstantMix"),
        format!("/Videos/{id}/AdditionalParts"),
        format!("/Audio/{id}/universal"),
        format!("/Items/{id}/RemoteImages"),
        format!("/Items/{id}/RemoteImages/Providers"),
        format!("/Items/{id}/RemoteSearch/Subtitles/en"),
        format!("/Videos/{id}/{id}/Subtitles/0/subtitles.m3u8?segmentLength=10"),
    ] {
        let (status, body) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    for (uri, body) in [
        (format!("/Items/{id}/PlaybackInfo"), json!({})),
        (format!("/UserFavoriteItems/{id}"), json!({})),
        (format!("/UserPlayedItems/{id}"), json!({})),
        (format!("/UserItems/{id}/Rating?likes=true"), json!({})),
        (
            format!("/UserItems/{id}/UserData"),
            json!({"IsFavorite": true, "Played": true}),
        ),
        (
            format!("/Videos/{id}/Subtitles"),
            json!({"Language":"en","Format":"srt","Data":"eA==","IsForced":false,"IsHearingImpaired":false}),
        ),
        (
            format!("/Items/{id}/RemoteSearch/Subtitles/unused"),
            json!({}),
        ),
    ] {
        let (status, response) = call(&f.router, "POST", &uri, Some(&f.viewer), Some(body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {response}");
    }
    let user_data =
        f.h.wired
            .state
            .user_data
            .get_user_data_dto(
                uuid::Uuid::parse_str(id).unwrap(),
                uuid::Uuid::parse_str(&f.viewer_id).unwrap(),
            )
            .await
            .unwrap();
    assert!(user_data.is_none_or(|data| !data.is_favorite && !data.played));
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Items/{id}?userId={}", f.viewer_id),
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Items/{id}/PlaybackInfo"),
            Some(&f.admin),
            Some(json!({"UserId": f.viewer_id, "DeviceProfile": {}})),
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "the posted target user scopes visibility before playback negotiation"
    );
    // Elevation does not bypass the administrator's own library restrictions.
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["IsAdministrator"] = json!(true);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(policy)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for (method, uri, body) in [
        ("GET", format!("/Items/{id}/ExternalIdInfos"), None),
        ("GET", format!("/Items/{id}/MetadataEditor"), None),
        (
            "POST",
            format!("/Items/{id}"),
            Some(json!({"Id": id, "Name": "Should not change"})),
        ),
        (
            "POST",
            format!("/Items/{id}/ContentType?contentType=movies"),
            None,
        ),
        ("POST", format!("/Items/{id}/Refresh"), None),
        (
            "POST",
            format!("/Items/RemoteSearch/Apply/{id}"),
            Some(json!({})),
        ),
        (
            "POST",
            format!(
                "/Items/{id}/RemoteImages/Download?type=Primary&imageUrl=http://127.0.0.1:9/no"
            ),
            None,
        ),
        ("DELETE", format!("/Videos/{id}/AlternateSources"), None),
        ("DELETE", format!("/Videos/{id}/Subtitles/0"), None),
    ] {
        let (status, response) = call(&f.router, method, &uri, Some(&f.viewer), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {response}");
    }
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Videos/MergeVersions?ids={},{}", f.allowed, f.hidden),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, item) = call(
        &f.router,
        "GET",
        &format!("/Items/{id}"),
        Some(&f.admin),
        None,
    )
    .await;
    assert_eq!(item["Name"], "Gamma (2001)");
    assert!(f.h.path("more/Gamma (2001)/Gamma (2001).mkv").exists());
}

async fn image_response(
    f: &Fixture,
    uri: &str,
    token: Option<&str>,
    head: bool,
    etag: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(if head { "HEAD" } else { "GET" })
        .uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!(r#"MediaBrowser Token="{token}", Client="test", Device="d", DeviceId="image-test", Version="1""#));
    }
    if let Some(etag) = etag {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    f.router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn hidden_images_and_indirect_media_are_checked_before_cached_responses() {
    let f = fixture().await;
    let id = uuid::Uuid::parse_str(&f.hidden).unwrap();
    let png: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 2,
        0, 0, 0, 253, 212, 154, 115, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0,
        68, 12, 16, 10, 0, 31, 238, 3, 253, 139, 95, 20, 212, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
        96, 130,
    ];
    f.h.wired
        .state
        .providers
        .save_image(
            id,
            png,
            "image/png",
            ferrofin_model::entities::ImageType::Primary,
            None,
        )
        .await
        .unwrap();
    let uri = format!("/Items/{id}/Images/Primary");
    let tagged_uri = format!("{uri}?tag=fixture");
    let response = image_response(&f, &tagged_uri, Some(&f.admin), false, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let image_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!image_bytes.is_empty());
    assert_eq!(
        image_response(&f, &tagged_uri, Some(&f.admin), false, Some(&etag))
            .await
            .status(),
        StatusCode::NOT_MODIFIED
    );
    // An administrator's warmed image/transform cache never authorizes another user.
    for uri in [
        tagged_uri,
        format!("{uri}/0?tag=fixture"),
        format!("{uri}/0/fixture/Png/100/100/0/0"),
        format!("{uri}?maxWidth=1&tag=fixture"),
    ] {
        assert_eq!(
            image_response(&f, &uri, Some(&f.admin), false, None)
                .await
                .status(),
            StatusCode::OK
        );
        for head in [false, true] {
            for etag in [None, Some(etag.as_str())] {
                assert_eq!(
                    image_response(&f, &uri, Some(&f.viewer), head, etag)
                        .await
                        .status(),
                    StatusCode::NOT_FOUND,
                    "{uri}"
                );
            }
        }
    }
    // Public routes retain an authenticated identity even when a default-route
    // schedule would deny it. Dropping it would bypass item visibility.
    let (_, viewer) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    let policy = viewer["Policy"].clone();
    let mut outside_schedule = policy.clone();
    outside_schedule["AccessSchedules"] = json!([{
        "Id": 0, "UserId": f.viewer_id,
        "DayOfWeek": "Everyday", "StartHour": 1, "EndHour": 0
    }]);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(outside_schedule),
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&f.router, "GET", "/Items", Some(&f.viewer), None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        image_response(&f, &uri, Some(&f.viewer), false, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(policy),
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    // Upstream permits user-less public images; retain that policy.
    assert_eq!(
        image_response(&f, &uri, None, false, None).await.status(),
        StatusCode::OK
    );
    for uri in [
        format!("/Items/{id}/Images"),
        format!("/Items/{id}/Ancestors"),
        format!("/MediaSegments/{id}"),
        format!("/Videos/{id}/{id}/Attachments/0"),
    ] {
        assert_eq!(
            call(&f.router, "GET", &uri, Some(&f.viewer), None).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    f.h.wired
        .state
        .trickplay
        .save_trickplay_info(&ferrofin_db::entities::playback::TrickplayInfoEntity {
            item_id: ferrofin_db::store::guid_to_db(id),
            width: 320,
            height: 180,
            interval: 1000,
            bandwidth: 100,
            thumbnail_count: 1,
            tile_width: 1,
            tile_height: 1,
        })
        .await
        .unwrap();
    let tile =
        f.h.wired
            .state
            .trickplay
            .get_trickplay_tile_path(id, 320, 0)
            .await
            .unwrap()
            .unwrap();
    touch(Path::new(&tile), "synthetic tile");
    for uri in [
        format!("/Videos/{id}/Trickplay/320/0.jpg"),
        format!(
            "/Videos/{}/Trickplay/320/0.jpg?mediaSourceId={id}",
            f.allowed
        ),
    ] {
        assert_eq!(
            call(&f.router, "GET", &uri, Some(&f.admin), None).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f.router, "GET", &uri, Some(&f.viewer), None).await.0,
            StatusCode::NOT_FOUND
        );
    }
    // The tile playlist is intentionally unscoped in the upstream controller.
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Videos/{id}/Trickplay/320/tiles.m3u8"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["IsAdministrator"] = json!(true);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(policy)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for (method, uri) in [
        ("DELETE", format!("/Items/{id}/Images/Primary")),
        ("DELETE", format!("/Items/{id}/Images/Primary/0")),
        ("POST", format!("/Items/{id}/Images/Primary")),
        ("POST", format!("/Items/{id}/Images/Primary/0")),
        (
            "POST",
            format!("/Items/{id}/Images/Backdrop/0/Index?newIndex=1"),
        ),
    ] {
        assert_eq!(
            call(&f.router, method, &uri, Some(&f.viewer), None).await.0,
            StatusCode::NOT_FOUND,
            "{method} {uri}"
        );
    }
    assert_eq!(
        image_response(&f, &uri, Some(&f.admin), false, None)
            .await
            .status(),
        StatusCode::OK
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn deletion_checks_visibility_before_permission_and_stops_in_input_order() {
    let f = fixture().await;
    let id = &f.hidden;
    for uri in [format!("/Items/{id}"), format!("/Items?ids={id}")] {
        assert_eq!(
            call(&f.router, "DELETE", &uri, Some(&f.viewer), None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Items/{id}"),
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let items = items_by_path(&f.router, &f.admin).await;
    let later = &items[&f
        .h
        .path("movies/Epsilon (2003)/Epsilon (2003).mkv")
        .to_string_lossy()
        .into_owned()];
    let uri = format!("/Items?ids={},{id},{later}", f.allowed);
    assert_eq!(
        call(&f.router, "DELETE", &uri, Some(&f.viewer), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Items/{}", f.allowed),
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for retained in [id, later] {
        assert_eq!(
            call(
                &f.router,
                "GET",
                &format!("/Items/{retained}"),
                Some(&f.admin),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    // A delete removes the item's files (`DeleteFileLocation = true`): the
    // allowed movie's folder goes; the hidden one's and the one after the
    // stop stay on disk.
    assert!(!f.h.path("movies/Alpha (1999)").exists());
    assert!(f.h.path("more/Gamma (2001)/Gamma (2001).mkv").exists());
    assert!(
        f.h.path("movies/Epsilon (2003)/Epsilon (2003).mkv")
            .exists()
    );
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["EnableContentDeletion"] = json!(false);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(policy)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &f.router,
            "DELETE",
            &format!("/Items/{later}"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &f.router,
            "DELETE",
            &format!("/Items/{id}"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, playlist) = call(
        &f.router,
        "POST",
        "/Playlists",
        Some(&f.admin),
        Some(json!({"Name":"Private", "Ids":[later], "IsPublic":false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{playlist}");
    let playlist_id = playlist["Id"].as_str().unwrap();
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Items/{playlist_id}"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.router,
            "DELETE",
            &format!("/Items/{playlist_id}"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Items/{playlist_id}"),
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn parent_browse_and_userless_exceptions() {
    let f = fixture().await;
    let id = &f.hidden;
    let (_, folders) = call(
        &f.router,
        "GET",
        "/Library/VirtualFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let hidden_library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["Name"] == "Hidden")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    let parent_uri = format!("/Items?parentId={hidden_library}");
    assert_eq!(
        call(&f.router, "GET", &parent_uri, Some(&f.viewer), None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.router, "GET", &parent_uri, Some(&f.admin), None)
            .await
            .0,
        StatusCode::OK
    );
    // Divergence from upstream (gl-1090): `GetStreamingState` reads the item with
    // no user, so Jellyfin streams it. Here a known user gets the same library
    // check as on `/Download`.
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Videos/{id}/stream?Static=true"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.router,
            "POST",
            "/Auth/Keys?app=visibility-fixture",
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, keys) = call(&f.router, "GET", "/Auth/Keys", Some(&f.admin), None).await;
    let key = keys["Items"][0]["AccessToken"].as_str().unwrap();
    for uri in [
        format!("/Items/{id}/Download"),
        format!("/Items/{id}/PlaybackInfo"),
        format!("/Items/{id}/ThemeSongs"),
    ] {
        assert_eq!(
            call(&f.router, "GET", &uri, Some(key), None).await.0,
            StatusCode::OK,
            "{uri}"
        );
    }
    for uri in [
        format!("/Items/{id}?userId={}", f.viewer_id),
        format!("/Items/{id}/PlaybackInfo?userId={}", f.viewer_id),
    ] {
        assert_eq!(
            call(&f.router, "GET", &uri, Some(key), None).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    // Upstream explicitly exempts API keys from the parent IsVisible gate,
    // including keys specifying a target user for DTO preferences.
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("{parent_uri}&userId={}", f.viewer_id),
            Some(key),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn public_user_picker_filters_authenticated_devices_after_setup() {
    let h = boot().await;
    let router = ferrofin_api::create_router(h.wired.state.clone());
    let (admin, _) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let id = create_user(&router, &admin, "device-user", "pw").await;
    let (_, mut dto) = call(&router, "GET", &format!("/Users/{id}"), Some(&admin), None).await;
    dto["Policy"]["IsHidden"] = json!(false);
    dto["Policy"]["EnableAllDevices"] = json!(false);
    dto["Policy"]["EnabledDevices"] = json!([]);
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/Users/{id}/Policy"),
            Some(&admin),
            Some(dto["Policy"].clone())
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for setup in [false, true] {
        let (_, mut config) =
            call(&router, "GET", "/System/Configuration", Some(&admin), None).await;
        config["IsStartupWizardCompleted"] = json!(setup);
        assert_eq!(
            call(
                &router,
                "POST",
                "/System/Configuration",
                Some(&admin),
                Some(config)
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        for token in [None, Some(admin.as_str())] {
            let (status, users) = call(&router, "GET", "/Users/Public", token, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                users.as_array().unwrap().iter().any(|u| u["Id"] == id),
                !setup || token.is_none(),
                "setup={setup}, authenticated={}",
                token.is_some()
            );
        }
    }
    // Match the header device used by `call`, case-insensitively.
    dto["Policy"]["EnabledDevices"] = json!([admin[..8].to_uppercase()]);
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/Users/{id}/Policy"),
            Some(&admin),
            Some(dto["Policy"].clone())
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, users) = call(&router, "GET", "/Users/Public", Some(&admin), None).await;
    assert!(users.as_array().unwrap().iter().any(|u| u["Id"] == id));
}

/// Jellyfin 12.1 runs `DeviceAccessHost` after a policy save and after a
/// rename (`UserManager.UpdatePolicyAsync` and `RenameUser` raise
/// `OnUserUpdated`): every device of the user that the saved policy no longer
/// allows is logged out, and signing in from it again is a security error.
#[tokio::test]
async fn a_policy_save_or_a_rename_signs_out_disallowed_devices() {
    let h = boot().await;
    let router = ferrofin_api::create_router(h.wired.state.clone());
    let (admin, _) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let id = create_user(&router, &admin, "device-user", "pw").await;
    let (user, _) = login(&router, "device-user", "pw").await;
    let (_, dto) = call(&router, "GET", &format!("/Users/{id}"), Some(&admin), None).await;
    let open = dto["Policy"].clone();
    let mut restricted = open.clone();
    restricted["EnableAllDevices"] = json!(false);
    restricted["EnabledDevices"] = json!([]);
    let save = |policy: Value| {
        let router = router.clone();
        let admin = admin.clone();
        let id = id.clone();
        async move {
            call(
                &router,
                "POST",
                &format!("/Users/{id}/Policy"),
                Some(&admin),
                Some(policy),
            )
            .await
            .0
        }
    };
    assert_eq!(save(restricted.clone()).await, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&router, "GET", "/Users/Me", Some(&user), None).await.0,
        StatusCode::UNAUTHORIZED,
        "the policy save signs out the device it no longer allows"
    );
    assert_eq!(
        call(
            &router,
            "POST",
            "/Users/AuthenticateByName",
            None,
            Some(json!({"Username":"device-user", "Pw":"pw"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "a denied device is a security error, not bad credentials"
    );

    // The rename leg: a token on a device the stored policy refuses, the
    // policy written without the save's device check, is signed out by a
    // rename.
    assert_eq!(save(open).await, StatusCode::NO_CONTENT);
    let (user, _) = login(&router, "device-user", "pw").await;
    let policy: ferrofin_model::users::UserPolicy =
        serde_json::from_value(restricted).expect("policy");
    h.wired
        .state
        .users
        .update_policy(uuid::Uuid::parse_str(&id).expect("id"), &policy)
        .await
        .expect("policy saved without the device check");
    assert_eq!(
        call(&router, "GET", "/Users/Me", Some(&user), None).await.0,
        StatusCode::OK,
        "nothing has run the device check yet"
    );
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/Users?userId={id}"),
            Some(&admin),
            Some(json!({"Name":"renamed","Policy":null}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&router, "GET", "/Users/Me", Some(&user), None).await.0,
        StatusCode::UNAUTHORIZED,
        "the rename runs the device check"
    );
}

#[tokio::test]
async fn metadata_rating_edits_keep_browse_and_direct_access_consistent() {
    let f = fixture().await;
    let movies = items_by_path(&f.router, &f.admin).await;
    let id = movies
        .iter()
        .find(|(path, _)| path.contains("Alpha"))
        .unwrap()
        .1
        .clone();
    let (_, mut dto) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    dto["Policy"]["MaxParentalRating"] = json!(12);
    dto["Policy"]["MaxParentalSubRating"] = json!(0);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(dto["Policy"].clone())
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for (custom, official, country, expected) in [
        (Some("10"), "FSK-18", "US", true),
        (None, "FSK-18", "US", false),
        (None, "PG", "CA", true),
        (None, "PG", "AU", false),
        (None, "", "US", true),
    ] {
        assert_eq!(
            call(
                &f.router,
                "POST",
                &format!("/Items/{id}"),
                Some(&f.admin),
                Some(
                    json!({"Name":"Alpha", "CustomRating":custom, "OfficialRating":official,
                "PreferredMetadataCountryCode":country})
                )
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        let (status, listing) = call(
            &f.router,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie",
            Some(&f.viewer),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            listing["Items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["Id"] == id),
            expected,
            "{custom:?} {official} {country}"
        );
        assert_eq!(
            call(
                &f.router,
                "GET",
                &format!("/Items/{id}"),
                Some(&f.viewer),
                None
            )
            .await
            .0,
            if expected {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            }
        );
    }
}

#[tokio::test]
async fn saved_unrated_policy_filters_browse_and_numeric_enum_values_round_trip() {
    let f = fixture().await;
    let movies = items_by_path(&f.router, &f.admin).await;
    let id = movies
        .iter()
        .find(|(path, _)| path.contains("Alpha"))
        .unwrap()
        .1
        .clone();
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Items/{id}"),
            Some(&f.admin),
            Some(json!({"Name":"Alpha","OfficialRating":"Unrated","CustomRating":null}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, mut dto) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    for (blocked, visible) in [
        (json!([]), true),
        (json!(["Movie", 99]), false),
        (json!([]), true),
    ] {
        dto["Policy"]["BlockUnratedItems"] = blocked.clone();
        assert_eq!(
            call(
                &f.router,
                "POST",
                &format!("/Users/{}/Policy", f.viewer_id),
                Some(&f.admin),
                Some(dto["Policy"].clone())
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        let (_, saved) = call(
            &f.router,
            "GET",
            &format!("/Users/{}", f.viewer_id),
            Some(&f.admin),
            None,
        )
        .await;
        assert_eq!(saved["Policy"]["BlockUnratedItems"], blocked);
        let (status, listed) = call(
            &f.router,
            "GET",
            &format!("/Items?ids={id}"),
            Some(&f.viewer),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            listed["Items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["Id"] == id),
            visible
        );
        assert_eq!(
            call(
                &f.router,
                "GET",
                &format!("/Items/{id}"),
                Some(&f.viewer),
                None
            )
            .await
            .0,
            if visible {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            }
        );
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn saved_tags_filter_lists_counts_and_views_with_root_allow_exception() {
    let f = fixture().await;
    let (_, mut dto) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    dto["Policy"]["EnableAllFolders"] = json!(true);
    for (id, name, tags) in [
        (&f.allowed, "Tagged", json!(["Café"])),
        (&f.hidden, "Private", json!(["private"])),
    ] {
        assert_eq!(
            call(
                &f.router,
                "POST",
                &format!("/Items/{id}"),
                Some(&f.admin),
                Some(json!({"Name":name,"Tags":tags,"Genres":["Shared"]}))
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
    }
    for (blocked, allowed, count) in [
        (json!([]), json!([]), 4),
        (json!(["private"]), json!([]), 3),
        (json!([]), json!(["CAFE"]), 1),
        (json!(["café"]), json!(["CAFE"]), 0),
        (json!([]), json!([]), 4),
    ] {
        dto["Policy"]["BlockedTags"] = blocked;
        dto["Policy"]["AllowedTags"] = allowed;
        assert_eq!(
            call(
                &f.router,
                "POST",
                &format!("/Users/{}/Policy", f.viewer_id),
                Some(&f.admin),
                Some(dto["Policy"].clone())
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        let (status, listed) = call(
            &f.router,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie",
            Some(&f.viewer),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed["TotalRecordCount"], count);
        assert_eq!(
            listed["Items"].as_array().unwrap().len(),
            usize::try_from(count).unwrap()
        );
        let (status, counts) = call(&f.router, "GET", "/Items/Counts", Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(counts["MovieCount"], count);
        for path in ["/Items", "/UserViews?includeHidden=true"] {
            let (status, roots) = call(&f.router, "GET", path, Some(&f.viewer), None).await;
            assert_eq!(status, StatusCode::OK, "{path} {roots}");
            let names: Vec<_> = roots["Items"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|i| i["Name"].as_str())
                .collect();
            assert!(
                names.contains(&"Allowed") && names.contains(&"Hidden"),
                "{path} {names:?}"
            );
        }
    }
    let (_, folders) = call(
        &f.router,
        "GET",
        "/Library/VirtualFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let folder = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["Name"] == "Allowed")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap();
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Items/{folder}"),
            Some(&f.admin),
            Some(json!({"Name":"Allowed","Tags":["Private"]}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    dto["Policy"]["BlockedTags"] = json!(["private"]);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(dto["Policy"].clone())
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for path in ["/Items", "/UserViews?includeHidden=true"] {
        let (status, roots) = call(&f.router, "GET", path, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::OK, "{path} {roots}");
        let names: Vec<_> = roots["Items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["Name"].as_str())
            .collect();
        assert!(
            !names.contains(&"Allowed") && names.contains(&"Hidden"),
            "{path} {names:?}"
        );
    }
}

#[tokio::test]
async fn current_web_schedule_payload_saves_and_revokes_existing_requests() {
    let f = fixture().await;
    let (_, mut user) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    for (schedules, allowed) in [
        (json!([]), true),
        (
            json!([{"DayOfWeek":"Everyday","StartHour":"25","EndHour":"26"}]),
            false,
        ),
        (
            json!([{"DayOfWeek":"Everyday","StartHour":"0","EndHour":"24"}]),
            true,
        ),
        (json!([]), true),
    ] {
        // Current Web rebuilds these three fields from HTML data attributes;
        // its POST carries neither the database Id nor the associated UserId.
        user["Policy"]["AccessSchedules"] = schedules;
        let (status, saved) = call(
            &f.router,
            "POST",
            &format!("/Users/{}/Policy", f.viewer_id),
            Some(&f.admin),
            Some(user["Policy"].clone()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{saved}");
        let (_, saved) = call(
            &f.router,
            "GET",
            &format!("/Users/{}", f.viewer_id),
            Some(&f.admin),
            None,
        )
        .await;
        let read = saved["Policy"]["AccessSchedules"].as_array().unwrap();
        assert_eq!(
            read.len(),
            user["Policy"]["AccessSchedules"].as_array().unwrap().len()
        );
        for row in read {
            assert!(row["Id"].as_i64().unwrap() > 0);
            assert_eq!(row["UserId"], f.viewer_id);
        }
        assert_eq!(
            call(&f.router, "GET", "/Users/Me", Some(&f.viewer), None)
                .await
                .0,
            if allowed {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(
            call(
                &f.router,
                "GET",
                &format!("/Users/{}", f.viewer_id),
                Some(&f.viewer),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
    }
}

/// The `Id`s of an item array.
fn item_ids(items: &Value) -> Vec<uuid::Uuid> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| uuid::Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn latest_and_grouping_options_keep_to_the_libraries_the_user_can_open() {
    let f = fixture().await;
    let allowed = uuid::Uuid::parse_str(&f.allowed).unwrap();
    let hidden = uuid::Uuid::parse_str(&f.hidden).unwrap();
    // Grouped (the movies arm of `GetLatestItemList`) and ungrouped (the plain query).
    for route in [
        "/Items/Latest?limit=100",
        "/Items/Latest?limit=100&groupItems=false",
    ] {
        let (status, latest) = call(&f.router, "GET", route, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        let ids = item_ids(&latest);
        assert!(ids.contains(&allowed), "{route}: {ids:?}");
        assert!(!ids.contains(&hidden), "{route}: {ids:?}");
        let (_, latest) = call(&f.router, "GET", route, Some(&f.admin), None).await;
        assert!(item_ids(&latest).contains(&hidden), "{route}: admin");
    }

    let names = |options: &Value| -> Vec<String> {
        options
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["Name"].as_str().unwrap().to_owned())
            .collect()
    };
    let viewer_id = &f.viewer_id;
    // An administrator reading the viewer's list gets the viewer's list.
    for (route, token, expected) in [
        (
            "/UserViews/GroupingOptions".to_owned(),
            &f.viewer,
            vec!["Allowed"],
        ),
        (
            format!("/Users/{viewer_id}/GroupingOptions"),
            &f.viewer,
            vec!["Allowed"],
        ),
        (
            format!("/UserViews/GroupingOptions?userId={viewer_id}"),
            &f.admin,
            vec!["Allowed"],
        ),
        (
            "/UserViews/GroupingOptions".to_owned(),
            &f.admin,
            vec!["Allowed", "Hidden"],
        ),
    ] {
        let (status, options) = call(&f.router, "GET", &route, Some(token), None).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert_eq!(names(&options), expected, "{route}");
    }

    // No library left is an empty answer, never an unscoped query.
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{viewer_id}"),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["EnabledFolders"] = json!([]);
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        Some(&f.admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let viewer = Some(f.viewer.as_str());
    let (_, latest) = call(&f.router, "GET", "/Items/Latest?limit=100", viewer, None).await;
    assert!(item_ids(&latest).is_empty(), "{latest}");
    let (_, options) = call(&f.router, "GET", "/UserViews/GroupingOptions", viewer, None).await;
    assert!(names(&options).is_empty(), "{options}");
}

#[tokio::test]
async fn grouping_options_sort_by_culture_not_by_code_point() {
    let f = fixture().await;
    std::fs::create_dir_all(f.h.path("lower")).unwrap();
    let path = encode(&f.h.path("lower").to_string_lossy());
    let (status, _) = call(&f.router, "POST", &format!(
        "/Library/VirtualFolders?name=aardvark&collectionType=movies&paths={path}&refreshLibrary=false"
    ), Some(&f.admin), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        f.h.wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .unwrap()
    );
    let (status, options) = call(
        &f.router,
        "GET",
        "/UserViews/GroupingOptions?culture=en-US",
        Some(&f.admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = options
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["Name"].as_str().unwrap())
        .collect();
    // Byte order would put the lower-case name last.
    assert_eq!(names, ["aardvark", "Allowed", "Hidden"]);
}

/// Every item a scope can name inside libraries the viewer cannot open.
struct HiddenScopes {
    /// The TV, music and home-video libraries.
    libraries: Vec<String>,
    /// The TV library.
    library: String,
    series: String,
    season: String,
    album: String,
    folder: String,
}

/// The id of the first item of `kind`, as the administrator sees it.
async fn first_of_kind(f: &Fixture, kind: &str) -> String {
    let uri = format!("/Items?recursive=true&includeItemTypes={kind}");
    let (status, items) = call(&f.router, "GET", &uri, Some(&f.admin), None).await;
    assert_eq!(status, StatusCode::OK, "{items}");
    items["Items"][0]["Id"]
        .as_str()
        .unwrap_or_else(|| panic!("no {kind}: {items}"))
        .to_owned()
}

/// Adds a TV, a music and a home-video library. The viewer's policy names
/// only the fixture's Allowed library, so all three are closed to them.
async fn hidden_scopes(f: &Fixture) -> HiddenScopes {
    for path in [
        "shows/Show/Season 1/Show S01E01.mkv",
        "tunes/Band/Album/01 Song.mp3",
        "home/Trip/Day/clip.mkv",
    ] {
        touch(&f.h.path(path), "");
    }
    for (name, kind, dir) in [
        ("Shows", "tvshows", "shows"),
        ("Tunes", "music", "tunes"),
        ("Home", "homevideos", "home"),
    ] {
        let path = encode(&f.h.path(dir).to_string_lossy());
        let uri = format!(
            "/Library/VirtualFolders?name={name}&collectionType={kind}&paths={path}&refreshLibrary=false"
        );
        let (status, _) = call(&f.router, "POST", &uri, Some(&f.admin), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert!(
        f.h.wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .unwrap()
    );
    let (_, folders) = call(
        &f.router,
        "GET",
        "/Library/VirtualFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let libraries: Vec<String> = ["Shows", "Tunes", "Home"]
        .iter()
        .map(|name| {
            folders
                .as_array()
                .unwrap()
                .iter()
                .find(|folder| folder["Name"] == *name)
                .unwrap()["ItemId"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    HiddenScopes {
        library: libraries[0].clone(),
        libraries,
        series: first_of_kind(f, "Series").await,
        season: first_of_kind(f, "Season").await,
        album: first_of_kind(f, "MusicAlbum").await,
        folder: first_of_kind(f, "Folder").await,
    }
}

/// Every route that takes an item as its scope, for one id.
fn scope_routes(id: &str, viewer_id: &str) -> Vec<String> {
    let mut routes: Vec<String> = [
        "/Items",
        "/UserItems/Resume",
        "/Trailers",
        "/Items/Latest",
        "/Items/Filters",
        "/Items/Filters2",
        "/Artists",
        "/Artists/AlbumArtists",
        "/Genres",
        "/MusicGenres",
        "/Studios",
        "/Persons",
        "/Years",
        "/Movies/Recommendations",
        "/Shows/NextUp",
        "/Shows/Upcoming",
    ]
    .iter()
    .map(|route| format!("{route}?parentId={id}"))
    .collect();
    routes.extend([
        format!("/Items?parentId={id}&recursive=true"),
        // A library's Collections tab, and its Playlists equivalent.
        format!("/Items?parentId={id}&includeItemTypes=BoxSet"),
        format!("/Items?parentId={id}&includeItemTypes=Playlist"),
        format!("/Users/{viewer_id}/Items?parentId={id}"),
        format!("/Users/{viewer_id}/Items/Latest?parentId={id}"),
        format!("/Search/Hints?searchTerm=s&parentId={id}"),
        format!("/Persons?appearsInItemId={id}"),
    ]);
    routes
}

// Divergence (owner decisions 2026-10-31 and 2026-11-01, gl-1091, gl-1136): see
// `require_scope_visible`.
#[tokio::test]
async fn a_scope_inside_a_library_the_user_cannot_open_lists_nothing() {
    let f = fixture().await;
    let s = hidden_scopes(&f).await;
    let mut open = Vec::new();
    for id in [&s.library, &s.series, &s.season, &s.album, &s.folder] {
        let mut routes = scope_routes(id, &f.viewer_id);
        routes.push(format!("/Shows/NextUp?seriesId={id}"));
        for route in routes {
            let (status, _) = call(&f.router, "GET", &route, Some(&f.viewer), None).await;
            if status != StatusCode::FORBIDDEN {
                open.push(format!("{status} {route}"));
            }
        }
    }
    assert!(open.is_empty(), "{open:#?}");
    let both = format!("/Items?ids={}&parentId={}", f.hidden, s.library);
    let (status, _) = call(&f.router, "GET", &both, Some(&f.viewer), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the scope answers first");

    // An administrator can open them all, and an API key is not a user.
    assert_eq!(
        call(
            &f.router,
            "POST",
            "/Auth/Keys?app=scope-fixture",
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, keys) = call(&f.router, "GET", "/Auth/Keys", Some(&f.admin), None).await;
    let key = keys["Items"][0]["AccessToken"].as_str().unwrap();
    let series = format!("/Items?parentId={}", s.series);
    for token in [f.admin.as_str(), key] {
        let (status, _) = call(&f.router, "GET", &series, Some(token), None).await;
        assert_eq!(status, StatusCode::OK);
    }
    let for_viewer = format!("{series}&userId={}", f.viewer_id);
    let (status, _) = call(&f.router, "GET", &for_viewer, Some(key), None).await;
    assert_eq!(status, StatusCode::OK, "an API key naming a user is exempt");
    // An administrator reading as the viewer gets the viewer's answer.
    let (status, _) = call(&f.router, "GET", &for_viewer, Some(&f.admin), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Opening the library opens everything inside it.
    let viewer_id = &f.viewer_id;
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{viewer_id}"),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["EnabledFolders"]
        .as_array_mut()
        .unwrap()
        .extend(s.libraries.iter().map(|library| json!(library)));
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        Some(&f.admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for id in [&s.library, &s.series, &s.season, &s.album, &s.folder] {
        let mut routes = scope_routes(id, viewer_id);
        routes.push(format!("/Shows/NextUp?seriesId={id}"));
        for route in routes {
            let (status, body) = call(&f.router, "GET", &route, Some(&f.viewer), None).await;
            assert_eq!(status, StatusCode::OK, "{route}: {body}");
        }
    }
}

#[tokio::test]
async fn a_scope_the_user_can_open_answers_on_every_route() {
    let f = fixture().await;
    let (_, folders) = call(
        &f.router,
        "GET",
        "/Library/VirtualFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let library = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Allowed")
        .unwrap()["ItemId"]
        .as_str()
        .unwrap()
        .to_owned();
    for id in [&library, &f.allowed] {
        for route in scope_routes(id, &f.viewer_id) {
            let (status, body) = call(&f.router, "GET", &route, Some(&f.viewer), None).await;
            assert_eq!(status, StatusCode::OK, "{route}: {body}");
        }
    }
    // The playlists folder belongs to no library, so a limit on libraries
    // leaves it open.
    let (_, media_folders) = call(
        &f.router,
        "GET",
        "/Library/MediaFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let playlists = media_folders["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Playlists")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    let route = format!("/Items?parentId={playlists}&includeItemTypes=Playlist");
    let (status, body) = call(&f.router, "GET", &route, Some(&f.viewer), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_scope_naming_no_item_answers_as_it_did() {
    let f = fixture().await;
    let missing = uuid::Uuid::from_u128(0xDEAD).simple().to_string();
    for token in [&f.viewer, &f.admin] {
        for (route, expected) in [
            (
                format!("/Items?parentId={missing}"),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("/Genres?parentId={missing}"),
                StatusCode::BAD_REQUEST,
            ),
            (format!("/Items/Latest?parentId={missing}"), StatusCode::OK),
            (format!("/Shows/NextUp?seriesId={missing}"), StatusCode::OK),
            (format!("/Items?ids={missing}"), StatusCode::OK),
            // The id filter comes last: a request it would empty is still refused.
            (
                format!("/Items?ids={missing}&parentId={missing}"),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("/Items?ids={missing}&filters=IsPlayed,IsUnplayed"),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (status, body) = call(&f.router, "GET", &route, Some(token), None).await;
            assert_eq!(status, expected, "{route}: {body}");
        }
    }
}

/// Saves a tuner and a guide that airs one episode of each of `titles` and a
/// film tomorrow, then refreshes the guide.
async fn air_series(f: &Fixture, titles: &[&str]) {
    use std::fmt::Write as _;
    let (playlist, guide) = (f.h.path("playlist.m3u"), f.h.path("guide.xml"));
    let day = (chrono::Utc::now() + chrono::Duration::days(1)).format("%Y%m%d");
    let mut programmes = String::new();
    for (hour, title) in titles.iter().enumerate() {
        // A channel's airing is identified by its start, so each gets its own.
        let stop = hour + 1;
        write!(
            programmes,
            "<programme start=\"{day}{hour:02}0000 +0000\" stop=\"{day}{stop:02}0000 +0000\" \
             channel=\"one\"><title>{title}</title><sub-title>Pilot</sub-title></programme>"
        )
        .expect("guide");
    }
    let (start, stop) = (titles.len(), titles.len() + 1);
    write!(
        programmes,
        "<programme start=\"{day}{start:02}0000 +0000\" stop=\"{day}{stop:02}0000 +0000\" \
         channel=\"one\"><title>Film</title></programme>"
    )
    .expect("guide");
    touch(
        &playlist,
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",one\nhttp://tuner.invalid/one\n",
    );
    touch(
        &guide,
        &format!(
            "<tv><channel id=\"one\"><display-name>one</display-name></channel>{programmes}</tv>"
        ),
    );
    let (status, body) = call(
        &f.router,
        "POST",
        "/LiveTv/TunerHosts",
        Some(&f.admin),
        Some(json!({ "Type": "m3u", "Url": playlist.to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &f.router,
        "POST",
        "/LiveTv/ListingProviders",
        Some(&f.admin),
        Some(json!({
            "Type": "xmltv",
            "Path": guide.to_string_lossy(),
            "EnableAllTuners": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Each save queued a refresh, and a later refresh supersedes an earlier
    // one's write. Once a lineup is listed they have all started, so the
    // refresh below is the newest and has finished when it returns.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, channels) = call(&f.router, "GET", "/LiveTv/Channels", Some(&f.admin), None).await;
        if channels["TotalRecordCount"].as_i64() > Some(0) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no lineup appeared");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    f.h.wired
        .state
        .live_tv
        .as_ref()
        .expect("live tv is wired")
        .refresh_guide()
        .await
        .expect("guide refresh");
}

/// Mints the fixture's only API key and returns it.
async fn only_api_key(f: &Fixture) -> String {
    let uri = "/Auth/Keys?app=guide-fixture";
    let (status, _) = call(&f.router, "POST", uri, Some(&f.admin), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, keys) = call(&f.router, "GET", "/Auth/Keys", Some(&f.admin), None).await;
    keys["Items"][0]["AccessToken"]
        .as_str()
        .expect("AccessToken")
        .to_owned()
}

/// The programme titles a guide request answers, sorted.
async fn guide_titles(
    f: &Fixture,
    method: &str,
    body: Option<Value>,
    query: &str,
    token: &str,
) -> Vec<String> {
    let uri = format!("/LiveTv/Programs{query}");
    let (status, page) = call(&f.router, method, &uri, Some(token), body).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {page}");
    let mut titles: Vec<String> = page["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .map(|item| item["Name"].as_str().expect("Name").to_owned())
        .collect();
    titles.sort();
    titles
}

// Divergence (owner decision 2026-10-31, gl-1091; gl-1113, gl-1130): see
// `query_programs_inner`.
#[tokio::test]
async fn a_series_the_user_cannot_open_names_no_guide_search() {
    let f = fixture().await;
    let s = hidden_scopes(&f).await;
    air_series(&f, &["Other", "Show"]).await;
    let series = &s.series;
    let viewer_id = &f.viewer_id;
    let (everything, only_show) = (["Other", "Show"], ["Show"]);

    assert_eq!(
        guide_titles(&f, "GET", None, "", &f.viewer).await,
        ["Film", "Other", "Show"]
    );

    // The series and an id naming nothing answer alike, so the answer says
    // nothing about the series: every series airing, and no film.
    let closed = format!("?librarySeriesId={series}");
    let missing = format!(
        "?librarySeriesId={}",
        uuid::Uuid::from_u128(0xDEAD).simple()
    );
    for query in [&closed, &missing] {
        let titles = guide_titles(&f, "GET", None, query, &f.viewer).await;
        assert_eq!(titles, everything, "{query}");
    }
    let page = async |query: &str| {
        let uri = format!("/LiveTv/Programs{query}&limit=1");
        call(&f.router, "GET", &uri, Some(&f.viewer), None).await.1
    };
    assert_eq!(page(&closed).await, page(&missing).await);
    assert_eq!(page(&closed).await["TotalRecordCount"], 2);
    let body = json!({ "LibrarySeriesId": series, "UserId": viewer_id });
    assert_eq!(
        guide_titles(&f, "POST", Some(body.clone()), "", &f.viewer).await,
        everything
    );
    // A body naming no user still reads the series as the signed-in caller.
    let no_user = json!({ "LibrarySeriesId": series });
    assert_eq!(
        guide_titles(&f, "POST", Some(no_user.clone()), "", &f.viewer).await,
        everything
    );
    // An administrator reading as the viewer gets the viewer's answer.
    let as_viewer = format!("{closed}&userId={viewer_id}");
    assert_eq!(
        guide_titles(&f, "GET", None, &as_viewer, &f.admin).await,
        everything
    );

    // An administrator can open it.
    assert_eq!(
        guide_titles(&f, "GET", None, &closed, &f.admin).await,
        only_show
    );
    assert_eq!(
        guide_titles(
            &f,
            "POST",
            Some(json!({ "LibrarySeriesId": series })),
            "",
            &f.admin
        )
        .await,
        only_show
    );
    // An API key has no user, so it reads the series as upstream does.
    let key = only_api_key(&f).await;
    assert_eq!(
        guide_titles(&f, "POST", Some(no_user.clone()), "", &key).await,
        only_show
    );

    // Opening the library narrows the guide for the viewer too.
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{viewer_id}"),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["EnabledFolders"]
        .as_array_mut()
        .unwrap()
        .extend(s.libraries.iter().map(|library| json!(library)));
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        Some(&f.admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        guide_titles(&f, "GET", None, &closed, &f.viewer).await,
        only_show
    );
    assert_eq!(
        guide_titles(&f, "POST", Some(body), "", &f.viewer).await,
        only_show
    );
    assert_eq!(
        guide_titles(&f, "POST", Some(no_user), "", &f.viewer).await,
        only_show
    );
}

#[tokio::test]
async fn explicit_ids_keep_to_what_the_user_can_open() {
    let f = fixture().await;
    let (allowed, hidden) = (&f.allowed, &f.hidden);
    let total = |page: &Value| page["TotalRecordCount"].as_i64().unwrap();
    // An empty id list means no filter, so this must not become the whole
    // library. `ids_the_user_may_not_open_never_reach_the_query` holds `/Trailers`
    // to the same, which this fixture cannot: it has no trailers.
    let uri = format!("/Items?ids={hidden}");
    let (status, page) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(item_ids(&page["Items"]).is_empty(), "{page}");
    assert_eq!(total(&page), 0);
    let both = format!("/Items?ids={hidden},{allowed}");
    let (_, page) = call(&f.router, "GET", &both, Some(&f.viewer), None).await;
    assert_eq!(
        item_ids(&page["Items"]),
        [uuid::Uuid::parse_str(allowed).unwrap()]
    );
    assert_eq!(total(&page), 1);
    let (_, page) = call(&f.router, "GET", &both, Some(&f.admin), None).await;
    assert_eq!(item_ids(&page["Items"]).len(), 2);
    // An API key is exempt, even when it names the viewer.
    assert_eq!(
        call(
            &f.router,
            "POST",
            "/Auth/Keys?app=ids-fixture",
            Some(&f.admin),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (_, keys) = call(&f.router, "GET", "/Auth/Keys", Some(&f.admin), None).await;
    let key = keys["Items"][0]["AccessToken"].as_str().unwrap();
    let as_viewer = format!("/Items?ids={hidden}&userId={}", f.viewer_id);
    let (_, page) = call(&f.router, "GET", &as_viewer, Some(key), None).await;
    assert_eq!(
        item_ids(&page["Items"]),
        [uuid::Uuid::parse_str(hidden).unwrap()]
    );
}

/// A playlist's listing leaves out a member of a library its caller was not
/// granted, and pages and counts what is left.
#[tokio::test]
async fn a_playlist_listing_leaves_out_members_of_hidden_libraries() {
    let f = fixture().await;
    let (status, created) = call(
        &f.router,
        "POST",
        "/Playlists",
        Some(&f.viewer),
        Some(json!({
            "Name": "mixed",
            "Ids": [f.allowed],
            "UserId": f.viewer_id,
            "MediaType": "Video",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let playlist = created["Id"].as_str().expect("id").to_owned();
    let (status, _) = call(
        &f.router,
        "POST",
        &format!(
            "/Playlists/{playlist}/Items?ids={}&userId={}",
            f.hidden, f.viewer_id
        ),
        Some(&f.viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let uri = format!("/Playlists/{playlist}/Items?userId={}", f.viewer_id);
    let (status, items) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
    assert_eq!(status, StatusCode::OK, "{items}");
    assert_eq!(
        item_ids(&items["Items"]),
        [uuid::Uuid::parse_str(&f.allowed).expect("id")],
        "{items}"
    );
    assert_eq!(items["TotalRecordCount"], 1, "{items}");
}

/// A playlist from two libraries, shared for editing with a viewer who can open
/// one of them: `[Gamma (hidden), Alpha, Delta (hidden), Epsilon]`.
struct SharedPlaylist {
    id: String,
    gamma: uuid::Uuid,
    alpha: uuid::Uuid,
    delta: uuid::Uuid,
    epsilon: uuid::Uuid,
}

impl SharedPlaylist {
    async fn new(f: &Fixture) -> Self {
        let items = items_by_path(&f.router, &f.admin).await;
        let id_of = |relative: &str| {
            let path = f.h.path(relative).to_string_lossy().into_owned();
            uuid::Uuid::parse_str(&items[&path]).expect("id")
        };
        let gamma = id_of("more/Gamma (2001)/Gamma (2001).mkv");
        let alpha = id_of("movies/Alpha (1999)/Alpha (1999).mkv");
        let delta = id_of("more/Delta (2002)/Delta (2002).mkv");
        let epsilon = id_of("movies/Epsilon (2003)/Epsilon (2003).mkv");
        let (status, created) = call(
            &f.router,
            "POST",
            "/Playlists",
            Some(&f.admin),
            Some(json!({
                "Name": "two libraries",
                "Ids": [gamma, alpha, delta, epsilon],
                "MediaType": "Video",
                "Users": [{ "UserId": f.viewer_id, "CanEdit": true }],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        Self {
            id: created["Id"].as_str().expect("id").to_owned(),
            gamma,
            alpha,
            delta,
            epsilon,
        }
    }

    /// The members `token` is given by `GET /Playlists/{id}/Items`.
    async fn listed(&self, f: &Fixture, token: &str) -> Vec<uuid::Uuid> {
        let uri = format!("/Playlists/{}/Items", self.id);
        let (status, page) = call(&f.router, "GET", &uri, Some(token), None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        item_ids(&page["Items"])
    }

    /// Every stored member, in order: the administrator can open them all.
    async fn stored(&self, f: &Fixture) -> Vec<uuid::Uuid> {
        self.listed(f, &f.admin).await
    }

    /// `POST /Playlists/{id}/Items/{member}/Move/{index}` as the viewer.
    async fn move_to(&self, f: &Fixture, member: uuid::Uuid, index: usize) {
        let uri = format!(
            "/Playlists/{}/Items/{}/Move/{index}",
            self.id,
            member.simple()
        );
        let (status, _) = call(&f.router, "POST", &uri, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}

/// The filter comes before the paging, so a page is full and the total is what
/// the caller can see, and `/Items?ParentId=` agrees. `ItemIds` keeps every
/// member, and so does the playlist when reads and an add have been through it.
#[tokio::test]
async fn a_shared_playlist_pages_what_its_reader_can_open_and_keeps_every_member() {
    let f = fixture().await;
    let p = SharedPlaylist::new(&f).await;
    let all = [p.gamma, p.alpha, p.delta, p.epsilon];
    let open = [p.alpha, p.epsilon];
    let page = |query: &str| format!("/Playlists/{}/Items{query}", p.id);

    let (_, first) = call(&f.router, "GET", &page("?limit=1"), Some(&f.viewer), None).await;
    assert_eq!(item_ids(&first["Items"]), [p.alpha], "{first}");
    assert_eq!(first["TotalRecordCount"], 2, "{first}");
    let (_, second) = call(
        &f.router,
        "GET",
        &page("?startIndex=1&limit=1"),
        Some(&f.viewer),
        None,
    )
    .await;
    assert_eq!(item_ids(&second["Items"]), [p.epsilon], "{second}");
    assert_eq!(second["TotalRecordCount"], 2, "{second}");
    let (_, past) = call(
        &f.router,
        "GET",
        &page("?startIndex=2"),
        Some(&f.viewer),
        None,
    )
    .await;
    assert!(item_ids(&past["Items"]).is_empty(), "{past}");
    assert_eq!(past["TotalRecordCount"], 2, "{past}");

    let browse = format!("/Items?ParentId={}&userId={}", p.id, f.viewer_id);
    let (_, browsed) = call(&f.router, "GET", &browse, Some(&f.viewer), None).await;
    assert_eq!(item_ids(&browsed["Items"]), open, "{browsed}");
    assert_eq!(browsed["TotalRecordCount"], 2, "{browsed}");

    let (_, playlist) = call(
        &f.router,
        "GET",
        &format!("/Playlists/{}", p.id),
        Some(&f.viewer),
        None,
    )
    .await;
    assert_eq!(item_ids_of(&playlist["ItemIds"]), all, "{playlist}");
    assert_eq!(p.stored(&f).await, all);

    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Playlists/{}/Items?ids={}", p.id, p.delta),
        Some(&f.viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(p.listed(&f, &f.viewer).await, open, "an add hides it too");
    assert_eq!(
        p.stored(&f).await,
        [p.gamma, p.alpha, p.delta, p.epsilon, p.delta],
        "an add still stores a member its caller cannot open"
    );
}

/// `ids` as a bare array of ids.
fn item_ids_of(ids: &Value) -> Vec<uuid::Uuid> {
    ids.as_array()
        .expect("ids")
        .iter()
        .map(|id| uuid::Uuid::parse_str(id.as_str().expect("id")).expect("id"))
        .collect()
}

/// The disabled-library edge the `GET /Playlists/{itemId}/Items` row records: a
/// member of a disabled library stays in the playlist's listing, which takes the
/// check `/Items?ids=` uses, and leaves `/Items?ParentId=`, which takes
/// `Folder.GetLinkedChildren(user)`'s rule, for the administrator and the viewer
/// alike.
#[tokio::test]
async fn members_of_a_disabled_library_stay_listed_and_leave_the_browse() {
    /// The members `token` is given by `/Items?ParentId=<playlist>`.
    async fn browsed(f: &Fixture, p: &SharedPlaylist, token: &str) -> Vec<uuid::Uuid> {
        let uri = format!("/Items?ParentId={}", p.id);
        let (status, page) = call(&f.router, "GET", &uri, Some(token), None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        item_ids(&page["Items"])
    }
    let f = fixture().await;
    let p = SharedPlaylist::new(&f).await;
    let all = [p.gamma, p.alpha, p.delta, p.epsilon];
    // "Allowed" holds Alpha and Epsilon; "Hidden" Gamma and Delta.
    let in_allowed = [p.alpha, p.epsilon];
    assert_eq!(browsed(&f, &p, &f.admin).await, all, "enabled");
    assert_eq!(browsed(&f, &p, &f.viewer).await, in_allowed, "enabled");

    let (_, folders) = call(
        &f.router,
        "GET",
        "/Library/VirtualFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let allowed = folders
        .as_array()
        .expect("folders")
        .iter()
        .find(|folder| folder["Name"] == "Allowed")
        .expect("Allowed")
        .clone();
    let mut options = allowed["LibraryOptions"].clone();
    options["Enabled"] = json!(false);
    let (status, _) = call(
        &f.router,
        "POST",
        "/Library/VirtualFolders/LibraryOptions",
        Some(&f.admin),
        Some(json!({"Id": allowed["ItemId"], "LibraryOptions": options})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert_eq!(p.stored(&f).await, all);
    assert_eq!(browsed(&f, &p, &f.admin).await, [p.gamma, p.delta]);
    assert_eq!(p.listed(&f, &f.viewer).await, in_allowed);
    assert_eq!(browsed(&f, &p, &f.viewer).await, Vec::<uuid::Uuid>::new());
}

/// A move's index counts the list its caller was given, so the member lands
/// where they asked and the members they cannot see keep their places.
#[tokio::test]
async fn a_move_counts_the_members_its_caller_can_open() {
    let f = fixture().await;
    let p = SharedPlaylist::new(&f).await;
    assert_eq!(p.listed(&f, &f.viewer).await, [p.alpha, p.epsilon]);

    // Down: `[Alpha, Epsilon]` to `[Epsilon, Alpha]`. Counted over every
    // member, Alpha is already at index 1 and nothing moves.
    p.move_to(&f, p.alpha, 1).await;
    assert_eq!(p.listed(&f, &f.viewer).await, [p.epsilon, p.alpha]);
    assert_eq!(p.stored(&f).await, [p.gamma, p.delta, p.epsilon, p.alpha]);

    // Back up: before the first member the viewer can open, not before Gamma.
    p.move_to(&f, p.alpha, 0).await;
    assert_eq!(p.listed(&f, &f.viewer).await, [p.alpha, p.epsilon]);
    assert_eq!(p.stored(&f).await, [p.gamma, p.delta, p.alpha, p.epsilon]);

    // Already there: stored order stays.
    p.move_to(&f, p.alpha, 0).await;
    assert_eq!(p.stored(&f).await, [p.gamma, p.delta, p.alpha, p.epsilon]);

    // Past the viewer's list, as a client counting `ChildCount` asks: the end.
    p.move_to(&f, p.alpha, 3).await;
    assert_eq!(p.listed(&f, &f.viewer).await, [p.epsilon, p.alpha]);
    assert_eq!(p.stored(&f).await, [p.gamma, p.delta, p.epsilon, p.alpha]);

    // A member the viewer was not given moves only if its id is named, which
    // `ItemIds` allows and the listing does not, and their list stays as it was.
    p.move_to(&f, p.gamma, 1).await;
    assert_eq!(p.listed(&f, &f.viewer).await, [p.epsilon, p.alpha]);
    assert_eq!(p.stored(&f).await, [p.delta, p.epsilon, p.gamma, p.alpha]);
}

/// The viewer's `GET /Items/{allowed}` status.
async fn viewer_reads_allowed(f: &Fixture) -> StatusCode {
    call(
        &f.router,
        "GET",
        &format!("/Items/{}", f.allowed),
        Some(&f.viewer),
        None,
    )
    .await
    .0
}

#[tokio::test]
async fn library_option_and_library_row_edits_reach_the_next_request() {
    // The policy and library rows are cached across requests (ferrofin_db::row_cache, gl-973):
    // each edit below must reach the request right after it, with the caches already warm.
    let f = fixture().await;
    assert!(
        f.h.db.row_cache_enabled(),
        "the composition root turns the caches on"
    );
    assert_eq!(viewer_reads_allowed(&f).await, StatusCode::OK);
    assert!(
        f.h.db.cached_libraries().is_some(),
        "and the read filled them"
    );
    let (_, folders) = call(
        &f.router,
        "GET",
        "/Library/VirtualFolders",
        Some(&f.admin),
        None,
    )
    .await;
    let allowed = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Allowed")
        .unwrap()
        .clone();
    let library = allowed["ItemId"].as_str().unwrap().to_owned();
    // A library disabled through its options is hidden (`CollectionFolder.IsVisible`).
    let library_path = format!("/Items/{library}");
    let read_library = || call(&f.router, "GET", &library_path, Some(&f.viewer), None);
    assert_eq!(read_library().await.0, StatusCode::OK);
    for (enabled, expected) in [(false, StatusCode::NOT_FOUND), (true, StatusCode::OK)] {
        let mut options = allowed["LibraryOptions"].clone();
        options["Enabled"] = json!(enabled);
        let (status, _) = call(
            &f.router,
            "POST",
            "/Library/VirtualFolders/LibraryOptions",
            Some(&f.admin),
            Some(json!({"Id": library, "LibraryOptions": options})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(read_library().await.0, expected, "Enabled = {enabled}");
        // `LibraryController.GetMediaFolders` drops it too, for every caller.
        let (_, media_folders) = call(
            &f.router,
            "GET",
            "/Library/MediaFolders",
            Some(&f.admin),
            None,
        )
        .await;
        let listed = |name: &str| {
            media_folders["Items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| folder["Name"] == name)
        };
        assert_eq!(listed("Allowed"), enabled, "Enabled = {enabled}");
        assert!(listed("Playlists"), "no options, so never disabled");
    }
    // A tag on the library's own row is inherited by its items, so a blocked tag hides them.
    let (_, mut user) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    user["Policy"]["BlockedTags"] = json!(["kept-out"]);
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Users/{}/Policy", f.viewer_id),
        Some(&f.admin),
        Some(user["Policy"].clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        viewer_reads_allowed(&f).await,
        StatusCode::OK,
        "no tag on the library yet"
    );
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Items/{library}"),
        Some(&f.admin),
        Some(json!({"Name": "Allowed", "Tags": ["kept-out"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        viewer_reads_allowed(&f).await,
        StatusCode::NOT_FOUND,
        "the library's tag is blocked"
    );
}

/// The names of a listing's `Playlist` rows, sorted.
fn playlist_names(listing: &Value) -> Vec<String> {
    let mut names: Vec<String> = listing["Items"]
        .as_array()
        .unwrap_or_else(|| panic!("no Items: {listing}"))
        .iter()
        .filter(|item| item["Type"] == "Playlist")
        .map(|item| item["Name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

/// A playlist of the fixture's visible movie, private unless `public`, as `token`.
async fn create_playlist(f: &Fixture, token: &str, name: &str, public: bool) -> String {
    let (status, created) = call(
        &f.router,
        "POST",
        "/Playlists",
        Some(token),
        Some(json!({"Name": name, "Ids": [f.allowed], "IsPublic": public})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{name}: {created}");
    created["Id"].as_str().unwrap().to_owned()
}

/// gl-831: a listing that is not limited to playlists hides the ones a user
/// cannot see, in the SQL that pages and counts it, whichever rule shows them
/// the others: their own, a share, open access. An administrator's mixed
/// listing keeps Jellyfin's answer, and `/Items/Suggestions` takes Jellyfin's
/// `IsVisible` for everyone.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn listings_hide_the_playlists_a_user_cannot_see() {
    let f = fixture().await;
    let private = create_playlist(&f, &f.admin, "Admin private", false).await;
    let shared = create_playlist(&f, &f.admin, "Admin shared", false).await;
    create_playlist(&f, &f.admin, "Admin open", true).await;
    create_playlist(&f, &f.viewer, "Viewer own", false).await;
    let (status, body) = call(
        &f.router,
        "POST",
        &format!("/Playlists/{shared}/Users/{}", f.viewer_id),
        Some(&f.admin),
        Some(json!({"CanEdit": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let visible = ["Admin open", "Admin shared", "Viewer own"];
    let viewer = format!("userId={}", f.viewer_id);

    // A mixed listing, and the playlist-only one that always hid them.
    for filter in [
        "",
        "&includeItemTypes=Playlist,Movie",
        "&includeItemTypes=Playlist",
    ] {
        let uri = format!("/Items?{viewer}&recursive=true{filter}");
        let (status, listing) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {listing}");
        assert_eq!(playlist_names(&listing), visible, "{uri}");
    }

    // The count and the pages agree with the listing: two movies, three playlists.
    let mut paged = Vec::new();
    for start in [0, 2, 4] {
        let uri = format!(
            "/Items?{viewer}&recursive=true&includeItemTypes=Playlist,Movie\
             &sortBy=SortName&limit=2&startIndex={start}&enableTotalRecordCount=true"
        );
        let (_, page) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
        assert_eq!(page["TotalRecordCount"], 5, "{uri}: {page}");
        paged.extend(playlist_names(&page));
    }
    paged.sort();
    assert_eq!(paged, visible);

    // Library-wide search reads the same rows.
    let uri = format!("/Search/Hints?{viewer}&searchTerm=Admin");
    let (_, hints) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
    let mut names: Vec<&str> = hints["SearchHints"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hint| hint["Name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Admin open", "Admin shared"], "{hints}");

    // Suggestions: `SuggestionsController` passes no skip, and the total stays
    // right because the rows never reach the page.
    for filter in ["", "&type=Playlist"] {
        let uri = format!("/Items/Suggestions?{viewer}&enableTotalRecordCount=true{filter}");
        let (status, listing) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {listing}");
        assert_eq!(playlist_names(&listing), visible, "{uri}");
    }
    let uri =
        format!("/Items/Suggestions?{viewer}&type=Playlist&limit=2&enableTotalRecordCount=true");
    let (_, page) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
    assert_eq!(page["TotalRecordCount"], 3, "{page}");
    assert_eq!(page["Items"].as_array().unwrap().len(), 2, "{page}");

    // By id, the item stays out of reach.
    let uri = format!("/Items?{viewer}&ids={private}");
    let (status, listing) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {listing}");
    assert_eq!(playlist_names(&listing), Vec::<String>::new(), "{uri}");

    // An administrator's mixed listing (`userId` defaults to the caller) is
    // Jellyfin's: nothing is taken out of it…
    let (_, listing) = call(
        &f.router,
        "GET",
        "/Items?recursive=true",
        Some(&f.admin),
        None,
    )
    .await;
    assert_eq!(
        playlist_names(&listing),
        ["Admin open", "Admin private", "Admin shared", "Viewer own"]
    );
    // …while `IsVisible` has no administrator arm, so Suggestions and the
    // playlist-only listing leave out another user's private playlist.
    for uri in [
        "/Items/Suggestions",
        "/Items?recursive=true&includeItemTypes=Playlist",
    ] {
        let (_, me) = call(&f.router, "GET", "/Users/Me", Some(&f.admin), None).await;
        let separator = if uri.contains('?') { '&' } else { '?' };
        let uri = format!("{uri}{separator}userId={}", me["Id"].as_str().unwrap());
        let (_, listing) = call(&f.router, "GET", &uri, Some(&f.admin), None).await;
        assert_eq!(
            playlist_names(&listing),
            ["Admin open", "Admin private", "Admin shared"],
            "{uri}"
        );
    }

    // A user no playlist names sees what is open to everyone.
    create_user(&f.router, &f.admin, "plain", "plain-pw").await;
    let (plain, plain_id) = login(&f.router, "plain", "plain-pw").await;
    let uri = format!("/Items?userId={plain_id}&recursive=true");
    let (_, listing) = call(&f.router, "GET", &uri, Some(&plain), None).await;
    assert_eq!(playlist_names(&listing), ["Admin open"], "{uri}: {listing}");
}

/// Sends `GET uri` with exactly the given headers (no default
/// `Authorization`), for the credential forms a stream URL can carry.
async fn status_with(router: &axum::Router, uri: &str, headers: &[(&str, String)]) -> StatusCode {
    let mut req = Request::builder().method("GET").uri(uri);
    for (name, value) in headers {
        req = req.header(*name, value);
    }
    router
        .clone()
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("response")
        .status()
}

/// Applies `change` to `user_id`'s policy through the route the dashboard uses.
async fn set_policy(f: &Fixture, user_id: &str, change: impl FnOnce(&mut Value)) {
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{user_id}"),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    change(&mut policy);
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Users/{user_id}/Policy"),
        Some(&f.admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// The parameters a segment URL must bind.
const SEGMENT_QUERY: &str = "runtimeTicks=0&actualSegmentLengthTicks=0";

/// The two static reads, which answer `200` from a file with no ffmpeg, so a
/// refusal cannot be mistaken for a missing track or a failed probe.
fn static_routes(id: &str) -> Vec<String> {
    vec![
        format!("/Videos/{id}/stream?static=true"),
        format!("/Videos/{id}/stream.mkv?static=true"),
        format!("/Audio/{id}/stream?static=true"),
        format!("/Audio/{id}/stream.mkv?static=true"),
    ]
}

/// Every route that reads an item's bytes for a caller, static reads included:
/// the stream, HLS, universal audio, subtitle and attachment families.
fn stream_routes(id: &str) -> Vec<String> {
    let mut routes = static_routes(id);
    routes.extend([
        format!("/Videos/{id}/stream?static=false&VideoCodec=h264&PlaySessionId=x"),
        format!("/Audio/{id}/stream?static=false&AudioCodec=mp3&PlaySessionId=x"),
        format!("/Audio/{id}/universal"),
        format!("/Videos/{id}/master.m3u8?MediaSourceId={id}"),
        format!("/Videos/{id}/main.m3u8?MediaSourceId={id}"),
        format!("/Videos/{id}/live.m3u8?MediaSourceId={id}"),
        format!("/Audio/{id}/master.m3u8?MediaSourceId={id}"),
        format!("/Audio/{id}/main.m3u8?MediaSourceId={id}"),
        format!("/Videos/{id}/hls1/main/0.ts?MediaSourceId={id}&{SEGMENT_QUERY}"),
        format!("/Audio/{id}/hls1/main/0.ts?MediaSourceId={id}&{SEGMENT_QUERY}"),
        format!("/Videos/{id}/{id}/Subtitles/0/Stream.srt"),
        format!("/Videos/{id}/{id}/Subtitles/0/0/Stream.srt"),
        format!("/Videos/{id}/{id}/Subtitles/0/subtitles.m3u8?segmentLength=30"),
        format!("/Videos/{id}/{id}/Attachments/0"),
    ]);
    routes
}

/// The routes a request with no credential may use, whose answer must not
/// depend on a user.
fn anonymous_routes(id: &str) -> Vec<String> {
    let mut routes = static_routes(id);
    routes.extend([
        format!("/Videos/{id}/{id}/Subtitles/0/Stream.srt"),
        format!("/Videos/{id}/{id}/Subtitles/0/0/Stream.srt"),
        format!("/Videos/{id}/{id}/Attachments/0"),
    ]);
    routes
}

/// Gives `id` an external subtitle as a scan stores one, whether or not this
/// host has an ffprobe to read the scanned file with.
async fn store_sidecar(f: &Fixture, id: &str) {
    use ferrofin_traits::persistence::MediaStreamRepository as _;
    let path = f.h.path(&format!("subtitles/{id}.srt"));
    touch(&path, "1\n00:00:00,500 --> 00:00:02,000\nA line\n");
    let item: uuid::Uuid = id.parse().unwrap();
    let row = ferrofin_db::entities::base_items::MediaStreamInfoEntity {
        item_id: ferrofin_db::store::guid_to_db(item),
        stream_index: 0,
        stream_type: i32::from(ferrofin_db::enums::MediaStreamTypeEntity::Subtitle),
        codec: Some("subrip".to_owned()),
        path: Some(path.to_string_lossy().into_owned()),
        is_external: true,
        ..ferrofin_db::entities::base_items::MediaStreamInfoEntity::default()
    };
    ferrofin_core::media_stream_repository::FerrofinMediaStreamRepository::new(f.h.db.clone())
        .save_media_streams(item, &[row])
        .await
        .unwrap();
}

/// A signed-in user is held to the libraries it may open on every route that
/// reads bytes (gl-1090). Upstream's `GetStreamingState` reads the item with no
/// user, so Jellyfin streams it.
#[tokio::test]
async fn a_known_user_is_refused_what_it_cannot_open_on_every_stream_route() {
    let f = fixture().await;
    for uri in stream_routes(&f.hidden) {
        let (status, body) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    // What the user may open answers as it does for the administrator, whose
    // reads of the hidden item are the same again.
    for (allowed, hidden) in stream_routes(&f.allowed)
        .into_iter()
        .zip(stream_routes(&f.hidden))
    {
        let viewer = call(&f.router, "GET", &allowed, Some(&f.viewer), None)
            .await
            .0;
        let admin = call(&f.router, "GET", &allowed, Some(&f.admin), None)
            .await
            .0;
        assert_eq!(viewer, admin, "{allowed}");
        let admin_hidden = call(&f.router, "GET", &hidden, Some(&f.admin), None)
            .await
            .0;
        assert_eq!(admin_hidden, admin, "{hidden}");
    }
    for uri in static_routes(&f.allowed) {
        assert_eq!(
            call(&f.router, "GET", &uri, Some(&f.viewer), None).await.0,
            StatusCode::OK,
            "{uri}"
        );
    }
}

/// The owner's boundary stops at a known user: no credential, an API key, and a
/// token that names nobody read these routes as they did (gl-1090). A token in
/// a form the server does not read names nobody either: `api_key` counts only
/// with `EnableLegacyAuthorization` on.
#[tokio::test]
async fn no_user_on_a_stream_route_means_no_user_check() {
    let f = fixture().await;
    let (status, _) = call(
        &f.router,
        "POST",
        "/Auth/Keys?app=stream-access",
        Some(&f.admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, keys) = call(&f.router, "GET", "/Auth/Keys", Some(&f.admin), None).await;
    let key = keys["Items"][0]["AccessToken"].as_str().unwrap();
    for uri in static_routes(&f.hidden) {
        let sep = if uri.contains('?') { '&' } else { '?' };
        for (label, request, headers) in [
            ("no credential", uri.clone(), vec![]),
            ("API key", format!("{uri}{sep}ApiKey={key}"), vec![]),
            (
                "unknown token",
                format!("{uri}{sep}ApiKey=not-a-token"),
                vec![],
            ),
            (
                "unread form",
                format!("{uri}{sep}api_key={}", f.viewer),
                vec![],
            ),
            (
                "unread header",
                uri.clone(),
                vec![("X-Emby-Token", f.viewer.clone())],
            ),
        ] {
            assert_eq!(
                status_with(&f.router, &request, &headers).await,
                StatusCode::OK,
                "{label}: {request}"
            );
        }
    }
    // A subtitle read answers from the sidecar, whichever library it is in.
    for id in [&f.allowed, &f.hidden] {
        store_sidecar(&f, id).await;
        for uri in [
            format!("/Videos/{id}/{id}/Subtitles/0/Stream.srt"),
            format!("/Videos/{id}/{id}/Subtitles/0/0/Stream.srt"),
        ] {
            assert_eq!(
                status_with(&f.router, &uri, &[]).await,
                StatusCode::OK,
                "{uri}"
            );
        }
    }
}

/// A token outside its access schedule and a locked-out account's token are
/// refused on the routes that take no credential, as on the HLS routes, and
/// dropping the token still reads what any anonymous request reads (gl-1090).
/// The dashboard's own "disable" revokes the account's tokens, so such a token
/// names nobody and reads as no credential does: lockout is what leaves a
/// disabled account's token standing.
#[tokio::test]
async fn account_state_binds_a_known_user_on_the_stream_routes() {
    let f = fixture().await;
    let routes = anonymous_routes(&f.allowed);
    set_policy(&f, &f.viewer_id, |policy| {
        // A window of one instant, midnight.
        policy["AccessSchedules"] = json!([{
            "Id": 0, "UserId": f.viewer_id, "DayOfWeek": "Everyday",
            "StartHour": 0.0, "EndHour": 0.0,
        }]);
    })
    .await;
    for uri in &routes {
        assert_eq!(
            call(&f.router, "GET", uri, Some(&f.viewer), None).await.0,
            StatusCode::FORBIDDEN,
            "outside the schedule: {uri}"
        );
    }
    set_policy(&f, &f.viewer_id, |policy| {
        policy["AccessSchedules"] = json!([]);
        policy["LoginAttemptsBeforeLockout"] = json!(1);
    })
    .await;
    let (status, _) = call(
        &f.router,
        "POST",
        "/Users/AuthenticateByName",
        None,
        Some(json!({"Username": "viewer", "Pw": "not-the-password"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{}", f.viewer_id),
        Some(&f.admin),
        None,
    )
    .await;
    assert_eq!(user["Policy"]["IsDisabled"], json!(true), "{user}");
    for uri in &routes {
        assert_eq!(
            call(&f.router, "GET", uri, Some(&f.viewer), None).await.0,
            StatusCode::UNAUTHORIZED,
            "locked out: {uri}"
        );
    }
    // The account is reached only on a route: a segment that is not
    // `stream.{container}` is no route for anyone.
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Videos/{}/streams.mkv", f.allowed),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for uri in static_routes(&f.allowed) {
        assert_eq!(
            status_with(&f.router, &uri, &[]).await,
            StatusCode::OK,
            "no credential: {uri}"
        );
    }
    // Reopen the account, sign in again, and disable it from the dashboard.
    set_policy(&f, &f.viewer_id, |policy| {
        policy["IsDisabled"] = json!(false);
        policy["LoginAttemptsBeforeLockout"] = json!(-1);
    })
    .await;
    let (viewer, _) = login(&f.router, "viewer", "viewer-pw").await;
    set_policy(&f, &f.viewer_id, |policy| {
        policy["IsDisabled"] = json!(true);
    })
    .await;
    for uri in static_routes(&f.allowed) {
        assert_eq!(
            call(&f.router, "GET", &uri, Some(&viewer), None).await.0,
            StatusCode::OK,
            "revoked: {uri}"
        );
    }
}

/// A version in a library the user cannot open is no source for them: a visible
/// item does not lend it out through `mediaSourceId` (gl-1090), as
/// `GetStaticMediaSources` keeps it out of `PlaybackInfo`.
#[tokio::test]
async fn a_visible_item_does_not_lend_out_a_hidden_version() {
    let f = fixture().await;
    let (status, _) = call(
        &f.router,
        "POST",
        &format!("/Videos/MergeVersions?ids={},{}", f.allowed, f.hidden),
        Some(&f.admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (allowed, hidden) = (&f.allowed, &f.hidden);
    let (_, info) = call(
        &f.router,
        "GET",
        &format!("/Items/{allowed}/PlaybackInfo"),
        Some(&f.viewer),
        None,
    )
    .await;
    let sources = info["MediaSources"].as_array().unwrap();
    assert_eq!(sources.len(), 1, "{info}");
    for uri in [
        format!("/Videos/{allowed}/stream?static=true&mediaSourceId={hidden}"),
        format!("/Audio/{allowed}/stream?static=true&mediaSourceId={hidden}"),
        format!("/Videos/{allowed}/stream.mkv?static=true&MediaSourceId={hidden}"),
        format!("/Videos/{allowed}/master.m3u8?MediaSourceId={hidden}"),
        format!("/Videos/{allowed}/main.m3u8?MediaSourceId={hidden}"),
        format!("/Videos/{allowed}/hls1/main/0.ts?MediaSourceId={hidden}&{SEGMENT_QUERY}"),
        format!("/Audio/{allowed}/universal?mediaSourceId={hidden}"),
        format!("/Videos/{allowed}/{hidden}/Subtitles/0/Stream.srt"),
        format!("/Videos/{allowed}/{hidden}/Subtitles/0/0/Stream.srt"),
        format!("/Videos/{allowed}/{hidden}/Subtitles/0/subtitles.m3u8?segmentLength=30"),
        format!("/Videos/{allowed}/{hidden}/Attachments/0"),
    ] {
        let (status, body) = call(&f.router, "GET", &uri, Some(&f.viewer), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    let simple = hidden.replace('-', "");
    let uri = format!("/Videos/{allowed}/stream?static=true&mediaSourceId={simple}");
    assert_eq!(
        call(&f.router, "GET", &uri, Some(&f.viewer), None).await.0,
        StatusCode::NOT_FOUND,
        "either spelling of the guid"
    );
    // Not a version: what the route answers, as for anyone.
    for (id, expected) in [("", StatusCode::OK), ("not-a-guid", StatusCode::NOT_FOUND)] {
        let uri = format!("/Videos/{allowed}/stream?static=true&mediaSourceId={id}");
        for token in [Some(&f.viewer), None] {
            assert_eq!(
                call(&f.router, "GET", &uri, token.map(String::as_str), None)
                    .await
                    .0,
                expected,
                "{uri}"
            );
        }
    }
    for (token, id) in [
        (&f.viewer, allowed),
        (&f.admin, allowed),
        (&f.admin, hidden),
    ] {
        let uri = format!("/Videos/{allowed}/stream?static=true&mediaSourceId={id}");
        assert_eq!(
            call(&f.router, "GET", &uri, Some(token), None).await.0,
            StatusCode::OK,
            "{uri}"
        );
    }
}

/// A DVR recording's row, whose id no `BaseItems` row carries, on a file of its
/// own at `folder`. `channel` is the channel it was captured from.
async fn stored_recording(f: &Fixture, folder: &str, name: &str, channel: uuid::Uuid) -> String {
    let path = f.h.path(&format!("{folder}/{name}.ts"));
    touch(&path, "captured");
    let id = uuid::Uuid::new_v4();
    let timer = ferrofin_livetv::dvr::TimerRecordingInfo {
        id: format!("timer-{name}"),
        name: name.to_owned(),
        channel_id: channel,
        ..ferrofin_livetv::dvr::TimerRecordingInfo::default()
    };
    ferrofin_livetv::dvr_repository::insert_recording(
        &f.h.db,
        id,
        &timer,
        &path,
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    id.simple().to_string()
}

/// An M3U tuner with one channel whose stream is a local origin, listed through
/// the routes the dashboard uses. Returns the channel's id.
async fn stored_channel(f: &Fixture) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = listener.local_addr().unwrap();
    let tuner = axum::Router::new().fallback(|| async { "live bytes" });
    tokio::spawn(async move { axum::serve(listener, tuner).await });
    let playlist = f.h.path("playlist.m3u");
    let guide = f.h.path("guide.xml");
    touch(
        &playlist,
        &format!("#EXTM3U\n#EXTINF:-1 tvg-id=\"one\" tvg-chno=\"1\",One\nhttp://{origin}/one.ts\n"),
    );
    touch(
        &guide,
        "<tv><channel id=\"one\"><display-name>One</display-name></channel></tv>",
    );
    for (uri, body) in [
        (
            "/LiveTv/TunerHosts",
            json!({"Type": "m3u", "Url": playlist.to_string_lossy()}),
        ),
        (
            "/LiveTv/ListingProviders",
            json!({"Type": "xmltv", "Path": guide.to_string_lossy(), "EnableAllTuners": true}),
        ),
    ] {
        let (status, response) = call(&f.router, "POST", uri, Some(&f.admin), Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {response}");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, page) = call(&f.router, "GET", "/LiveTv/Channels", Some(&f.admin), None).await;
        if let Some(id) = page["Items"][0]["Id"].as_str() {
            f.h.wired
                .state
                .live_tv
                .as_ref()
                .unwrap()
                .refresh_guide()
                .await
                .unwrap();
            return id.to_owned();
        }
        assert!(std::time::Instant::now() < deadline, "no lineup appeared");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A recording's id names no `BaseItems` row, and it must still play for every
/// caller who may reach it. Its rule is `GET /LiveTv/Recordings/{id}`'s, the one
/// route that reads it, since `PlaybackInfo` finds no item for anyone: "Live TV
/// access", its own visibility, the library holding its file, and its channel.
#[allow(
    clippy::too_many_lines,
    reason = "one recording, through every rule that reaches it"
)]
#[tokio::test]
async fn a_recording_plays_for_whoever_may_read_it() {
    let f = fixture().await;
    let channel = stored_channel(&f).await;
    let channel = channel.parse().unwrap();
    let id = stored_recording(&f, "recordings", "show", channel).await;
    let uri = format!("/Videos/{id}/stream?static=true");
    let audio = format!("/Audio/{id}/stream?static=true");
    assert_eq!(status_with(&f.router, &uri, &[]).await, StatusCode::OK);
    for token in [&f.viewer, &f.admin] {
        assert_eq!(
            call(
                &f.router,
                "GET",
                &format!("/LiveTv/Recordings/{id}"),
                Some(token),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        for uri in [&uri, &audio] {
            assert_eq!(
                call(&f.router, "GET", uri, Some(token), None).await.0,
                StatusCode::OK,
                "{uri}"
            );
        }
    }
    let hidden = stored_recording(&f, "more", "kept-out", channel).await;
    let uri_hidden = format!("/Videos/{hidden}/stream?static=true");
    assert_eq!(
        call(&f.router, "GET", &uri_hidden, Some(&f.viewer), None)
            .await
            .0,
        StatusCode::NOT_FOUND,
        "a file inside a library the user cannot open"
    );
    assert_eq!(
        call(&f.router, "GET", &uri_hidden, Some(&f.admin), None)
            .await
            .0,
        StatusCode::OK
    );
    let lost = stored_recording(&f, "recordings", "gone", uuid::Uuid::from_u128(0xdead)).await;
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Videos/{lost}/stream?static=true"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::OK,
        "a channel gone from the lineup leaves its recordings playable"
    );
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/LiveTv/Recordings/{lost}"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    // Its channel hidden from the user, and then the recording itself: what the
    // user may not see of it, on the stream and on its own route.
    for kind in ["LiveTvChannel", "Other"] {
        set_policy(&f, &f.viewer_id, |policy| {
            policy["BlockUnratedItems"] = json!([kind]);
        })
        .await;
        for route in [uri.clone(), format!("/LiveTv/Recordings/{id}")] {
            assert_eq!(
                call(&f.router, "GET", &route, Some(&f.viewer), None)
                    .await
                    .0,
                StatusCode::NOT_FOUND,
                "{kind} unrated blocked: {route}"
            );
        }
    }
    set_policy(&f, &f.viewer_id, |policy| {
        policy["BlockUnratedItems"] = json!([]);
        policy["EnableLiveTvAccess"] = json!(false);
    })
    .await;
    for uri in [&uri, &audio] {
        assert_eq!(
            call(&f.router, "GET", uri, Some(&f.viewer), None).await.0,
            StatusCode::NOT_FOUND,
            "no Live TV access: {uri}"
        );
    }
    assert_eq!(status_with(&f.router, &uri, &[]).await, StatusCode::OK);
    assert_eq!(
        call(&f.router, "GET", &uri, Some(&f.admin), None).await.0,
        StatusCode::OK
    );
}

/// A channel is a row with no path, so the library list does not reach it. Its
/// rule is `PlaybackInfo`'s, whose answer the stream route repeats: rating and
/// tag rules hide it, and "Live TV access" does not, there or here.
#[tokio::test]
async fn a_channel_answers_as_its_playback_info_does() {
    let f = fixture().await;
    let id = stored_channel(&f).await;
    let stream = format!("/Videos/{id}/stream?static=true");
    let info = format!("/Items/{id}/PlaybackInfo");
    assert_eq!(status_with(&f.router, &stream, &[]).await, StatusCode::OK);
    for token in [&f.viewer, &f.admin] {
        assert_eq!(
            call(&f.router, "GET", &info, Some(token), None).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f.router, "GET", &stream, Some(token), None).await.0,
            StatusCode::OK
        );
    }
    set_policy(&f, &f.viewer_id, |policy| {
        policy["EnableLiveTvAccess"] = json!(false);
    })
    .await;
    for uri in [&info, &stream] {
        assert_eq!(
            call(&f.router, "GET", uri, Some(&f.viewer), None).await.0,
            StatusCode::OK,
            "{uri}"
        );
    }
    set_policy(&f, &f.viewer_id, |policy| {
        policy["BlockUnratedItems"] = json!(["LiveTvChannel"]);
    })
    .await;
    for uri in [&info, &stream] {
        assert_eq!(
            call(&f.router, "GET", uri, Some(&f.viewer), None).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    assert_eq!(status_with(&f.router, &stream, &[]).await, StatusCode::OK);
}
