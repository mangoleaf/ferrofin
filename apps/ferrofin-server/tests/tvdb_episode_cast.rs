//! TheTVDB's episode credits, end to end over real HTTP: a scan saves an
//! episode's own `Characters`, never the series' regular cast.
//!
//! The plugin's `MapEpisodeToResult` maps only the episode's record
//! (`TvdbEpisodeProvider.cs:225,272-308`, jellyfin-plugin-tvdb `5c4592f`), and
//! neither `EpisodeMetadataService` nor `MetadataService` adds the series'
//! people to an episode.
//!
//! Boots the real server ([`ferrofin_server::run`]) over a one-episode library
//! whose only metadata fetcher is TheTVDB, answered by an in-process stand-in
//! through [`Config::provider_endpoints`], then reads the saved credits back as
//! a client does. A first scan fetches the series before its episode, which is
//! the state the scan's cache must not turn into episode credits.

use std::time::{Duration, Instant};

use axum::{Json, Router, http::Uri};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="tvdb-cast-test", Device="fixture", DeviceId="tvdb-cast-test", Version="1""#;

/// How long the scan gets to save the credits being waited for.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

/// A TheTVDB stand-in for one series (5) with one episode (51).
async fn the_tvdb(uri: Uri) -> Json<Value> {
    Json(match uri.path() {
        "/login" => json!({"data":{"token":"fixture"}}),
        "/series/5/extended" => json!({"data":{"id":5,"name":"Cast Show","characters":[
            {"personName":"Series Regular","name":"Lead","peopleType":"Actor"},
            {"personName":"Second Regular","name":"Sidekick","peopleType":"Actor"}
        ]}}),
        "/series/5/episodes/official" => {
            json!({"data":{"episodes":[{"id":51,"seasonNumber":1,"number":1}]}})
        }
        "/episodes/51/extended" => json!({"data":{"id":51,"name":"Pilot","characters":[
            {"personName":"Episode Guest","name":"Villain","peopleType":"Guest Star"},
            {"personName":"Episode Director","peopleType":"Director"}
        ]}}),
        _ => json!({"data":{}}),
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

    /// The one item of `kind` once the scan has saved its credits: the name,
    /// role and type of each, as `GET /Items/{id}` returns them.
    async fn await_credits(&self, kind: &str) -> Vec<(String, Option<String>, String)> {
        let deadline = Instant::now() + SCAN_DEADLINE;
        loop {
            let items = self
                .get(&format!("/Items?recursive=true&includeItemTypes={kind}"))
                .await;
            if let Some(id) = items["Items"][0]["Id"].as_str() {
                let item = self.get(&format!("/Items/{id}?Fields=People")).await;
                if let Some(people) = item["People"].as_array().filter(|p| !p.is_empty()) {
                    return people
                        .iter()
                        .map(|person| {
                            (
                                person["Name"].as_str().unwrap().to_owned(),
                                person["Role"].as_str().map(str::to_owned),
                                person["Type"].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect();
                }
            }
            assert!(
                Instant::now() < deadline,
                "no {kind} credits after the scan: {items}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn credit(name: &str, role: Option<&str>, kind: &str) -> (String, Option<String>, String) {
    (name.to_owned(), role.map(str::to_owned), kind.to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scanned_tvdb_episode_lists_only_its_own_characters() {
    let tmp = tempfile::tempdir().unwrap();
    let tv = tmp.path().join("tv");
    let show = tv.join("Cast Show");
    let season = show.join("Season 01");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("Cast Show S01E01.mkv"), b"fixture").unwrap();
    std::fs::write(
        show.join("tvshow.nfo"),
        "<tvshow><tvdbid>5</tvdbid></tvshow>",
    )
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(the_tvdb))
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
            tvdb: Some(endpoint.clone()),
            ..ProviderEndpoints::offline()
        },
        ..Config::test_stub(tmp.path())
    })
    .await;
    let mut api = Api {
        client,
        base,
        auth: CLIENT.to_owned(),
    };
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

    let options = json!({"PathInfos":[{"Path":tv}],"EnableRealtimeMonitor":false,
        "EnableInternetProviders":true,"MetadataSavers":[],
        "EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,
        "TypeOptions":[{"Type":"Series","MetadataFetchers":["TheTVDB"],"ImageFetchers":[]},
            {"Type":"Season","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Episode","MetadataFetchers":["TheTVDB"],"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;

    // The series answered with its regulars, so its record was in the scan's
    // cache when its episode was asked for.
    assert_eq!(
        api.await_credits("Series").await,
        [
            credit("Series Regular", Some("Lead"), "Actor"),
            credit("Second Regular", Some("Sidekick"), "Actor"),
        ]
    );
    assert_eq!(
        api.await_credits("Episode").await,
        [
            credit("Episode Guest", Some("Villain"), "GuestStar"),
            // `UpdatePeople` stores a missing role as "".
            credit("Episode Director", Some(""), "Director"),
        ]
    );

    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
