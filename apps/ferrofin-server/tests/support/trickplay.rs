//! Real server HTTP fixture with deterministic probe/extraction subprocesses.

#![allow(dead_code)] // Shared controls serve several settings-specific HTTP tests.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::{Json, Router};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

const CLIENT: &str = r#"MediaBrowser Client="trickplay-test", Device="fixture", DeviceId="trickplay-test", Version="1""#;
// A valid 2x2 PNG. The decoder guesses the image format; the real tile encoder
// writes a JPEG, so HTTP serving still traverses the production JPEG pipeline.
const FRAME: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0,
    0, 0, 253, 212, 154, 115, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 68, 12,
    16, 10, 0, 31, 238, 3, 253, 139, 95, 20, 212, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

pub struct Fixture {
    pub tmp: tempfile::TempDir,
    pub client: reqwest::Client,
    pub base: String,
    pub auth: String,
    pub library: String,
    pub movie: String,
    pub options: Value,
    pub media: PathBuf,
    pub media_file: PathBuf,
    server: std::thread::JoinHandle<()>,
    mock: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> reqwest::Response {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", &self.auth);
        if let Some(body) = body {
            request = request.json(body);
        }
        request.send().await.unwrap()
    }
    pub async fn get(&self, path: &str) -> Value {
        self.request(reqwest::Method::GET, path, None)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    pub async fn post(&self, path: &str, body: Option<&Value>) {
        self.request(reqwest::Method::POST, path, body)
            .await
            .error_for_status()
            .unwrap();
    }
    pub async fn save_options(&self) {
        self.post(
            "/Library/VirtualFolders/LibraryOptions",
            Some(&json!({"Id":self.library,"LibraryOptions":self.options})),
        )
        .await;
    }
    pub fn extractions(&self) -> usize {
        std::fs::read_to_string(self.tmp.path().join("extractions"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    pub fn hold(&self) {
        std::fs::write(self.tmp.path().join("hold"), b"hold").unwrap();
    }
    pub fn release(&self) {
        let _ = std::fs::remove_file(self.tmp.path().join("hold"));
    }
    pub async fn wait_extractions(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.extractions() < count {
            assert!(Instant::now() < deadline, "no extraction {count}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn task(&self, key: &str) -> Value {
        let tasks = self.get("/ScheduledTasks").await;
        tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["Key"] == key)
            .unwrap()
            .clone()
    }
    pub async fn run_task(&self, key: &str) {
        let before = self.task(key).await;
        self.post(
            &format!("/ScheduledTasks/Running/{}", before["Id"].as_str().unwrap()),
            None,
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let after = self.task(key).await;
            if after["State"] == "Idle"
                && after["LastExecutionResult"]["EndTimeUtc"]
                    != before["LastExecutionResult"]["EndTimeUtc"]
            {
                assert_eq!(
                    after["LastExecutionResult"]["Status"], "Completed",
                    "{after}"
                );
                return;
            }
            assert!(Instant::now() < deadline, "task did not finish: {after}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn item(&self) -> Value {
        self.get(&format!("/Items/{}?Fields=Trickplay,Etag", self.movie))
            .await
    }
    pub async fn refresh(&self, mode: &str, regenerate: bool) -> String {
        let before = self.item().await["Etag"].as_str().unwrap().to_owned();
        self.post(&format!("/Items/{}/Refresh?metadataRefreshMode={mode}&imageRefreshMode=None&regenerateTrickplay={regenerate}", self.movie), None).await;
        before
    }
    pub async fn wait_etag(&self, before: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.item().await["Etag"] == before {
            assert!(Instant::now() < deadline, "refresh did not save");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn tile(&self, width: i32) -> reqwest::Response {
        self.request(
            reqwest::Method::GET,
            &format!("/Videos/{}/Trickplay/{width}/0.jpg", self.movie),
            None,
        )
        .await
    }
    pub async fn wait_tile(&self, width: i32) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let response = self.tile(width).await;
            if response.status().is_success() {
                assert_eq!(response.headers()["content-type"], "image/jpeg");
                assert!(response.bytes().await.unwrap().starts_with(&[0xff, 0xd8]));
                return;
            }
            assert!(Instant::now() < deadline, "trickplay tile missing");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn set_behavior(&self, behavior: &str) {
        let mut config = self.get("/System/Configuration").await;
        config["TrickplayOptions"]["ScanBehavior"] = json!(behavior);
        self.post("/System/Configuration", Some(&config)).await;
    }
    pub fn sidecar_root(&self) -> PathBuf {
        self.media_file.with_extension("trickplay")
    }
    pub fn internal_root(&self) -> PathBuf {
        let id = uuid::Uuid::parse_str(&self.movie).unwrap().to_string();
        self.tmp
            .path()
            .join("data/trickplay")
            .join(&id[..2])
            .join(id)
    }
    pub fn seed_user_grid(&self) {
        let grid = self.sidecar_root().join("2 - 1x1");
        std::fs::create_dir_all(&grid).unwrap();
        std::fs::write(grid.join("0.jpg"), FRAME).unwrap();
    }
    pub async fn finish(self) {
        self.release();
        self.post("/System/Shutdown", None).await;
        self.server.join().unwrap();
        self.mock.abort();
    }
    #[allow(clippy::too_many_lines)]
    pub async fn start() -> Self {
        Self::start_with_name("Movie.mkv").await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn start_with_name(filename: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        std::fs::create_dir_all(&media).unwrap();
        let media_file = media.join(filename);
        std::fs::write(&media_file, b"video fixture").unwrap();
        std::fs::write(tmp.path().join("frame.png"), FRAME).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mock = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .fallback(|| async { Json(json!({"results":[],"data":{"token":"fixture"}})) }),
            )
            .await
            .unwrap();
        });
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = Config {
            port,
            ffmpeg_path: Some(tool_stub(tmp.path(), "ffmpeg")),
            ffprobe_path: Some(tool_stub(tmp.path(), "ffprobe")),
            musicbrainz_base_url: endpoint.clone(),
            studios_repo_url: endpoint.clone(),
            provider_endpoints: ProviderEndpoints {
                tmdb: Some(endpoint.clone()),
                tmdb_images: Some(endpoint.clone()),
                tvdb: Some(endpoint.clone()),
                omdb: Some(endpoint.clone()),
                fanart: Some(endpoint.clone()),
                audiodb: Some(endpoint.clone()),
                lrclib: Some(endpoint.clone()),
                opensubtitles: Some(endpoint),
            },
            ..Config::test_stub(tmp.path())
        };
        let server = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(ferrofin_server::run(config))
                .unwrap();
        });
        let mut fixture = Self {
            tmp,
            client: reqwest::Client::new(),
            base: format!("http://127.0.0.1:{port}"),
            auth: CLIENT.to_owned(),
            library: String::new(),
            movie: String::new(),
            options: Value::Null,
            media,
            media_file,
            server,
            mock,
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if fixture
                .client
                .get(format!("{}/System/Info/Public", fixture.base))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            assert!(Instant::now() < deadline, "server did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let login: Value = fixture
            .request(
                reqwest::Method::POST,
                "/Users/AuthenticateByName",
                Some(&json!({"Username":"admin","Pw":""})),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        fixture.auth = format!(
            "{CLIENT}, Token=\"{}\"",
            login["AccessToken"].as_str().unwrap()
        );
        let mut config = fixture.get("/System/Configuration").await;
        config["TrickplayOptions"] = json!({"WidthResolutions":[2],"TileWidth":1,"TileHeight":1,"Interval":1000,"JpegQuality":75,"Qscale":4,"ProcessThreads":1,"ProcessPriority":"BelowNormal","EnableHwAcceleration":false,"EnableHwEncoding":false,"EnableKeyFrameOnlyExtraction":false,"ScanBehavior":"Blocking"});
        fixture.post("/System/Configuration", Some(&config)).await;
        fixture.options = json!({"PathInfos":[{"Path":fixture.media}],"EnableRealtimeMonitor":false,"EnableInternetProviders":false,"SaveLocalMetadata":false,"MetadataSavers":[],"EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,"ExtractTrickplayImagesDuringLibraryScan":false,"SaveTrickplayWithMedia":false,"TypeOptions":[{"Type":"Movie","MetadataFetchers":[],"ImageFetchers":[]}]});
        fixture
            .post(
                "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=false",
                Some(&json!({"LibraryOptions":fixture.options})),
            )
            .await;
        fixture.get("/Library/VirtualFolders").await[0]["ItemId"]
            .as_str()
            .unwrap()
            .clone_into(&mut fixture.library);
        fixture.run_task("RefreshLibrary").await;
        let movies = fixture
            .get("/Items?Recursive=true&IncludeItemTypes=Movie")
            .await;
        assert_eq!(movies["Items"].as_array().unwrap().len(), 1, "{movies}");
        movies["Items"][0]["Id"]
            .as_str()
            .unwrap()
            .clone_into(&mut fixture.movie);
        fixture
    }
}

fn tool_stub(root: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(name);
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "-version" ]; then
cat <<'EOF'
{name} version 6.1.1 Copyright (c) 2000-2023 the FFmpeg developers
libavutil      58. 29.100
libavcodec     60. 31.102
libavformat    60. 16.100
libavdevice    60.  3.100
libavfilter     9. 12.100
libswscale      7.  5.100
libswresample   4. 12.100
EOF
elif [ "{name}" = "ffprobe" ]; then
cat <<'EOF'
{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264","width":2,"height":2}}],"format":{{"format_name":"matroska","duration":"2","size":"4096","bit_rate":"12800"}}}}
EOF
else
for argument do output="$argument"; done
case "$output" in
  *%08d.jpg)
    root=$(dirname "$0")
    printf 'extract\n' >> "$root/extractions"
    while [ -f "$root/hold" ]; do sleep 0.02; done
    directory=$(dirname "$output")
    mkdir -p "$directory"
    cp "$root/frame.png" "$directory/00000001.jpg"
    ;;
esac
fi
"#
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
