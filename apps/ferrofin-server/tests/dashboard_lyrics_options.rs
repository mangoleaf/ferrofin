//! Saved lyric destinations follow live library and metadata-root settings.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, http::Uri};

use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

const CLIENT: &str =
    r#"MediaBrowser Client="lyrics-test", Device="fixture", DeviceId="lyrics-test", Version="1""#;

struct Api {
    client: reqwest::Client,
    base: String,
    auth: String,
}

impl Api {
    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: &Value) {
        self.client
            .post(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .json(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    async fn upload(&self, item: &str, text: &str) {
        self.client
            .post(format!(
                "{}/Audio/{item}/Lyrics?fileName=lyrics.lrc",
                self.base
            ))
            .header("Authorization", &self.auth)
            .header("Content-Type", "text/plain")
            .body(format!("[00:01.00]{text}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let lyrics = self.get(&format!("/Audio/{item}/Lyrics")).await;
        assert_eq!(lyrics["Lyrics"][0]["Text"], text, "{lyrics}");
    }

    async fn delete_lyrics(&self, item: &str) {
        self.client
            .delete(format!("{}/Audio/{item}/Lyrics", self.base))
            .header("Authorization", &self.auth)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
}

async fn lyric_provider(State(requests): State<Arc<Mutex<Vec<String>>>>, uri: Uri) -> Json<Value> {
    requests.lock().unwrap().push(uri.to_string());
    Json(
        json!({"id":42,"trackName":"Song","artistName":"Artist","albumName":"Album", "duration":60,
        "syncedLyrics":"[00:01.00]scheduled download"}),
    )
}

fn probe_stub(root: &Path, tool: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(tool);
    std::fs::write(&path, format!(r#"#!/bin/sh
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
{{"streams":[{{"index":0,"codec_type":"audio","codec_name":"flac","sample_rate":"48000","channels":2}}],"format":{{"format_name":"flac","duration":"60.0","size":"1024","bit_rate":"1000","tags":{{"title":"Song","album":"Album","artist":"Artist","album_artist":"Artist"}}}}}}
EOF
fi
"#)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn internal_sidecar(root: &Path, item: &str) -> PathBuf {
    let id = uuid::Uuid::parse_str(item).unwrap().simple().to_string();
    root.join("library")
        .join(&id[..2])
        .join(id)
        .join("Song.lrc")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn saved_lyrics_destinations_change_without_restarting() {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("music/Artist/Album");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("Song.flac"), b"fixture").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(lyric_provider)
        .with_state(requests.clone());
    let mock = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = Config {
        port,
        ffmpeg_path: Some(probe_stub(tmp.path(), "ffmpeg")),
        ffprobe_path: Some(probe_stub(tmp.path(), "ffprobe")),
        provider_endpoints: ProviderEndpoints {
            lrclib: Some(endpoint),
            ..Default::default()
        },
        ..Config::test_stub(tmp.path())
    };
    let server = std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ferrofin_server::run(config))
            .unwrap();
    });
    let mut api = Api {
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap(),
        base: format!("http://127.0.0.1:{port}"),
        auth: CLIENT.to_owned(),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(response) = api
            .client
            .get(format!("{}/System/Info/Public", api.base))
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        assert!(Instant::now() < deadline, "server readiness");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let login: Value = api
        .client
        .post(format!("{}/Users/AuthenticateByName", api.base))
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
    api.auth = format!(
        "{CLIENT}, Token=\"{}\"",
        login["AccessToken"].as_str().unwrap()
    );
    let user = login["User"]["Id"].as_str().unwrap();
    let mut policy = api.get(&format!("/Users/{user}")).await["Policy"].clone();
    policy["EnableLyricManagement"] = json!(true);
    api.post(&format!("/Users/{user}/Policy"), &policy).await;
    let mut options = json!({
        "PathInfos":[{"Path":tmp.path().join("music")}], "EnableRealtimeMonitor":false,
        "SaveLyricsWithMedia":false,"MetadataSavers":[],
        "DisabledLyricFetchers":["LrcLib Lyrics"],
        "TypeOptions":[
            {"Type":"Audio","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"MusicAlbum","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"MusicArtist","MetadataFetchers":[],"ImageFetchers":[]}
        ]
    });
    api.post(
        "/Library/VirtualFolders?name=Music&collectionType=music&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    let library = api.get("/Library/VirtualFolders").await[0]["ItemId"].clone();
    api.post("/Library/Refresh", &Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let item = loop {
        let items = api
            .get("/Items?recursive=true&includeItemTypes=Audio")
            .await;
        if let Some(item) = items["Items"].as_array().unwrap().first() {
            break item["Id"].as_str().unwrap().to_owned();
        }
        assert!(Instant::now() < deadline, "audio discovery: {items}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let root_a = tmp.path().join("lyrics-a");
    let root_b = tmp.path().join("lyrics-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    let mut server_config = api.get("/System/Configuration").await;
    server_config["MetadataPath"] = json!(root_a);
    api.post("/System/Configuration", &server_config).await;
    let adjacent = media.join("Song.lrc");
    let internal_a = internal_sidecar(&root_a, &item);
    api.upload(&item, "internal").await;
    assert!(internal_a.is_file());
    assert!(!adjacent.exists());
    api.delete_lyrics(&item).await;
    options["SaveLyricsWithMedia"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    api.upload(&item, "adjacent").await;
    assert!(adjacent.is_file());
    assert!(!internal_a.exists());
    api.delete_lyrics(&item).await;
    options["SaveLyricsWithMedia"] = json!(false);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    server_config["MetadataPath"] = json!(root_b);
    api.post("/System/Configuration", &server_config).await;
    let internal_b = internal_sidecar(&root_b, &item);
    api.upload(&item, "new root").await;
    assert!(internal_b.is_file());
    assert!(!internal_a.exists());
    assert!(!adjacent.exists());
    api.delete_lyrics(&item).await;
    options["SaveLyricsWithMedia"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    std::fs::create_dir(&adjacent).unwrap();
    api.upload(&item, "fallback").await;
    assert_eq!(
        std::fs::read_to_string(internal_b).unwrap(),
        "[00:01.00]fallback"
    );
    std::fs::remove_dir(&adjacent).unwrap();
    api.delete_lyrics(&item).await;
    // Disabled automatic providers remain available through manual search,
    // matching LyricsController's Audio overload in the pinned source.
    options["DisabledLyricFetchers"] = json!(["lRcLiB LyRiCs"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let manual = api.get(&format!("/Audio/{item}/RemoteSearch/Lyrics")).await;
    assert!(!manual.as_array().unwrap().is_empty(), "{manual}");
    let before = requests.lock().unwrap().len();
    run_lyric_task(&api).await;
    assert_eq!(
        requests.lock().unwrap().len(),
        before,
        "disabled automatic provider ran"
    );
    assert!(!adjacent.exists());
    options["DisabledLyricFetchers"] = json!([]);
    options["LyricFetcherOrder"] = json!(["LrcLib Lyrics"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    run_lyric_task(&api).await;
    assert!(
        adjacent.is_file(),
        "the live task selection did not download lyrics"
    );
    assert_eq!(
        api.get(&format!("/Audio/{item}/Lyrics")).await["Lyrics"][0]["Text"],
        "scheduled download"
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "/api/get/42")
    );
    api.delete_lyrics(&item).await;
    options["DisabledLyricFetchers"] = json!(["LRCLIB LYRICS"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let before = requests.lock().unwrap().len();
    run_lyric_task(&api).await;
    assert_eq!(
        requests.lock().unwrap().len(),
        before,
        "selection was cached after disabling again"
    );
    assert!(!adjacent.exists());
    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    mock.abort();
}

async fn run_lyric_task(api: &Api) {
    let tasks = api.get("/ScheduledTasks").await;
    let task = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["Key"] == "DownloadLyrics")
        .unwrap();
    let id = task["Id"].as_str().unwrap();
    let previous = task["LastExecutionResult"]["EndTimeUtc"].clone();
    let started = Instant::now();
    api.post(&format!("/ScheduledTasks/Running/{id}"), &Value::Null)
        .await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let task = api.get(&format!("/ScheduledTasks/{id}")).await;
        let result = &task["LastExecutionResult"];
        if task["State"] == "Idle"
            && !result["EndTimeUtc"].is_null()
            && result["EndTimeUtc"] != previous
        {
            assert_eq!(result["Status"], "Completed", "{task}");
            println!(
                "lyric task completed in {:.2} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "lyric task did not complete: {task}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
