//! The quality a re-encoded image is written at, over the real router and
//! composition root.
//!
//! Upstream encodes an item, by-name or user image at `quality ?? 100`
//! (v12.2 `ImageController.GetImageInternal`) and the splashscreen at a fixed
//! 90 (`GetSplashscreen`). The served JPEG's quantization tables show which:
//! libjpeg's scaling, which `jpeg-encoder` uses, makes every entry 1 at
//! quality 100, and the first luminance entry 3 at quality 90.

use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use ferrofin_db::Database;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::library::ScanTrigger;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "image-quality-admin-pw";
const MOVIE: &str = "movies/Crimson Square (1963)/Crimson Square (1963).mkv";

/// Writes a 64×64 gradient PNG, so a JPEG re-encode has detail to quantize.
fn write_png(path: &Path) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    image::RgbImage::from_fn(64, 64, |x, y| {
        image::Rgb([
            u8::try_from(x * 4).expect("byte"),
            u8::try_from(y * 4).expect("byte"),
            128,
        ])
    })
    .save(path)
    .expect("png");
}

/// A booted server that scanned one movie with a `poster.png`, and a session.
struct Fixture {
    wired: ferrofin_server::state::WiredApp,
    _temp: tempfile::TempDir,
    router: axum::Router,
    token: String,
    movie: String,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        server_name: "ferrofin-image-quality".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    let movie_path = temp.path().join(MOVIE);
    std::fs::create_dir_all(movie_path.parent().expect("parent")).expect("dir");
    std::fs::write(&movie_path, b"").expect("movie");
    write_png(&movie_path.with_file_name("poster.png"));
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
    let path = encode(&temp.path().join("movies").to_string_lossy());
    let (status, _) = call(
        &router,
        "POST",
        &format!(
            "/Library/VirtualFolders?name=Movies&collectionType=movies&paths={path}&refreshLibrary=false"
        ),
        Some(&token),
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
    let items = json_of(
        call(
            &router,
            "GET",
            "/Items?recursive=true&includeItemTypes=Movie",
            Some(&token),
            None,
        )
        .await,
    );
    let movie = items["Items"][0]["Id"]
        .as_str()
        .expect("the scanned movie")
        .to_owned();
    Fixture {
        wired,
        _temp: temp,
        router,
        token,
        movie,
    }
}

#[tokio::test]
async fn item_images_encode_at_100_and_the_splashscreen_at_90() {
    // `wired` stays bound: dropping it would stop the server's services.
    let Fixture {
        wired,
        _temp,
        router,
        token,
        movie,
    } = fixture().await;
    let (status, poster) = call(
        &router,
        "GET",
        &format!("/Items/{movie}/Images/Primary?maxWidth=32&format=jpg"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let table = first_quant_table(&poster);
    assert!(
        table.iter().all(|&q| q == 1),
        "an item image without `quality` is encoded at 100: {table:?}"
    );

    // An administrator may fetch the splashscreen while it is disabled.
    let data = wired.state.config.application_paths().data_path();
    write_png(&Path::new(&data).join("splashscreen.png"));
    let (status, splash) = call(
        &router,
        "GET",
        "/Branding/Splashscreen?format=jpg",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        first_quant_table(&splash)[0],
        3,
        "the splashscreen is encoded at 90"
    );
}

/// The first quantization table (`DQT`) of a baseline JPEG, in zigzag order.
fn first_quant_table(jpeg: &[u8]) -> Vec<u8> {
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        assert_eq!(jpeg[at], 0xFF, "a marker at {at}");
        let marker = jpeg[at + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if marker == 0xDB {
            assert_eq!(jpeg[at + 4] >> 4, 0, "an 8-bit table");
            return jpeg[at + 5..at + 5 + 64].to_vec();
        }
        at += 2 + len;
    }
    panic!("no DQT segment in a {}-byte body", jpeg.len());
}

/// Sends one request as `token` and returns status + raw body.
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let ident = r#"Client="test", Device="d", DeviceId="image-quality", Version="1""#;
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
    (status, bytes)
}

/// The JSON body of a `200`.
fn json_of((status, bytes): (StatusCode, Bytes)) -> Value {
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("json")
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
