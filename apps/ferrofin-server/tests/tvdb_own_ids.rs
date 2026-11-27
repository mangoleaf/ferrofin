//! TheTVDB looks a season and an episode up by their own TheTVDB id, end to
//! end over real HTTP (jellyfin-plugin-tvdb `v24`, `228f7b3`): a season
//! folder and an episode file naming their `tvdbid` are fetched by it, not
//! by their numbers through the series, which here points at other records.
//!
//! Boots the real server ([`ferrofin_server::run`]) over a one-episode
//! library whose only metadata fetcher is TheTVDB, answered by an
//! in-process stand-in through [`Config::provider_endpoints`], then reads
//! the saved season and episode back as a client does.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, http::Uri};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="tvdb-own-ids-test", Device="fixture", DeviceId="tvdb-own-ids-test", Version="1""#;

/// How long the scan gets to save what is waited for.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

/// A TheTVDB stand-in: series 5's numbers point at season 51 and episode 91,
/// the item names at season 77 and episode 88. Every path asked is logged.
async fn the_tvdb(State(asked): State<Arc<Mutex<Vec<String>>>>, uri: Uri) -> Json<Value> {
    asked.lock().unwrap().push(uri.path().to_owned());
    Json(match uri.path() {
        "/login" => json!({"data":{"token":"fixture"}}),
        "/series/5/extended" => json!({"data":{"id":5,"name":"Own Show",
            "seasons":[{"id":51,"number":1,"type":{"type":"official"}}]}}),
        "/seasons/51/extended" => json!({"data":{"id":51,"translations":{
            "overviewTranslations":[{"language":"eng","overview":"By the series."}]}}}),
        "/seasons/77/extended" => json!({"data":{"id":77,"translations":{
            "overviewTranslations":[{"language":"eng","overview":"The season's own."}]}}}),
        "/series/5/episodes/official" => {
            json!({"data":{"episodes":[{"id":91,"seasonNumber":1,"number":1}]}})
        }
        "/episodes/91/extended" => episode(91, "By number"),
        "/episodes/88/extended" => episode(88, "Its own"),
        _ => json!({"data":{}}),
    })
}

/// Episode `id`'s record, named and described `name` in English (the plugin
/// takes no untranslated name or overview by default).
fn episode(id: i64, name: &str) -> Value {
    json!({"data":{"id":id,"name":name,"translations":{
        "nameTranslations":[{"language":"eng","name":name}],
        "overviewTranslations":[{"language":"eng","overview":name}]}}})
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

    /// The one item of `kind` once the scan has saved its TheTVDB id and
    /// `field` on it, as `GET /Items/{id}` returns it.
    async fn await_field(&self, kind: &str, field: &str) -> Value {
        let deadline = Instant::now() + SCAN_DEADLINE;
        loop {
            let items = self
                .get(&format!("/Items?recursive=true&includeItemTypes={kind}"))
                .await;
            if let Some(id) = items["Items"][0]["Id"].as_str() {
                let item = self
                    .get(&format!("/Items/{id}?Fields=Overview,ProviderIds"))
                    .await;
                if item["ProviderIds"]["Tvdb"].is_string() && item[field].is_string() {
                    return item;
                }
            }
            assert!(
                Instant::now() < deadline,
                "no {kind} {field} after the scan: {items}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_season_and_an_episode_naming_their_tvdb_ids_are_fetched_by_them() {
    let tmp = tempfile::tempdir().unwrap();
    let tv = tmp.path().join("tv");
    let show = tv.join("Own Show");
    let season = show.join("Season 01 [tvdbid-77]");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("Own Show S01E01 [tvdbid-88].mkv"), b"fixture").unwrap();
    std::fs::write(
        show.join("tvshow.nfo"),
        "<tvshow><tvdbid>5</tvdbid></tvshow>",
    )
    .unwrap();

    let asked = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(the_tvdb)
        .with_state(Arc::clone(&asked));
    let mock = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
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
            {"Type":"Season","MetadataFetchers":["TheTVDB"],"ImageFetchers":[]},
            {"Type":"Episode","MetadataFetchers":["TheTVDB"],"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;

    let season = api.await_field("Season", "Overview").await;
    assert_eq!(season["Overview"], "The season's own.", "{season}");
    assert_eq!(season["ProviderIds"]["Tvdb"], "77", "{season}");
    let episode = api.await_field("Episode", "Overview").await;
    assert_eq!(episode["Name"], "Its own", "{episode}");
    assert_eq!(episode["Overview"], "Its own", "{episode}");
    assert_eq!(episode["ProviderIds"]["Tvdb"], "88", "{episode}");
    let asked = asked.lock().unwrap().clone();
    for by_number in ["/seasons/51/extended", "/series/5/episodes/official"] {
        assert!(!asked.iter().any(|path| path == by_number), "{asked:?}");
    }

    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
