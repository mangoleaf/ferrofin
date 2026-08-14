//! First-Light integration test — the real client flow, end to end.
//!
//! Boots the actual composition root ([`ferrofin_server::state::build_app_state`])
//! over a fresh temp database seeded with a default administrator (via the real
//! [`ferrofin_server::seed::seed_default_admin`]) and a couple of scanned movie
//! items, then drives the exact sequence a Jellyfin client performs on first
//! contact — via `tower`'s [`ServiceExt::oneshot`] against the real
//! [`ferrofin_api::create_router`] (no network sockets):
//!
//! 1. `GET  /System/Info/Public` — anonymous, `200`.
//! 2. `POST /Users/AuthenticateByName` — `200` → `AuthenticationResult`.
//! 3. `GET  /Users/Me` — with the session token, `200`.
//! 4. `GET  /Items` — `200` → the seeded items.
//! 5. `GET`/`POST /Items/{id}/PlaybackInfo` — `200` → `MediaSources`.
//! 6. `GET  /Videos/{id}/stream` with a `Range` header — `206` + `Content-Range`.
//!
//! This exercises the whole vertical: config → DB + migrations → manager wiring →
//! seeding → routing → auth → library query → media-source resolution → static
//! file serving.
//!
//! Port note: Jellyfin's `AuthenticationResult` carries the minted `AccessToken`
//! in its body, and Ferrofin's `SessionManager` now returns it too (via
//! `AuthenticationResultData`). So this test reads the token straight out of the
//! `AuthenticateByName` response body's `AccessToken`, presents it on the
//! subsequent authenticated requests, and cross-checks it against the token
//! persisted on the session's `Device` row through the wired `DeviceManager` —
//! proving the client-facing token actually works end to end (the bug: this field
//! used to serialize as `null`, so no client could ever authenticate).

use std::io::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::devices::DeviceQuery;
use ferrofin_traits::persistence::ItemPersistenceService;
use tower::ServiceExt as _;
use uuid::Uuid;

/// The seeded administrator's credentials (mirrors a configured fresh install).
const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "first-light-pw";

/// A booted server plus the handles the test drives it with.
struct Harness {
    /// The wired application state (managers over the temp DB).
    wired: WiredApp,
    /// The temp media file backing the first item's direct-play path.
    media_path: String,
    /// The first (playable) item's id.
    movie_id: Uuid,
    /// The second seeded item's id (so `GET /Items` returns more than one).
    second_id: Uuid,
    /// Kept alive so the temp directory (DB file + media file) is not deleted.
    _temp: tempfile::TempDir,
}

/// Builds a bootstrap [`Config`] whose data/config/cache dirs live under `root`.
fn test_config(root: &std::path::Path) -> Config {
    Config {
        data_dir: root.join("data"),
        config_dir: root.join("config"),
        cache_dir: root.join("cache"),
        web_dir: root.join("web"),
        bind_addr: "127.0.0.1".parse().unwrap(),
        port: 0,
        https_port: 0,
        published_url: None,
        base_url: String::new(),
        omdb_api_key: String::new(),
        studios_repo_url: String::new(),
        tvdb_api_key: String::new(),
        tvdb_subscriber_pin: String::new(),
        fanart_personal_api_key: String::new(),
        musicbrainz_base_url: String::new(),
        ffmpeg_path: None,
        ffprobe_path: None,
        library_roots: Vec::new(),
        server_name: "ferrofin-first-light".to_owned(),
        log_level: "info".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        db_pool: None,
        enable_metrics: None,
        metrics_sample_interval: None,
        scan_progress_every: None,
        wasm_call_timeout_secs: None,
        wasm_memory_limit_mb: None,
        wasm_event_queue_capacity: None,
        wasm_private_http_allow: None,
        max_plugin_download_mb: None,
    }
}

