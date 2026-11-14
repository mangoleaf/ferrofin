//! "Identify → Apply" takes the artwork of the result the admin picked, end to
//! end over real HTTP: the image providers run the chosen result's provider
//! first ("When identifying, run the provider the user picked first so the
//! correct image is used", `MetadataService.GetNonLocalImageProviders`), so a
//! result from a provider ranked below another for images still brings its own
//! poster (jellyfin#14289).
//!
//! Boots the real server ([`ferrofin_server::run`]) over a one-movie library
//! that ranks TheMovieDb above OMDb for images, with both answered by an
//! in-process stand-in through [`Config::provider_endpoints`]. Each serves a
//! poster of its own size, which tells them apart.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{Uri, header};
use axum::response::IntoResponse;
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="identify-artwork-test", Device="fixture", DeviceId="identify-artwork-test", Version="1""#;

/// How long the scan gets to save the movie being waited for.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

/// TheMovieDb's poster size.
const TMDB_POSTER: (u32, u32) = (2, 3);
/// OMDb's poster size.
const OMDB_POSTER: (u32, u32) = (4, 6);

/// A PNG of `size`.
fn png((width, height): (u32, u32)) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

/// TheMovieDb and OMDb on Heat (TMDb 949, IMDb tt0113277), each with its own
/// poster.
fn answer(endpoint: &str, path: &str) -> axum::response::Response {
    let json = |body: Value| axum::Json(body).into_response();
    match path {
        "/tmdb/search/movie" => json(json!({"results":[{"id":949,"title":"Heat",
            "release_date":"1995-12-15"}]})),
        "/tmdb/find/tt0113277" => json(json!({"movie_results":[{"id":949,"title":"Heat"}]})),
        "/tmdb/movie/949" => json(json!({"id":949,"title":"Heat","imdb_id":"tt0113277",
            "overview":"From TheMovieDb.","release_date":"1995-12-15",
            "poster_path":"/tmdb-poster.png","videos":{"results":[]},
            "credits":{"cast":[],"crew":[]},"release_dates":{"results":[]}})),
        "/tmdb/movie/949/images" => json(json!({"posters":[{"file_path":"/tmdb-poster.png",
            "width":TMDB_POSTER.0,"height":TMDB_POSTER.1}],"backdrops":[],"logos":[]})),
        "/omdb/" => json(json!({"Response":"True","Title":"Heat","Year":"1995",
            "imdbID":"tt0113277","Plot":"From OMDb.","Type":"movie",
            "Poster":format!("{endpoint}/omdb-poster.png")})),
        "/images/original/tmdb-poster.png" => {
            ([(header::CONTENT_TYPE, "image/png")], png(TMDB_POSTER)).into_response()
        }
        "/omdb-poster.png" => {
            ([(header::CONTENT_TYPE, "image/png")], png(OMDB_POSTER)).into_response()
        }
        _ => (axum::http::StatusCode::NOT_FOUND, "{}").into_response(),
    }
}

