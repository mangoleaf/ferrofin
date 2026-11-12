//! TMDb's "Choose Image" candidates, end to end over real HTTP: each image
//! family takes its own size from the TMDb settings page, and a scaled size
//! drops TMDb's dimensions, which describe the original file
//! (`TmdbClientManager.ConvertToRemoteImageInfo`, `TmdbUtils.IsOriginalImageSize`);
//! each candidate also carries a preview at its family's fixed size
//! (`ThumbnailUrl`, `TmdbClientManager.ThumbnailSizes`).
//!
//! Boots the real server ([`ferrofin_server::run`]) over a one-episode library
//! whose series carries a Tmdb id in its folder name, with TMDb answered by an
//! in-process stand-in through [`Config::provider_endpoints`], sets the TMDb
//! plugin's sizes as the settings page does, then reads
//! `GET /Items/{id}/RemoteImages` for the series, the season and the episode.

use std::time::{Duration, Instant};

use axum::{Json, Router, http::Uri};
use ferrofin_server::config::{Config, OFFLINE_PROVIDER_URL, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="tmdb-images-test", Device="fixture", DeviceId="tmdb-images-test", Version="1""#;

/// The TMDb plugin's id (`Plugins/Tmdb/Plugin.cs`).
const TMDB_PLUGIN: &str = "b8715ed16c4745289ad3f72deb539cd4";

/// How long the scan gets to save the episode being waited for.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

/// A TMDb stand-in for series 1399's images, its season 1's and its
/// episode 1x01's, each with the original file's dimensions.
async fn tmdb(uri: Uri) -> Json<Value> {
    Json(match uri.path() {
        "/tv/1399/images" => json!({
            "posters":[{"file_path":"/poster.jpg","width":2000,"height":3000}],
            "backdrops":[{"file_path":"/backdrop.jpg","width":3840,"height":2160},
                {"file_path":"/text.jpg","iso_639_1":"en","width":3840,"height":2160}],
            "logos":[{"file_path":"/logo.png","width":800,"height":300}]}),
        "/tv/1399/season/1/images" => {
            json!({"posters":[{"file_path":"/season.jpg","width":1000,"height":1500}]})
        }
        "/tv/1399/season/1/episode/1/images" => {
            json!({"stills":[{"file_path":"/still.jpg","width":1920,"height":1080}]})
        }
        _ => json!({}),
    })
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
        let status = response.status();
        assert!(
            status.is_success(),
            "{path}: {status}: {}",
            response.text().await.unwrap()
        );
    }

    /// The id of the one item of `kind`, once the scan has saved it.
    async fn await_id(&self, kind: &str) -> String {
        let deadline = Instant::now() + SCAN_DEADLINE;
        loop {
            let items = self
                .get(&format!("/Items?recursive=true&includeItemTypes={kind}"))
                .await;
            if let Some(id) = items["Items"][0]["Id"].as_str() {
                return id.to_owned();
            }
            assert!(Instant::now() < deadline, "no {kind} after the scan");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// TMDb's candidates for `id`.
    async fn tmdb_images(&self, id: &str) -> Vec<Candidate> {
        let images = self.get(&format!("/Items/{id}/RemoteImages")).await;
        images["Images"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ProviderName"] == "TheMovieDb")
            .map(|image| {
                let relative = |url: &Value| {
                    url.as_str()
                        .map(|url| url.strip_prefix(OFFLINE_PROVIDER_URL).unwrap().to_owned())
                };
                (
                    image["Type"].as_str().unwrap().to_owned(),
                    relative(&image["Url"]).unwrap(),
                    relative(&image["ThumbnailUrl"]),
                    image["Width"].as_i64(),
                    image["Height"].as_i64(),
                )
            })
            .collect()
    }
}

/// One candidate: its type, its URL and preview URL (relative to the image
/// root), and its dimensions.
type Candidate = (String, String, Option<String>, Option<i64>, Option<i64>);

fn image(kind: &str, url: &str, thumbnail: &str, size: Option<(i64, i64)>) -> Candidate {
    (
        kind.to_owned(),
        url.to_owned(),
        Some(thumbnail.to_owned()),
        size.map(|(width, _)| width),
        size.map(|(_, height)| height),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tmdb_candidates_follow_each_familys_size_setting() {
    let tmp = tempfile::tempdir().unwrap();
    let tv = tmp.path().join("tv");
    let season = tv.join("Size Show [tmdbid-1399]").join("Season 01");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("Size Show S01E01.mkv"), b"fixture").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(tmdb))
            .await
            .unwrap();
    });
    let ffmpeg = tool_stub(tmp.path(), "ffmpeg");
    let ffprobe = tool_stub(tmp.path(), "ffprobe");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let (server, base, _config) = boot::spawn_server(&client, |port| Config {
        port,
        ffmpeg_path: Some(ffmpeg.clone()),
        ffprobe_path: Some(ffprobe.clone()),
        provider_endpoints: ProviderEndpoints {
            tmdb: Some(endpoint.clone()),
            ..ProviderEndpoints::offline()
        },
        ..Config::test_stub(tmp.path())
    })
    .await;
    let api = Api::login(client, base).await;

    // No fetcher: the scan asks TMDb nothing, and the chooser asks every
    // provider whatever the library enables (`IncludeDisabledProviders`).
    let options = json!({"PathInfos":[{"Path":tv}],"EnableRealtimeMonitor":false,
        "MetadataSavers":[],"EnableChapterImageExtraction":false,
        "EnableTrickplayImageExtraction":false,
        "TypeOptions":[{"Type":"Series","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Season","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Episode","MetadataFetchers":[],"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;
    let episode = api.await_id("Episode").await;
    let season = api.await_id("Season").await;
    let series = api.await_id("Series").await;

    // The settings page saves the whole document back with the sizes chosen;
    // the poster and logo sizes stay unset, which means `original`.
    let mut settings = api
        .get(&format!("/Plugins/{TMDB_PLUGIN}/Configuration"))
        .await;
    settings["BackdropSize"] = json!("w1280");
    settings["StillSize"] = json!("w300");
    api.post(&format!("/Plugins/{TMDB_PLUGIN}/Configuration"), &settings)
        .await;

    // One assertion, so a failure shows all three answers.
    assert_eq!(
        [
            api.tmdb_images(&series).await,
            api.tmdb_images(&season).await,
            api.tmdb_images(&episode).await,
        ],
        [
            // A backdrop with text comes back as a Thumb, ranked first for
            // being in the preferred language, and keeps the backdrop sizes.
            vec![
                image("Thumb", "/w1280/text.jpg", "/w780/text.jpg", None),
                image(
                    "Primary",
                    "/original/poster.jpg",
                    "/w500/poster.jpg",
                    Some((2000, 3000)),
                ),
                image(
                    "Backdrop",
                    "/w1280/backdrop.jpg",
                    "/w780/backdrop.jpg",
                    None
                ),
                image(
                    "Logo",
                    "/original/logo.png",
                    "/w500/logo.png",
                    Some((800, 300)),
                ),
            ],
            // A season's posters take the poster size, not the still size.
            vec![image(
                "Primary",
                "/original/season.jpg",
                "/w500/season.jpg",
                Some((1000, 1500)),
            )],
            // An episode's stills take the still size
            // (`ConvertStillsToRemoteImageInfo`).
            vec![image("Primary", "/w300/still.jpg", "/w500/still.jpg", None)],
        ]
    );

    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