/// A minimal scanned `Movie` [`BaseItemEntity`] with an on-disk `path`.
///
/// The `type_` is the stored `BaseItems.Type` name (as the scanner writes it) so
/// kind-filtered queries would resolve it; `path` points at a real file so
/// direct-play can serve it.
fn movie_item(id: Uuid, name: &str, path: &str) -> BaseItemEntity {
    let type_name = ferrofin_core::item_type_lookup::stored_type_name(
        ferrofin_model::data::BaseItemKind::Movie,
    )
    .expect("Movie has a stored type name")
    .to_owned();
    BaseItemEntity {
        id: ferrofin_db::store::guid_to_db(id),
        name: Some(name.to_owned()),
        path: Some(path.to_owned()),
        media_type: Some("Video".to_owned()),
        is_movie: true,
        type_: type_name,
        // Everything else is absent for a minimal scanned row.
        album: None,
        album_artists: None,
        artists: None,
        audio: None,
        channel_id: None,
        clean_name: None,
        community_rating: None,
        critic_rating: None,
        custom_rating: None,
        data: None,
        date_created: None,
        date_last_media_added: None,
        date_last_refreshed: None,
        date_last_saved: None,
        date_modified: None,
        end_date: None,
        episode_title: None,
        external_id: None,
        external_series_id: None,
        external_service_id: None,
        extra_type: None,
        forced_sort_name: None,
        genres: None,
        height: None,
        index_number: None,
        inherited_parental_rating_sub_value: None,
        inherited_parental_rating_value: None,
        is_folder: false,
        is_in_mixed_folder: false,
        is_locked: false,
        is_repeat: false,
        is_series: false,
        is_virtual_item: false,
        lufs: None,
        normalization_gain: None,
        official_rating: None,
        extra_ids: None,
        original_title: None,
        overview: None,
        owner_id: None,
        parent_id: None,
        parent_index_number: None,
        preferred_metadata_country_code: None,
        preferred_metadata_language: None,
        premiere_date: None,
        presentation_unique_key: None,
        primary_version_id: None,
        production_locations: None,
        production_year: None,
        run_time_ticks: None,
        season_id: None,
        season_name: None,
        series_id: None,
        series_name: None,
        series_presentation_unique_key: None,
        show_id: None,
        size: None,
        sort_name: None,
        start_date: None,
        studios: None,
        tagline: None,
        tags: None,
        top_parent_id: None,
        total_bitrate: None,
        unrated_type: None,
        width: None,
    }
}

/// Boots the composition root over a temp DB, seeds the admin + two movie items,
/// and returns the [`Harness`].
async fn boot() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let config = test_config(temp.path());

    // A real media file on disk so direct-play (`ServeFile`) has bytes to serve
    // and Range requests produce a genuine `Content-Range`.
    let media_path = temp.path().join("bunny.mp4");
    {
        let mut f = std::fs::File::create(&media_path).unwrap();
        // A non-trivial body so a `bytes=0-3` range yields a partial (< full) read.
        f.write_all(&vec![0xAB_u8; 4096]).unwrap();
    }
    let media_path = media_path.to_string_lossy().into_owned();

    // Open + migrate the real database, then wire every concrete manager. The
    // data directory must exist before SQLite can create the file there (the
    // binary's `open_database` does this; the test opens the pool directly).
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    // `build_app_state` runs against a migrated DB; migrate here (the binary's
    // `open_database` does this, but the test opens the pool directly).
    db.run_migrations().await.expect("migrations");

    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        filters: Vec::new(),
        encoders: Vec::new(),
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, shutdown_tx)
        .await
        .expect("wire app state");

    // Fresh-install seeding: the real seed path creates the admin.
    let outcome = ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    assert!(
        matches!(
            outcome,
            ferrofin_server::seed::SeedOutcome::SeededWithConfiguredPassword { .. }
        ),
        "fresh DB seeds the configured admin, got {outcome:?}"
    );

    // Seed two scanned movie items through the real persistence service (the
    // production write path). Ids avoid `1` (the query translator's placeholder).
    let movie_id = Uuid::from_u128(0x0F1);
    let second_id = Uuid::from_u128(0x0F2);
    let persistence: Arc<dyn ItemPersistenceService> = Arc::new(
        ferrofin_core::FerrofinItemPersistenceService::new(db.clone()),
    );
    persistence
        .save_items(&[
            movie_item(movie_id, "Big Buck Bunny", &media_path),
            // The second item has no on-disk path; it still shows up in `/Items`.
            movie_item(second_id, "Sintel", &media_path),
        ])
        .await
        .expect("save seeded items");

    Harness {
        wired,
        media_path,
        movie_id,
        second_id,
        _temp: temp,
    }
}

