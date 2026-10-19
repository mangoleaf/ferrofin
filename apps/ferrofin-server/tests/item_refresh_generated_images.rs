//! gl-576: refreshing an item redraws the Primary Ferrofin generated for it
//! from other items' artwork, as Jellyfin's `MetadataService` runs the
//! `BaseDynamicImageProvider` family on a full or replace-all refresh.
//!
//! Boots the real composition root over a temp database, scans a library
//! through `POST /Library/Refresh`, recolours the artwork the generated image
//! was drawn from, then `POST /Items/{id}/Refresh` the way jellyfin-web's
//! "Refresh metadata" → "Replace all images" sends it, and waits for the
//! generated file to carry the new colour:
//!
//! - a `Genre` (refreshed through the provider manager): its 2×2 collage;
//! - a library (`CollectionFolder`, refreshed through a scan of the library):
//!   its tile;
//! - the root (`UserRootFolder`, refreshed through a scan of every library):
//!   its copy of a library's art, which a library's refresh leaves alone;
//! - a `PhotoAlbum` (refreshed through a scan of its folder): its copy of the
//!   first photo.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_core::item_repository::FerrofinItemRepository;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_db::Database;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::ImageType;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "generated-images-pw";
const AUTH: &str = r#"MediaBrowser Client="Generated Images", Device="Test Rig", DeviceId="rig-gi", Version="1.0.0""#;

const RED: [u8; 3] = [180, 30, 30];
const BLUE: [u8; 3] = [30, 30, 180];

/// jellyfin-web's "Refresh metadata" with "Replace all images" ticked.
const REPLACE_IMAGES: &str = "MetadataRefreshMode=FullRefresh&ImageRefreshMode=FullRefresh&ReplaceAllMetadata=false&ReplaceAllImages=true";

/// A booted server over a temp data dir, and what the test drives it with.
struct Harness {
    wired: WiredApp,
    items: Arc<dyn ItemRepository>,
    router: axum::Router,
    auth: String,
    _temp: tempfile::TempDir,
}

/// Boots the composition root. [`Config::test_stub`] points every remote
/// provider at a closed local port, so the scan and the refresh never leave
/// the machine.
async fn boot(temp: tempfile::TempDir) -> Harness {
    std::fs::create_dir_all(temp.path().join("config")).expect("config dir");
    let config = Config {
        server_name: "ferrofin-generated-images".to_owned(),
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
            db,
            Arc::new(ItemTypeLookup::new()),
        )),
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
    /// Sends `method uri` as the admin; returns the status.
    async fn send(&self, method: &str, uri: &str) -> StatusCode {
        self.router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::AUTHORIZATION, &self.auth)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status()
    }

    /// Waits until no library reports a scan in flight (`RefreshStatus`), so
    /// a refresh's closing passes are done.
    async fn wait_for_libraries_idle(&self) {
        wait_for("every library to be idle", async || {
            let response = self
                .router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/Library/VirtualFolders")
                        .header(header::AUTHORIZATION, &self.auth)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .ok()?;
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .ok()?;
            let folders: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            folders
                .as_array()?
                .iter()
                .all(|f| f["RefreshStatus"].as_str() == Some("Idle"))
                .then_some(())
        })
        .await;
    }

    /// Adds a library over `media` and scans every library.
    async fn add_library_and_scan(&self, name: &str, collection_type: &str, media: &Path) {
        let paths = media.to_string_lossy().replace(' ', "%20");
        let status = self
            .send(
                "POST",
                &format!(
                    "/Library/VirtualFolders?name={name}&collectionType={collection_type}&paths={paths}&refreshLibrary=false"
                ),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "library added");
        assert_eq!(
            self.send("POST", "/Library/Refresh").await,
            StatusCode::NO_CONTENT,
            "scan queued"
        );
    }

    /// The id of the `kind` row named `name`, once the scan has made it.
    async fn row_id(&self, kind: BaseItemKind, name: &str) -> uuid::Uuid {
        wait_for(&format!("the {kind:?} row {name}"), async || {
            let row = self
                .items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![kind],
                    name: Some(name.to_owned()),
                    ..InternalItemsQuery::default()
                })
                .await
                .ok()?
                .into_iter()
                .next()?;
            uuid::Uuid::parse_str(&row.id).ok()
        })
        .await
    }

    /// The colour of the item's generated Primary at a pixel inside its
    /// first tile, once it is `want`.
    async fn wait_for_primary_colour(&self, what: &str, item: uuid::Uuid, want: [u8; 3]) {
        wait_for(what, async || {
            let images = self.items.get_image_infos(item).await.ok()?;
            let primary = images.iter().find(|i| i.image_type == ImageType::Primary)?;
            let pixel = image::open(&primary.path)
                .ok()?
                .to_rgb8()
                .get_pixel(10, 10)
                .0;
            (pixel == want).then_some(())
        })
        .await;
    }
}

