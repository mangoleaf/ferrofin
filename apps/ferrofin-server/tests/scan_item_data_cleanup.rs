//! A library scan's removals take the item's data with them, as upstream's
//! `LibraryManager.DeleteItem` does for the children `Folder.ValidateChildren`
//! drops: the metadata folders, then the extracted data (trickplay tiles in
//! both locations, chapter images, subtitle and attachment caches), then the
//! rows. And the boot sweeps once what earlier versions left behind
//! (`CleanupOrphanedExternalData`).
//!
//! Boots the real composition root ([`ferrofin_server::state::build_app_state`])
//! over a temp data dir:
//!
//! - folders planted before the first boot for an id no item has go with
//!   that boot, and a folder that is not `{xx}/{id}` stays;
//! - with two libraries added over the real router and scanned, and every
//!   folder an item's data can live in given to each scanned item, a movie
//!   whose file is deleted loses all of them on the next scan, and the movie
//!   beside it keeps its own; a removed library's episode loses all of them
//!   on the next scan, the trickplay folder beside its file included, though
//!   the file stays; a movie in its own folder loses the trickplay folder in
//!   it though the folder stays; a movie whose file changes container under the same
//!   name loses its own data but not the trickplay folder beside its media,
//!   which the new file's item shares; and a genre that no item has any more
//!   loses its art folder with its row.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use uuid::Uuid;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "cleanup-admin-pw";

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

    /// The server's data directory (`{program data}/data`).
    fn data(&self) -> PathBuf {
        self.path("data/data")
    }

    /// The internal metadata directory (`{program data}/metadata`).
    fn metadata(&self) -> PathBuf {
        self.path("data/metadata")
    }
}

/// Writes a small file (and its directories).
fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dir");
    }
    std::fs::write(path, b"x").expect("write");
}

/// `{root}/{id[..2]}/{id}` in the spelling `id` is given in.
fn indexed(root: &Path, id: &str) -> PathBuf {
    root.join(&id[..2]).join(id)
}

/// Boots over a temp dir holding the media, after `plant` has written into
/// it.
async fn boot(plant: impl FnOnce(&Path)) -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-scan-cleanup".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    for path in [
        "movies/Kilo.mkv",
        "movies/Lima.mkv",
        "movies/Mike.mkv",
        "movies/November (2003)/November (2003).mkv",
        "shows/Show/Season 1/Show S01E01.mkv",
    ] {
        touch(&temp.path().join(path));
    }
    // Kilo's genre: no other item has it, so it dies with Kilo.
    std::fs::write(
        temp.path().join("movies/Kilo.nfo"),
        "<movie><title>Kilo</title><genre>Zydeco</genre></movie>",
    )
    .expect("nfo");
    plant(temp.path());
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
    Harness { wired, temp }
}

/// Sends one request as `token` and returns status + body.
async fn call(router: &axum::Router, method: &str, uri: &str, token: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            header::AUTHORIZATION,
            format!(
                r#"MediaBrowser Token="{token}", Client="test", Device="d", DeviceId="cleanup", Version="1""#
            ),
        )
        .body(Body::empty())
        .expect("request");
    let res = router.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Authenticates the administrator and returns its token.