/// Reads the full body of a response as bytes.
async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec()
}

/// Parses a response body as JSON.
async fn body_json(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(response).await).expect("valid JSON body")
}

/// The `Authorization` header a Jellyfin client presents *before* it has a token
/// (client identity only — the auth-context middleware parses these into the
/// session's app/device fields at `AuthenticateByName`).
const CLIENT_AUTH: &str =
    r#"MediaBrowser Client="First Light", Device="Test Rig", DeviceId="rig-1", Version="1.0.0""#;

/// The `Authorization` header a Jellyfin client presents *after* login: the same
/// client identity plus the minted session `Token`. Modern clients carry the
/// token in this `MediaBrowser` scheme (the bare `X-Emby-Token` header is only
/// honoured when legacy authorization is enabled).
fn client_auth_with_token(token: &str) -> String {
    format!(
        r#"MediaBrowser Client="First Light", Device="Test Rig", DeviceId="rig-1", Version="1.0.0", Token="{token}""#
    )
}

/// Step 1 — `GET /System/Info/Public` is reachable anonymously and carries the
/// server identity.
async fn step_public_system_info(router: &axum::Router) {
    let public = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/System/Info/Public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        public.status(),
        StatusCode::OK,
        "public system info is reachable without auth"
    );
    let public = body_json(public).await;
    assert!(
        public.get("ServerName").is_some() || public.get("Id").is_some(),
        "public info carries server identity: {public}"
    );
}

/// The authenticated user's id plus the access token echoed in the response.
struct Authenticated {
    user_id: String,
    access_token: String,
}

/// Step 2 — `POST /Users/AuthenticateByName` authenticates the seeded admin and
/// returns an `AuthenticationResult` carrying the user, session, and — crucially —
/// a non-empty `AccessToken`. Returns the user id and the echoed token.
async fn step_authenticate(router: &axum::Router) -> Authenticated {
    let auth_req = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header(header::AUTHORIZATION, CLIENT_AUTH)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
        ))
        .unwrap();
    let auth = router.clone().oneshot(auth_req).await.unwrap();
    assert_eq!(
        auth.status(),
        StatusCode::OK,
        "the seeded admin authenticates"
    );
    let auth = body_json(auth).await;
    assert!(
        auth.get("User").is_some() && auth.get("SessionInfo").is_some(),
        "AuthenticationResult carries the user + session: {auth}"
    );
    // The regression guard: the token must be echoed in the body (it used to
    // serialize as `null`, so clients could never obtain one).
    let access_token = auth["AccessToken"]
        .as_str()
        .expect("AuthenticationResult carries an AccessToken")
        .to_owned();
    assert!(
        !access_token.is_empty(),
        "the echoed AccessToken is non-empty: {auth}"
    );
    Authenticated {
        user_id: auth["User"]["Id"]
            .as_str()
            .expect("user id in result")
            .to_owned(),
        access_token,
    }
}

/// Cross-checks the token echoed in the response body against the token persisted
/// on the session's `Device` row (via the wired `DeviceManager`) — they must be
/// the exact same value the auth layer validates against.
async fn assert_token_matches_device_row(harness: &Harness, echoed_token: &str) {
    let devices = harness
        .wired
        .state
        .devices
        .get_devices(&DeviceQuery {
            device_id: Some("rig-1".to_owned()),
            ..DeviceQuery::default()
        })
        .await
        .expect("list devices");
    let persisted = devices
        .items
        .first()
        .map(|d| d.access_token.clone())
        .expect("a device with a minted token exists");
    assert_eq!(
        echoed_token, persisted,
        "the echoed AccessToken equals the persisted Devices.AccessToken"
    );
}

