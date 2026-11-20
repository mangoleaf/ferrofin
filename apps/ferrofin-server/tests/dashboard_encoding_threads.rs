//! The dashboard's encoding thread count limits the scan's probes from the next
//! start on, as upstream's `MediaEncoder.SetFFmpegPath` reads it once at
//! startup (`GetNumberOfThreads(null, options, null)`), and its default `-1`
//! never reaches `-threads`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferrofin_server::config::Config;
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="encoding-threads-test", Device="fixture", DeviceId="encoding-threads", Version="1""#;

/// How long a scan gets to probe one new file.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

/// An ffmpeg or ffprobe that appends its arguments to `argv` and answers every
/// probe with one FLAC stream.
fn probe_stub(root: &Path, tool: &str, argv: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.join(tool);
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{argv}'
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
echo '{{"streams":[{{"index":0,"codec_type":"audio","codec_name":"flac","sample_rate":"44100","channels":1}}],"format":{{"format_name":"flac","duration":"1","size":"4096","bit_rate":"12800"}}}}'
fi
"#,
        argv = argv.display()
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
    async fn login(client: &reqwest::Client, base: String) -> Self {
        let login: Value = client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", CLIENT)
            .json(&json!({"Username": "admin", "Pw": ""}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = login["AccessToken"].as_str().unwrap();
        Self {
            client: client.clone(),
            base,
            auth: format!("{CLIENT}, Token=\"{token}\""),
        }
    }

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
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header("Authorization", &self.auth)
            .json(body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}: {}",
            response.status(),
            response.text().await.unwrap()
        );
    }

    /// `POST /System/Shutdown`, then waits for `run` to return.
    async fn shut_down(&self, server: std::thread::JoinHandle<anyhow::Result<()>>) {
        self.post("/System/Shutdown", &Value::Null).await;
        tokio::time::timeout(
            Duration::from_mins(1),
            tokio::task::spawn_blocking(move || server.join()),
        )
        .await
        .expect("run returns after shutdown")
        .expect("join task")
        .expect("server thread did not panic")
        .expect("run exits cleanly");
    }
}

/// The `-threads` value of the probe of `file`, once the scan has run it.
async fn probe_threads(argv: &Path, file: &str) -> String {
    let deadline = Instant::now() + SCAN_DEADLINE;
    loop {
        let log = std::fs::read_to_string(argv).unwrap_or_default();
        let probe = log
            .lines()
            .find(|line| line.contains("-show_streams") && line.contains(file));
        if let Some(line) = probe {
            let tokens: Vec<_> = line.split_whitespace().collect();
            let at = tokens
                .iter()
                .position(|token| *token == "-threads")
                .unwrap_or_else(|| panic!("no -threads: {line}"));
            return tokens[at + 1].to_owned();
        }
        assert!(Instant::now() < deadline, "{file} was never probed:\n{log}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_saved_encoding_thread_count_reaches_the_probes_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let music = tmp.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::write(music.join("first.flac"), b"audio fixture").unwrap();
    let argv = tmp.path().join("argv.log");
    let ffmpeg = probe_stub(tmp.path(), "ffmpeg", &argv);
    let ffprobe = probe_stub(tmp.path(), "ffprobe", &argv);
    let client = reqwest::Client::new();
    let config = |port| Config {
        port,
        ffmpeg_path: Some(ffmpeg.clone()),
        ffprobe_path: Some(ffprobe.clone()),
        ..Config::test_stub(tmp.path())
    };

    // The default `-1` is "automatic": `-threads 0`, never the raw value.
    let (server, base, _) = boot::spawn_server(&client, config).await;
    let api = Api::login(&client, base).await;
    let library = json!({"LibraryOptions": {
        "PathInfos": [{"Path": music}], "EnableRealtimeMonitor": false,
        "EnableInternetProviders": false, "SaveLocalMetadata": false, "MetadataSavers": [],
        "EnableLUFSScan": false,
        "TypeOptions": [
            {"Type": "MusicArtist", "MetadataFetchers": [], "ImageFetchers": []},
            {"Type": "MusicAlbum", "MetadataFetchers": [], "ImageFetchers": []},
            {"Type": "Audio", "MetadataFetchers": [], "ImageFetchers": []}
        ]
    }});
    api.post(
        "/Library/VirtualFolders?name=Music&collectionType=music&refreshLibrary=true",
        &library,
    )
    .await;
    assert_eq!(probe_threads(&argv, "first.flac").await, "0");

    // Saved through the dashboard's encoding page; upstream reads it at startup.
    let mut encoding = api.get("/System/Configuration/encoding").await;
    assert_eq!(encoding["EncodingThreadCount"], -1);
    encoding["EncodingThreadCount"] = json!(1);
    api.post("/System/Configuration/encoding", &encoding).await;
    api.shut_down(server).await;

    std::fs::write(music.join("second.flac"), b"audio fixture").unwrap();
    let (server, base, _) = boot::spawn_server(&client, config).await;
    let api = Api::login(&client, base).await;
    api.post("/Library/Refresh", &Value::Null).await;
    assert_eq!(probe_threads(&argv, "second.flac").await, "1");
    api.shut_down(server).await;
}
