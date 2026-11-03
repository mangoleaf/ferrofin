//! gl-325: a refresh of an item with no file of its own, which the provider
//! manager refreshes rather than the library scan, re-derives the parental
//! score the item stores. Jellyfin's `MetadataService.BeforeSave` runs
//! `BaseItem.OnMetadataChanged` on every refresh: the item's custom, else
//! official, rating, each looked up along its display chain, scored against
//! its `GetPreferredMetadataCountryCode()`.
//!
//! Boots the composition root over a temp database and scans a TV library
//! whose episode sits directly in its series folder, so the scan gives the
//! series a season with no path, and whose series NFO rates the show TV-14.
//! The season's stored score is then cleared, as a database an older rating
//! table scored, or one adopted from Jellyfin, can leave it. The season is
//! refreshed through `POST /Items/{id}/Refresh` the way jellyfin-web's
//! "Refresh metadata" sends it, and its stored score is read back.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_core::item_persistence_service::FerrofinItemPersistenceService;
use ferrofin_core::item_repository::FerrofinItemRepository;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_db::Database;
use ferrofin_model::data::BaseItemKind;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "parental-score-pw";
const AUTH: &str = r#"MediaBrowser Client="Parental Score", Device="Test Rig", DeviceId="rig-ps", Version="1.0.0""#;

/// jellyfin-web's "Refresh metadata" → "Search for missing metadata".
const FULL_REFRESH: &str = "MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=false&ReplaceAllImages=false";

/// A booted server over a temp data dir, and what the test drives it with.
struct Harness {
    wired: WiredApp,
    items: Arc<dyn ItemRepository>,
    persistence: FerrofinItemPersistenceService,
    router: axum::Router,
    auth: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root. [`Config::test_stub`] points every remote
/// provider at a closed local port, and the library below enables none, so
/// the scan and the refresh never leave the machine.
async fn boot(temp: tempfile::TempDir) -> Harness {
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-parental-score".to_owned(),
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
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
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
    let auth = authenticate(&router).await;
    Harness {
        items: Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        persistence: FerrofinItemPersistenceService::new(db),
        wired,
        router,
        auth,
        _temp: temp,
    }
}

/// Authenticates the seeded admin; returns the token-bearing auth header.
async fn authenticate(router: &axum::Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, AUTH)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK, "admin authenticates");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let token = body["AccessToken"].as_str().expect("token");
    format!(r#"{AUTH}, Token="{token}""#)
}

impl Harness {
    /// Sends `method uri` as the admin with an optional JSON body; returns
    /// the status.
    async fn send(&self, method: &str, uri: &str, body: Option<serde_json::Value>) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, &self.auth);
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        self.router
            .clone()
            .oneshot(request.expect("request"))
            .await
            .expect("response")
            .status()
    }

    /// Adds a TV library over `media` with every remote fetcher off, and
    /// scans it.
    async fn add_tv_library_and_scan(&self, media: &Path) {
        let type_options = ["Series", "Season", "Episode"].map(
            |kind| serde_json::json!({ "Type": kind, "MetadataFetchers": [], "ImageFetchers": [] }),
        );
        let options = serde_json::json!({ "LibraryOptions": {
            "PathInfos": [{ "Path": media }],
            "EnableRealtimeMonitor": false,
            "EnableInternetProviders": false,
            "EnableChapterImageExtraction": false,
            "EnableTrickplayImageExtraction": false,
            "TypeOptions": type_options,
        }});
        let status = self
            .send(
                "POST",
                "/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=false",
                Some(options),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "library added");
        assert!(
            self.wired
                .state
                .library
                .run_library_scan(ScanTrigger::Api, None)
                .await
                .expect("scan"),
            "the scan ran"
        );
    }

    /// The stored `kind` row named `name`.
    async fn row(
        &self,
        kind: BaseItemKind,
        name: &str,
    ) -> ferrofin_db::entities::base_items::BaseItemEntity {
        self.items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![kind],
                name: Some(name.to_owned()),
                ..InternalItemsQuery::default()
            })
            .await
            .expect("query")
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("a {kind:?} row named {name}"))
    }
}

/// Polls `probe` until it returns `Some`, for at most 60 s.
async fn wait_for<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A season with no path inherits its series' TV-14 through its display
/// chain (`OfficialRatingForComparison`), so its refresh stores TV-14's US
/// score, 14 (the score the scan gave it), over the one it held.
#[tokio::test(flavor = "multi_thread")]
async fn refreshing_a_pathless_season_rederives_its_stored_parental_score() {
    let temp = tempfile::tempdir().expect("temp dir");
    let tv = temp.path().join("tv");
    let show = tv.join("Zqx Show");
    std::fs::create_dir_all(&show).expect("show dir");
    std::fs::write(
        show.join("tvshow.nfo"),
        "<tvshow><title>Zqx Show</title><mpaa>TV-14</mpaa></tvshow>",
    )
    .expect("nfo");
    std::fs::write(show.join("Zqx Show S01E01.mkv"), b"").expect("episode");
    let h = boot(temp).await;
    h.add_tv_library_and_scan(&tv).await;

    let series = h.row(BaseItemKind::Series, "Zqx Show").await;
    assert_eq!(series.official_rating.as_deref(), Some("TV-14"));
    let mut season = h.row(BaseItemKind::Season, "Season 1").await;
    assert!(season.path.is_none(), "the scan made a season with no path");
    assert_eq!(
        season.official_rating, None,
        "the season has no rating of its own"
    );
    assert_eq!(
        season.inherited_parental_rating_value,
        Some(14),
        "the scan scores the season by its series' rating"
    );
    let scanned = (
        season.inherited_parental_rating_value,
        season.inherited_parental_rating_sub_value,
    );

    // A score the season's rating no longer gives.
    season.inherited_parental_rating_value = None;
    season.inherited_parental_rating_sub_value = None;
    h.persistence
        .save_items(std::slice::from_ref(&season))
        .await
        .expect("store a stale score");

    let id = uuid::Uuid::parse_str(&season.id).expect("id");
    let before = season.date_last_refreshed;
    assert_eq!(
        h.send("POST", &format!("/Items/{id}/Refresh?{FULL_REFRESH}"), None)
            .await,
        StatusCode::NO_CONTENT,
        "refresh queued"
    );
    let refreshed = wait_for("the season's refresh to be saved", async || {
        let row = h.items.retrieve_item(id).await.ok()??;
        (row.date_last_refreshed != before).then_some(row)
    })
    .await;
    assert_eq!(
        (
            refreshed.inherited_parental_rating_value,
            refreshed.inherited_parental_rating_sub_value,
        ),
        scanned,
        "the refresh re-derives the season's stored score from its series' rating"
    );
}