/// A solid `colour` PNG at `path`.
fn write_png(path: &Path, colour: [u8; 3]) {
    image::RgbImage::from_pixel(40, 60, image::Rgb(colour))
        .save(path)
        .expect("png");
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

/// A genre refreshes through the provider manager (it has no file), which
/// runs `GenreImageProvider` forced by the full refresh: the collage is
/// resampled from the movie's recoloured poster. The library's refresh then
/// forces `CollectionFolderImageProvider` the same way, and the root's
/// `FolderImageProvider`.
#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_genres_or_librarys_images_redraws_its_collage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let media = temp.path().join("movies");
    let movie = media.join("Heat (1995)");
    std::fs::create_dir_all(&movie).expect("movie dir");
    std::fs::write(movie.join("Heat (1995).mkv"), [0u8; 1024]).expect("movie file");
    std::fs::write(
        movie.join("movie.nfo"),
        r#"<?xml version="1.0"?>
<movie><title>Heat</title><year>1995</year><genre>Crime</genre></movie>"#,
    )
    .expect("nfo");
    write_png(&movie.join("poster.png"), RED);
    let harness = boot(temp).await;
    // `GET /Items/Root` creates the user root (`GetUserRootFolder()`), so
    // the library hangs under it and the scan copies its art.
    assert_eq!(harness.send("GET", "/Items/Root").await, StatusCode::OK);
    harness
        .add_library_and_scan("Movies", "movies", &media)
        .await;

    let genre = harness.row_id(BaseItemKind::Genre, "Crime").await;
    harness
        .wait_for_primary_colour("the scan's genre collage", genre, RED)
        .await;

    write_png(&movie.join("poster.png"), BLUE);
    assert_eq!(
        harness
            .send("POST", &format!("/Items/{genre}/Refresh?{REPLACE_IMAGES}"))
            .await,
        StatusCode::NO_CONTENT
    );
    harness
        .wait_for_primary_colour("the refresh to redraw the genre collage", genre, BLUE)
        .await;

    // The library's tile, drawn by the scan from the red poster and fresh for
    // a week, is redrawn by the library's own full refresh.
    let library = harness
        .row_id(BaseItemKind::CollectionFolder, "Movies")
        .await;
    harness
        .wait_for_primary_colour("the scan's library tile", library, RED)
        .await;
    assert_eq!(
        harness
            .send(
                "POST",
                &format!("/Items/{library}/Refresh?{REPLACE_IMAGES}")
            )
            .await,
        StatusCode::NO_CONTENT
    );
    harness
        .wait_for_primary_colour("the refresh to redraw the library tile", library, BLUE)
        .await;

    // A library's refresh, once its closing passes are done, has not
    // reached the root; the root's own refresh does.
    harness.wait_for_libraries_idle().await;
    let root = harness
        .row_id(BaseItemKind::UserRootFolder, "Media Folders")
        .await;
    harness
        .wait_for_primary_colour("the scan's root image", root, RED)
        .await;
    assert_eq!(
        harness
            .send("POST", &format!("/Items/{root}/Refresh?{REPLACE_IMAGES}"))
            .await,
        StatusCode::NO_CONTENT
    );
    harness
        .wait_for_primary_colour("the refresh to recopy the root image", root, BLUE)
        .await;
    drop(harness.wired);
}

/// A photo album refreshes through a scan of its folder, which runs
/// `PhotoAlbumImageProvider` forced by the full refresh: the album's Primary
/// is copied again from its recoloured first photo.
#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_photo_albums_images_recopies_its_first_photo() {
    let temp = tempfile::tempdir().expect("temp dir");
    let media = temp.path().join("photos");
    let album = media.join("Trip");
    std::fs::create_dir_all(&album).expect("album dir");
    write_png(&album.join("a.png"), RED);
    let harness = boot(temp).await;
    harness
        .add_library_and_scan("Photos", "homevideos", &media)
        .await;

    let album_id = harness.row_id(BaseItemKind::PhotoAlbum, "Trip").await;
    harness
        .wait_for_primary_colour("the scan's album image", album_id, RED)
        .await;

    write_png(&album.join("a.png"), BLUE);
    assert_eq!(
        harness
            .send(
                "POST",
                &format!("/Items/{album_id}/Refresh?{REPLACE_IMAGES}")
            )
            .await,
        StatusCode::NO_CONTENT
    );
    harness
        .wait_for_primary_colour("the refresh to recopy the album image", album_id, BLUE)
        .await;
    drop(harness.wired);
}
