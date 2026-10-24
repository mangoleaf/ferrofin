//! An item DTO's `MediaSources` over the real router and composition root.
//!
//! Upstream's `DtoService` builds them with `GetStaticMediaSources(item, true,
//! user)` (v12.2 `DtoService.cs:410`), whose per-user tail
//! (`MediaSourceManager.cs:397-421`) picks the user's default audio and
//! subtitle streams, scores the subtitle streams, and applies the user's
//! playback permissions. The item endpoints must carry the same per-user
//! fields `PlaybackInfo` does.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_core::media_stream_repository::FerrofinMediaStreamRepository;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
use ferrofin_db::enums::MediaStreamTypeEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use ferrofin_traits::persistence::MediaStreamRepository as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "media-sources-admin-pw";
const MOVIE: &str = "movies/Crimson Square (1963)/Crimson Square (1963).mkv";

/// A booted server, its database, the temp dir it scanned, and a session.
struct Fixture {
    _wired: WiredApp,
    _temp: tempfile::TempDir,
    router: axum::Router,
    admin: String,
    admin_id: String,
    movie: String,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-item-media-sources".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let movie_path = temp.path().join(MOVIE);
    std::fs::create_dir_all(movie_path.parent().expect("parent")).expect("dir");
    std::fs::write(&movie_path, b"").expect("movie");
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
    let (admin, admin_id) = login(&router, ADMIN_USER, ADMIN_PASSWORD).await;
    let path = encode(&temp.path().join("movies").to_string_lossy());
    let (status, _) = call(
        &router,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Movies&collectionType=movies&paths={path}&refreshLibrary=false"
        ),
        Some(&admin),
        None,
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
    let (status, items) = call(
        &router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie&fields=Path",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{items}");
    let movie = items["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .find(|i| i["Path"] == movie_path.to_string_lossy().as_ref())
        .and_then(|i| i["Id"].as_str())
        .expect("the scanned movie")
        .to_owned();
    // The streams of the issue's example: an English default audio and
    // subtitle, and a French alternative of each.
    store_streams(&db, Uuid::parse_str(&movie).expect("id")).await;
    Fixture {
        _wired: wired,
        _temp: temp,
        router,
        admin,
        admin_id,
        movie,
    }
}

/// `0 h264 · 1 aac eng (default) · 2 aac fra · 3 subrip eng (default) ·
/// 4 subrip fra`, as the probe stores them.
async fn store_streams(db: &Database, item: Uuid) {
    use MediaStreamTypeEntity::{Audio, Subtitle, Video};
    let rows: Vec<MediaStreamInfoEntity> = [
        (Video, "h264", None, true),
        (Audio, "aac", Some("eng"), true),
        (Audio, "aac", Some("fra"), false),
        (Subtitle, "subrip", Some("eng"), true),
        (Subtitle, "subrip", Some("fra"), false),
    ]
    .into_iter()
    .zip(0_i64..)
    .map(
        |((kind, codec, language, is_default), index)| MediaStreamInfoEntity {
            item_id: guid_to_db(item),
            stream_index: index,
            stream_type: i32::from(kind),
            codec: Some(codec.to_owned()),
            language: language.map(str::to_owned),
            is_default,
            ..MediaStreamInfoEntity::default()
        },
    )
    .collect();
    FerrofinMediaStreamRepository::new(db.clone())
        .save_media_streams(item, &rows)
        .await
        .expect("streams");
}

/// Sends one request as `token` and returns status + body.
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

/// The one source of the movie on its detail endpoint, as `token` sees it.
async fn detail_source(f: &Fixture, token: &str, user_id: &str) -> Value {
    let (status, dto) = call(
        &f.router,
        "GET",
        &format!("/Users/{user_id}/Items/{}", f.movie),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{dto}");
    // `DtoService.cs:1473-1485`: the top-level `MediaStreams` are the item's
    // own source's, scores included.
    assert_eq!(dto["MediaStreams"], dto["MediaSources"][0]["MediaStreams"]);
    dto["MediaSources"][0].clone()
}

/// The same source on a list page that asked for `MediaSources`.
async fn list_source(f: &Fixture, token: &str, user_id: &str) -> Value {
    let (status, page) = call(
        &f.router,
        "GET",
        &format!(
            "/Items?userId={user_id}&ids={}&fields=MediaSources",
            f.movie
        ),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["Items"][0]["MediaSources"][0].clone()
}

/// The source `PlaybackInfo` hands the same user — the path that already
/// runs the per-user tail, and so the oracle for the item DTO.
async fn playback_source(f: &Fixture, token: &str, user_id: &str) -> Value {
    let (status, info) = call(
        &f.router,
        "GET",
        &format!("/Items/{}/PlaybackInfo?userId={user_id}", f.movie),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{info}");
    info["MediaSources"][0].clone()
}

/// The per-user fields of a source: the default stream indices, each
/// subtitle stream's `Score`, and the two policy-driven `Supports*` flags.
fn per_user_fields(source: &Value) -> Value {
    let scores: Vec<Value> = source["MediaStreams"]
        .as_array()
        .expect("MediaStreams")
        .iter()
        .filter(|s| s["Type"] == "Subtitle")
        .map(|s| json!([s["Index"], s["Score"]]))
        .collect();
    json!({
        "DefaultAudioStreamIndex": source["DefaultAudioStreamIndex"],
        "DefaultSubtitleStreamIndex": source["DefaultSubtitleStreamIndex"],
        "SubtitleScores": scores,
        "SupportsTranscoding": source["SupportsTranscoding"],
        "SupportsDirectStream": source["SupportsDirectStream"],
    })
}

/// Merges `changes` into `user_id`'s stored configuration.
async fn configure(f: &Fixture, user_id: &str, changes: &Value) {
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{user_id}"),
        Some(&f.admin),
        None,
    )
    .await;
    let mut config = user["Configuration"].clone();
    for (key, value) in changes.as_object().expect("object") {
        config[key] = value.clone();
    }
    let (status, body) = call(
        &f.router,
        "POST",
        &format!("/Users/{user_id}/Configuration"),
        Some(&f.admin),
        Some(config),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn item_media_sources_pick_the_users_default_streams() {
    let f = fixture().await;

    // Stock configuration (`SubtitleMode: Default`, no language preference,
    // `PlayDefaultAudioTrack`): the default-flagged English audio and
    // subtitle. `SetSubtitleStreamScores` scores only the subtitles the mode
    // would load (`MediaStreamSelector.cs:112-118`: in `Default`, the
    // external, default and forced ones), so the French track has none.
    let detail = detail_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(detail["DefaultAudioStreamIndex"], 1, "{detail}");
    assert_eq!(detail["DefaultSubtitleStreamIndex"], 3, "{detail}");
    assert!(
        detail["MediaStreams"][3]["Score"].is_number(),
        "{}",
        detail["MediaStreams"][3]
    );
    assert!(
        detail["MediaStreams"][4].get("Score").is_none(),
        "{}",
        detail["MediaStreams"][4]
    );
    let playback = playback_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(per_user_fields(&detail), per_user_fields(&playback));
    let listed = list_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(per_user_fields(&listed), per_user_fields(&playback));

    // French preferences, given as two-letter codes the localization manager
    // resolves to the streams' three-letter ones, subtitles always on, and
    // the default-flagged audio no longer preferred (with it, the English
    // track wins whatever the language: `MediaStreamSelector.cs:18-26`).
    configure(
        &f,
        &f.admin_id,
        &json!({
            "PlayDefaultAudioTrack": false,
            "AudioLanguagePreference": "fr",
            "SubtitleLanguagePreference": "fr",
            "SubtitleMode": "Always",
        }),
    )
    .await;
    let detail = detail_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(detail["DefaultAudioStreamIndex"], 2, "{detail}");
    assert_eq!(detail["DefaultSubtitleStreamIndex"], 4, "{detail}");
    let playback = playback_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(per_user_fields(&detail), per_user_fields(&playback));
    let listed = list_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(per_user_fields(&listed), per_user_fields(&playback));
}

#[tokio::test]
async fn item_media_sources_follow_the_users_playback_policy() {
    let f = fixture().await;
    let (status, created) = call(
        &f.router,
        "POST",
        "/Users/New",
        Some(&f.admin),
        Some(json!({ "Name": "viewer", "Password": "viewer-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let viewer_id = created["Id"].as_str().expect("id").to_owned();
    let (_, user) = call(
        &f.router,
        "GET",
        &format!("/Users/{viewer_id}"),
        Some(&f.admin),
        None,
    )
    .await;
    let mut policy = user["Policy"].clone();
    policy["EnableVideoPlaybackTranscoding"] = json!(false);
    policy["EnablePlaybackRemuxing"] = json!(false);
    let (status, body) = call(
        &f.router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        Some(&f.admin),
        Some(policy),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (viewer, _) = login(&f.router, "viewer", "viewer-pw").await;

    // `MediaSourceManager.cs:413-417`: a video source's `SupportsTranscoding`
    // and `SupportsDirectStream` are the user's permissions.
    let detail = detail_source(&f, &viewer, &viewer_id).await;
    assert_eq!(detail["SupportsTranscoding"], false, "{detail}");
    assert_eq!(detail["SupportsDirectStream"], false, "{detail}");
    let playback = playback_source(&f, &viewer, &viewer_id).await;
    assert_eq!(per_user_fields(&detail), per_user_fields(&playback));
    let listed = list_source(&f, &viewer, &viewer_id).await;
    assert_eq!(per_user_fields(&listed), per_user_fields(&playback));

    // The administrator's own view of the same item is unaffected.
    let detail = detail_source(&f, &f.admin, &f.admin_id).await;
    assert_eq!(detail["SupportsTranscoding"], true, "{detail}");
    assert_eq!(detail["SupportsDirectStream"], true, "{detail}");
}