/// The stub tool the server probes the fixture video with: a version banner
/// for discovery, a one-stream answer for any probe.
fn tool_stub(root: &std::path::Path, tool: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(tool);
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "-version" ]; then
cat <<'EOF'
{tool} version 6.1.1 Copyright (c) 2000-2023 the FFmpeg developers
libavutil      58. 29.100
libavcodec     60. 31.102
libavformat    60. 16.100
libavdevice    60.  3.100
libavfilter     9. 12.100
libswscale      7.  5.100
libswresample   4. 12.100
EOF
else
cat <<'EOF'
{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264","width":64,"height":64}}],"format":{{"format_name":"matroska,webm","duration":"60.0","size":"1024","bit_rate":"1000"}}}}
EOF
fi
"#
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Api {
    client: reqwest::Client,
    base: String,
    auth: String,
}

impl Api {
    /// Signs in as the seeded admin.
    async fn login(client: reqwest::Client, base: String) -> Self {
        let login: Value = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", CLIENT)
            .json(&json!({"Username":"admin","Pw":""}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["AccessToken"].as_str().unwrap();
        let auth = format!("{CLIENT}, Token=\"{token}\"");
        Self { client, base, auth }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
    }

    async fn post(&self, path: &str, body: &Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .json(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        assert!(
            status.is_success(),
            "{path}: {status}: {}",
            response.text().await.unwrap()
        );
    }

    /// The movie's id, once the scan has saved it with a poster.
    async fn await_movie_with_poster(&self) -> String {
        let deadline = Instant::now() + SCAN_DEADLINE;
        loop {
            let items: Value = self
                .get("/Items?recursive=true&includeItemTypes=Movie")
                .await
                .json()
                .await
                .unwrap();
            let movie = &items["Items"][0];
            if let (Some(id), Some(_)) =
                (movie["Id"].as_str(), movie["ImageTags"]["Primary"].as_str())
            {
                return id.to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "no movie with a poster after the scan"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The size of the movie's poster.
    async fn poster_size(&self, id: &str) -> (u32, u32) {
        let bytes = self
            .get(&format!("/Items/{id}/Images/Primary"))
            .await
            .bytes()
            .await
            .unwrap();
        let poster = image::load_from_memory(&bytes).unwrap();
        (poster.width(), poster.height())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identify_applies_the_chosen_results_poster() {
    let tmp = tempfile::tempdir().unwrap();
    let movies = tmp.path().join("movies");
    let folder = movies.join("Heat (1995)");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("Heat (1995).mkv"), b"fixture").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (log, root) = (Arc::clone(&requests), endpoint.clone());
    let stand_in = Router::new().fallback(move |uri: Uri| {
        log.lock().unwrap().push(uri.path().to_owned());
        let response = answer(&root, uri.path());
        async move { response }
    });
    let mock = tokio::spawn(async move { axum::serve(listener, stand_in).await.unwrap() });
    let ffmpeg = tool_stub(tmp.path(), "ffmpeg");
    let ffprobe = tool_stub(tmp.path(), "ffprobe");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let (server, base, _config) = boot::spawn_server(&client, |port| Config {
        port,
        ffmpeg_path: Some(ffmpeg.clone()),
        ffprobe_path: Some(ffprobe.clone()),
        provider_endpoints: ProviderEndpoints {
            tmdb: Some(format!("{endpoint}/tmdb")),
            tmdb_images: Some(format!("{endpoint}/images")),
            omdb: Some(format!("{endpoint}/omdb/")),
            ..ProviderEndpoints::offline()
        },
        // The posters come from the stand-in on loopback.
        image_private_http_allow: Some("127.0.0.1".to_owned()),
        ..Config::test_stub(tmp.path())
    })
    .await;
    let api = Api::login(client, base).await;

    // TheMovieDb ranks above OMDb for both metadata and images.
    let fetchers = json!(["TheMovieDb", "The Open Movie Database"]);
    let options = json!({"PathInfos":[{"Path":movies}],"EnableRealtimeMonitor":false,
        "MetadataSavers":[],"EnableChapterImageExtraction":false,
        "EnableTrickplayImageExtraction":false,
        "TypeOptions":[{"Type":"Movie","MetadataFetchers":fetchers,
            "MetadataFetcherOrder":fetchers,"ImageFetchers":fetchers,
            "ImageFetcherOrder":fetchers}]});
    api.post(
        "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;
    let movie = api.await_movie_with_poster().await;
    assert_eq!(
        api.poster_size(&movie).await,
        TMDB_POSTER,
        "the scan takes the first-ranked provider's poster"
    );

    // The admin picks OMDb's result and replaces the existing images.
    requests.lock().unwrap().clear();
    api.post(
        &format!("/Items/RemoteSearch/Apply/{movie}?replaceAllImages=true"),
        &json!({"Name":"Heat","ProductionYear":1995,
            "ProviderIds":{"Imdb":"tt0113277"},
            "SearchProviderName":"The Open Movie Database"}),
    )
    .await;
    assert_eq!(
        api.poster_size(&movie).await,
        OMDB_POSTER,
        "the chosen result's poster: {:?}",
        requests.lock().unwrap()
    );

    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
