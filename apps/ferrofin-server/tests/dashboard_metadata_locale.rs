//! Saved library/server metadata locales reach providers through real HTTP.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, http::Uri};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

const CLIENT: &str =
    r#"MediaBrowser Client="locale-test", Device="fixture", DeviceId="locale-test", Version="1""#;

async fn provider(State(requests): State<Arc<Mutex<Vec<String>>>>, uri: Uri) -> Json<Value> {
    requests.lock().unwrap().push(uri.to_string());
    if uri.path() == "/movie/603" {
        let url = reqwest::Url::parse(&format!("http://fixture{uri}")).unwrap();
        let language = url
            .query_pairs()
            .find(|(key, _)| key == "language")
            .map_or_else(|| "missing".to_owned(), |(_, value)| value.into_owned());
        return Json(json!({
            "id":603,"title":format!("Locale {language}"),"overview":format!("Overview {language}"),
            "release_date":"1999-03-30", "vote_average":8,
            "release_dates":{"results":[
                {"iso_3166_1":"US","release_dates":[{"certification":"R"}]},
                {"iso_3166_1":"FR","release_dates":[{"certification":"12"}]},
                {"iso_3166_1":"DE","release_dates":[{"certification":"12"}]},
                {"iso_3166_1":"AR","release_dates":[{"certification":"13"}]}
            ]},"videos":{"results":[]},"credits":{"cast":[],"crew":[]}
        }));
    }
    Json(json!({"results":[],"posters":[],"backdrops":[],"logos":[],"data":{"token":"fixture"}}))
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

    async fn await_movie(&self, name: &str, rating: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let items = self
                .get("/Items?recursive=true&includeItemTypes=Movie")
                .await;
            if let Some(item) = items["Items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["Name"] == name && item["OfficialRating"] == rating)
            {
                return item.clone();
            }
            assert!(
                Instant::now() < deadline,
                "expected {name} / {rating}, got {items}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn probe_stub(root: &std::path::Path, tool: &str) -> std::path::PathBuf {
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

// One real server validates the live changes in sequence.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_locales_change_provider_requests_without_restarting() {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("movies/Locale");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("Locale.mkv"), b"fixture").unwrap();
    std::fs::write(
        media.join("movie.nfo"),
        "<movie><tmdbid>603</tmdbid></movie>",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(provider)
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
            tmdb: Some(endpoint.clone()),
            tmdb_images: Some(endpoint.clone()),
            tvdb: Some(endpoint.clone()),
            omdb: Some(endpoint.clone()),
            fanart: Some(endpoint.clone()),
            audiodb: Some(endpoint.clone()),
            opensubtitles: Some(endpoint.clone()),
        },
        studios_repo_url: endpoint.clone(),
        musicbrainz_base_url: endpoint,
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
        if api
            .client
            .get(format!("{}/System/Info/Public", api.base))
            .send()
            .await
            .is_ok()
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
    let mut config = api.get("/System/Configuration").await;
    config["PreferredMetadataLanguage"] = json!("fr");
    config["MetadataCountryCode"] = json!("FR");
    api.post("/System/Configuration", &config).await;
    let mut options = json!({"PathInfos":[{"Path":media.parent().unwrap()}],
        "EnableRealtimeMonitor":false,"EnableInternetProviders":true,
        "EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,
        "TypeOptions":[{"Type":"Movie","MetadataFetchers":["TheMovieDb"],"ImageFetchers":[]}]
    });
    api.post(
        "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    let folders = api.get("/Library/VirtualFolders").await;
    let library = &folders.as_array().unwrap()[0]["ItemId"];
    api.post("/Library/Refresh", &Value::Null).await;
    let movie = api.await_movie("Locale fr", "FR-12").await;
    let id = movie["Id"].as_str().unwrap();
    let refresh = format!(
        "/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"
    );

    options["PreferredMetadataLanguage"] = json!("de");
    options["MetadataCountryCode"] = json!("DE");
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    api.post(&refresh, &Value::Null).await;
    api.await_movie("Locale de", "FSK-12").await;

    options["PreferredMetadataLanguage"] = json!("");
    options["MetadataCountryCode"] = json!("");
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    config["PreferredMetadataLanguage"] = json!("es-419");
    config["MetadataCountryCode"] = json!("AR");
    api.post("/System/Configuration", &config).await;
    api.post(&refresh, &Value::Null).await;
    api.await_movie("Locale es-AR", "AR-13").await;
    let requests = requests.lock().unwrap().clone();
    for language in ["fr", "de", "es-AR"] {
        assert!(
            requests.iter().any(|uri| uri.starts_with("/movie/603?")
                && uri.contains(&format!("language={language}"))),
            "{requests:?}"
        );
    }
    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    mock.abort();
}