/// Step 3 — `GET /Users/Me` with the session token returns the authenticated admin.
async fn step_users_me(router: &axum::Router, auth_header: &str, expected_user_id: &str) {
    let me = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Users/Me")
                .header(header::AUTHORIZATION, auth_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        me.status(),
        StatusCode::OK,
        "Users/Me with the token is 200"
    );
    let me = body_json(me).await;
    assert_eq!(
        me["Id"].as_str(),
        Some(expected_user_id),
        "Users/Me returns the authenticated admin"
    );
    assert_eq!(me["Name"].as_str(), Some(ADMIN_USER));
}

/// Step 4 — `GET /Items` returns the seeded items.
async fn step_items(router: &axum::Router, auth_header: &str, harness: &Harness) {
    let items = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Items?recursive=true")
                .header(header::AUTHORIZATION, auth_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(items.status(), StatusCode::OK, "Items list is 200");
    let items = body_json(items).await;
    let returned = items["Items"].as_array().expect("Items array");
    let ids: Vec<&str> = returned.iter().filter_map(|i| i["Id"].as_str()).collect();
    let movie = harness.movie_id.to_string();
    let second = harness.second_id.to_string();
    assert!(
        ids.iter().any(|id| id.eq_ignore_ascii_case(&movie)),
        "the playable movie is listed: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.eq_ignore_ascii_case(&second)),
        "the second seeded item is listed: {ids:?}"
    );
}

/// Step 5 — `GET`/`POST /Items/{id}/PlaybackInfo` return media sources carrying
/// the item's on-disk path.
async fn step_playback_info(router: &axum::Router, auth_header: &str, harness: &Harness) {
    // GET form.
    let playback = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{}/PlaybackInfo", harness.movie_id))
                .header(header::AUTHORIZATION, auth_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(playback.status(), StatusCode::OK, "GET PlaybackInfo is 200");
    let playback = body_json(playback).await;
    let sources = playback["MediaSources"].as_array().expect("MediaSources");
    assert!(!sources.is_empty(), "at least one media source is returned");
    assert_eq!(
        sources[0]["Path"].as_str(),
        Some(harness.media_path.as_str()),
        "the media source carries the on-disk path"
    );

    // POST form with a posted device profile.
    let posted = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{}/PlaybackInfo", harness.movie_id))
                .header(header::AUTHORIZATION, auth_header)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "DeviceProfile": {} }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(posted.status(), StatusCode::OK, "POST PlaybackInfo is 200");
    let posted = body_json(posted).await;
    assert!(
        !posted["MediaSources"].as_array().unwrap().is_empty(),
        "POST PlaybackInfo also returns media sources"
    );
}

/// Step 6 — `GET /Videos/{id}/stream` with a `Range` header direct-plays the
/// file: `206 Partial Content` + a matching `Content-Range`, serving exactly the
/// requested bytes.
async fn step_ranged_stream(router: &axum::Router, harness: &Harness) {
    let stream = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Videos/{}/stream", harness.movie_id))
                .header(header::RANGE, "bytes=0-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        stream.status(),
        StatusCode::PARTIAL_CONTENT,
        "a ranged direct-play stream is 206 Partial Content"
    );
    let content_range = stream
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    assert!(
        content_range
            .as_deref()
            .is_some_and(|cr| cr.starts_with("bytes 0-3/")),
        "Content-Range reflects the requested range: {content_range:?}"
    );
    let served = body_bytes(stream).await;
    assert_eq!(served.len(), 4, "exactly the requested 4 bytes are served");
}

#[tokio::test]
async fn first_light_client_flow() {
    let harness = boot().await;
    let router = ferrofin_api::create_router(harness.wired.state.clone());

    step_public_system_info(&router).await;
    let authed = step_authenticate(&router).await;
    // The token the client received in the body is the real, working token:
    // cross-check it against the persisted device row, then use *it* (not a
    // side-channel DB read) for every subsequent authenticated request.
    assert_token_matches_device_row(&harness, &authed.access_token).await;
    let auth_header = client_auth_with_token(&authed.access_token);
    step_users_me(&router, &auth_header, &authed.user_id).await;
    step_items(&router, &auth_header, &harness).await;
    step_playback_info(&router, &auth_header, &harness).await;
    step_ranged_stream(&router, &harness).await;
}
