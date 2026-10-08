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
    Harness { wired, temp }
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
    // Deletion in this baseline removes database rows only.
    assert!(f.h.path("movies/Alpha (1999)/Alpha (1999).mkv").exists());
    assert!(f.h.path("more/Gamma (2001)/Gamma (2001).mkv").exists());
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
async fn browse_and_userless_exceptions_keep_their_upstream_contract() {
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
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&f.router, "GET", &parent_uri, Some(&f.admin), None)
            .await
            .0,
        StatusCode::OK
    );
    // A container-only request re-roots before the parent visibility check.
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("{parent_uri}&includeItemTypes=BoxSet"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, ids) = call(
        &f.router,
        "GET",
        &format!("/Items?ids={id}"),
        Some(&f.viewer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids["Items"].as_array().unwrap().len(), 1);
    assert_eq!(
        uuid::Uuid::parse_str(ids["Items"][0]["Id"].as_str().unwrap()).unwrap(),
        uuid::Uuid::parse_str(id).unwrap()
    );
    assert_eq!(
        call(
            &f.router,
            "GET",
            &format!("/Videos/{id}/stream"),
            Some(&f.viewer),
            None
        )
        .await
        .0,
        StatusCode::OK
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

#[tokio::test]
async fn renamed_user_loses_existing_tokens_on_disallowed_devices() {
    let h = boot().await;
    let router = ferrofin_api::create_router(h.wired.state.clone());
    let (admin, _) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let id = create_user(&router, &admin, "device-user", "pw").await;
    let (user, _) = login(&router, "device-user", "pw").await;
    let (_, mut dto) = call(&router, "GET", &format!("/Users/{id}"), Some(&admin), None).await;
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
    assert_eq!(
        call(&router, "GET", "/Users/Me", Some(&user), None).await.0,
        StatusCode::OK,
        "policy saves do not emit OnUserUpdated in the pinned source"
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
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/Users?userId={id}"),
            Some(&admin),
            Some(json!({"Name":"renamed"}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&router, "GET", "/Users/Me", Some(&user), None).await.0,
        StatusCode::UNAUTHORIZED
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
