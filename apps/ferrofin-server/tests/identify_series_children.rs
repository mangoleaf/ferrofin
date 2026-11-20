//! "Identify → Apply" on a series, end to end over real HTTP: the refresh
//! carries the chosen result to the series' seasons and episodes, and
//! `ApplySearchResult` hands each of their lookups the result's ids as its
//! series' (`SeriesProviderIds = GetValidProviderIds(result.ProviderIds)`,
//! `MetadataService.cs:272-293`). TheMovieDb's season and episode providers
//! read the series' `Tmdb` id there alone (`TmdbSeasonProvider.cs:49`,
//! `TmdbEpisodeProvider.cs:92`), so a result from TheTVDB leaves them without
//! one for that refresh, although the series itself still matches
//! TheMovieDb through the TheTVDB id.
//!
//! Boots the real server ([`ferrofin_server::run`]) over a one-episode library
//! whose only metadata fetcher is TheMovieDb, answered by an in-process
//! stand-in through [`Config::provider_endpoints`] that logs every request.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use axum::{Json, Router, http::Uri};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="identify-series-test", Device="fixture", DeviceId="identify-series-test", Version="1""#;

/// How long the scan gets to save the episode being waited for.
const SCAN_DEADLINE: Duration = Duration::from_mins(1);

/// A TheMovieDb stand-in for one series (1399, TheTVDB 121361) with one
/// episode.
fn the_moviedb(path: &str) -> axum::response::Response {
    let body = match path {
        "/search/tv" => json!({"results":[{"id":1399,"name":"GoT"}]}),
        "/find/121361" => json!({"movie_results":[],"tv_results":[{"id":1399}]}),
        "/tv/1399" => json!({"id":1399,"name":"GoT","overview":"A series.",
            "external_ids":{"tvdb_id":121_361}}),
        "/tv/1399/season/1" => json!({"id":3624,"name":"Season 1","episodes":[
            {"id":63056,"episode_number":1,"name":"Winter Is Coming",
             "overview":"Ned is summoned south.","air_date":"2011-04-17"}]}),
        "/tv/1399/season/1/episode/1" => json!({"credits":{"cast":[]}}),
        _ => return (axum::http::StatusCode::NOT_FOUND, "{}").into_response(),
    };
    Json(body).into_response()
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

    /// The one item of `kind`, once the scan has saved it with `overview`.
    async fn await_item(&self, kind: &str, overview: &str) -> Value {
        let deadline = Instant::now() + SCAN_DEADLINE;
        loop {
            let items = self
                .get(&format!(
                    "/Items?recursive=true&includeItemTypes={kind}&fields=Overview,ProviderIds"
                ))
                .await;
            let item = &items["Items"][0];
            if item["Overview"].as_str() == Some(overview) {
                return item.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no {kind} with its overview after the scan: {items}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_series_identified_through_thetvdb_leaves_its_episodes_no_tmdb_series_id() {
    let tmp = tempfile::tempdir().unwrap();
    let tv = tmp.path().join("tv");
    let season = tv.join("GoT").join("Season 01");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("GoT S01E01.mkv"), b"fixture").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = Arc::clone(&requests);
    let stand_in = Router::new().fallback(move |uri: Uri| {
        log.lock().unwrap().push(uri.path().to_owned());
        let response = the_moviedb(uri.path());
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
            tmdb: Some(endpoint.clone()),
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

    let tmdb = json!(["TheMovieDb"]);
    let options = json!({"PathInfos":[{"Path":tv}],"EnableRealtimeMonitor":false,
        "MetadataSavers":[],"EnableChapterImageExtraction":false,
        "EnableTrickplayImageExtraction":false,
        "TypeOptions":[{"Type":"Series","MetadataFetchers":tmdb,"ImageFetchers":[]},
            {"Type":"Season","MetadataFetchers":tmdb,"ImageFetchers":[]},
            {"Type":"Episode","MetadataFetchers":tmdb,"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Shows&collectionType=tvshows&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;
    api.await_item("Episode", "Ned is summoned south.").await;
    let series = api.await_item("Series", "A series.").await;
    let series_id = series["Id"].as_str().unwrap();

    // The admin picks TheTVDB's result for the series.
    requests.lock().unwrap().clear();
    api.post(
        &format!("/Items/RemoteSearch/Apply/{series_id}?replaceAllImages=false"),
        &json!({"Name":"GoT","ProviderIds":{"Tvdb":"121361"},"SearchProviderName":"TheTVDB"}),
    )
    .await;
    let asked = requests.lock().unwrap().clone();
    assert!(
        asked.iter().any(|path| path == "/find/121361"),
        "the series matches TheMovieDb through the chosen TheTVDB id: {asked:?}"
    );
    assert!(
        !asked
            .iter()
            .any(|path| path.starts_with("/tv/1399/season/")),
        "its season and episode have no TheMovieDb series id: {asked:?}"
    );
    let series = api
        .get(&format!("/Items/{series_id}?fields=ProviderIds"))
        .await;
    assert_eq!(series["ProviderIds"]["Tvdb"], "121361", "{series}");
    assert_eq!(series["ProviderIds"]["Tmdb"], "1399", "{series}");

    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap().unwrap())
        .await
        .unwrap();
    mock.abort();
}