async fn login(router: &axum::Router) -> String {
    let res = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(
                    header::AUTHORIZATION,
                    r#"MediaBrowser Client="test", Device="d", DeviceId="cleanup", Version="1""#,
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "the administrator authenticates"
    );
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let v: Value = serde_json::from_slice(&bytes).expect("json");
    v["AccessToken"].as_str().expect("token").to_owned()
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

/// The scanned movies and episodes by `Path`.
async fn ids_by_path(
    router: &axum::Router,
    token: &str,
) -> std::collections::HashMap<String, Uuid> {
    let (status, items) = call(
        router,
        "GET",
        "/Items?recursive=true&includeItemTypes=Movie,Episode&fields=Path",
        token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{items}");
    items["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .filter_map(|i| {
            Some((
                i["Path"].as_str()?.to_owned(),
                Uuid::parse_str(i["Id"].as_str()?).ok()?,
            ))
        })
        .collect()
}

async fn scan(harness: &Harness) {
    assert!(
        harness
            .wired
            .state
            .library
            .run_library_scan(ScanTrigger::Api, None)
            .await
            .expect("scan"),
        "the scan ran to its end"
    );
}

/// Every folder an item's data lives in, each given a file: its art
/// folder, its internal metadata folder with chapter images, its trickplay
/// tiles in both locations, and its subtitle and attachment caches.
fn plant_item_data(h: &Harness, id: Uuid, media: &str) -> Vec<PathBuf> {
    let (hyphenated, dashless) = (id.hyphenated().to_string(), id.simple().to_string());
    let media = h.path(media);
    let beside = media.with_extension("trickplay");
    let folders = vec![
        h.metadata().join("library").join(hyphenated.to_uppercase()),
        indexed(&h.metadata().join("library"), &dashless).join("chapters"),
        indexed(&h.data().join("trickplay"), &hyphenated),
        beside,
        indexed(&h.data().join("subtitles"), &hyphenated),
        indexed(&h.data().join("attachments"), &hyphenated),
    ];
    for folder in &folders {
        touch(&folder.join("file"));
    }
    folders
}

/// `CleanupOrphanedExternalData` at boot: each root's `{xx}/{id}` folder
/// for an id no item has goes (the metadata root's `chapters` only), and a
/// folder that is not one stays.
#[tokio::test]
async fn the_first_boot_sweeps_extracted_data_no_item_owns() {
    let orphan = Uuid::new_v4();
    let (hyphenated, dashless) = (orphan.hyphenated().to_string(), orphan.simple().to_string());
    let roots = [
        "data/data/trickplay",
        "data/data/subtitles",
        "data/data/attachments",
    ];
    let h = boot(|dir| {
        for root in roots {
            touch(&indexed(&dir.join(root), &hyphenated).join("file"));
            touch(
                &dir.join(root)
                    .join("not-an-id")
                    .join(&hyphenated)
                    .join("file"),
            );
        }
        touch(&indexed(&dir.join("data/metadata/library"), &dashless).join("chapters/1.jpg"));
    })
    .await;
    for root in roots {
        assert!(
            !indexed(&h.path(root), &hyphenated).exists(),
            "{root}: the orphan's folder is swept"
        );
        assert!(
            h.path(root).join("not-an-id").join(&hyphenated).exists(),
            "{root}: a folder under no two-character folder stays"
        );
    }
    let metadata = indexed(&h.metadata().join("library"), &dashless);
    assert!(
        !metadata.join("chapters").exists(),
        "its chapter images are swept"
    );
    assert!(metadata.exists(), "its metadata folder stays");
}

#[tokio::test]
async fn scan_removals_delete_the_items_extracted_data_and_metadata_folders() {
    let h = boot(|_| {}).await;
    let router = ferrofin_api::create_router(h.wired.state.clone());
    let admin = login(&router).await;
    for (name, kind, dir) in [
        ("Movies", "movies", "movies"),
        ("Shows", "tvshows", "shows"),
    ] {
        let path = encode(&h.path(dir).to_string_lossy());
        let (status, body) = call(
            &router,
            "POST",
            &format!(
                "/Library/VirtualFolders?name={name}&collectionType={kind}&paths={path}&refreshLibrary=false"
            ),
            &admin,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{name}: {body}");
    }
    scan(&h).await;
    let ids = ids_by_path(&router, &admin).await;
    let id = |relative: &str| -> Uuid {
        *ids.get(&h.path(relative).to_string_lossy().into_owned())
            .unwrap_or_else(|| panic!("{relative} scanned: {ids:?}"))
    };
    let kilo = plant_item_data(&h, id("movies/Kilo.mkv"), "movies/Kilo.mkv");
    let lima = plant_item_data(&h, id("movies/Lima.mkv"), "movies/Lima.mkv");
    let mut mike = plant_item_data(&h, id("movies/Mike.mkv"), "movies/Mike.mkv");
    let november_path = "movies/November (2003)/November (2003).mkv";
    let november = plant_item_data(&h, id(november_path), november_path);
    let episode_path = "shows/Show/Season 1/Show S01E01.mkv";
    let episode = plant_item_data(&h, id(episode_path), episode_path);
    let (status, genres) = call(&router, "GET", "/Genres?recursive=true", &admin).await;
    assert_eq!(status, StatusCode::OK, "{genres}");
    let genre = genres["Items"]
        .as_array()
        .expect("Items")
        .iter()
        .find(|g| g["Name"] == "Zydeco")
        .and_then(|g| Uuid::parse_str(g["Id"].as_str()?).ok())
        .unwrap_or_else(|| panic!("Kilo's genre is listed: {genres}"));
    let genre_art = h
        .metadata()
        .join("library")
        .join(genre.hyphenated().to_string().to_uppercase());
    touch(&genre_art.join("folder.jpg"));

    // A movie whose file is gone: the scan removes it and all its data, and
    // its genre, which no other item has, with its art. A movie whose file
    // changed container keeps the trickplay folder beside it (`Mike.trickplay`
    // is the new `Mike.mp4`'s too) and loses the rest.
    // November's folder stays, planned as a plain folder, whose own
    // trickplay folder would have the same name; it has none, being no video.
    std::fs::remove_file(h.path("movies/Kilo.mkv")).expect("delete the file");
    std::fs::remove_file(h.path(november_path)).expect("delete the file");
    std::fs::rename(h.path("movies/Mike.mkv"), h.path("movies/Mike.mp4")).expect("remux");
    scan(&h).await;
    for folder in kilo.iter().chain(&november) {
        assert!(
            !folder.exists(),
            "{} removed with the movie",
            folder.display()
        );
    }
    assert!(!genre_art.exists(), "the dead genre's art goes with it");
    let shared = h.path("movies/Mike.trickplay");
    assert!(
        shared.join("file").exists(),
        "the trickplay folder beside Mike's media is the new file's too"
    );
    mike.retain(|folder| *folder != shared);
    for folder in &mike {
        assert!(
            !folder.exists(),
            "{} removed with the old Mike",
            folder.display()
        );
    }
    for folder in lima.iter().chain(&episode) {
        assert!(folder.exists(), "{} kept", folder.display());
    }

    // A removed library: its items go on the next scan, with their data.
    let (status, body) = call(
        &router,
        "DELETE",
        "/Library/VirtualFolders?name=Shows&refreshLibrary=false",
        &admin,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    scan(&h).await;
    for folder in &episode {
        assert!(
            !folder.exists(),
            "{} removed with the library's episode",
            folder.display()
        );
    }
    assert!(h.path(episode_path).exists(), "the media itself stays");
    for folder in &lima {
        assert!(folder.exists(), "{} kept", folder.display());
    }
}
