//! Saved library/server metadata locales reach providers through real HTTP.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

const POSTER: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0,
    0, 0, 253, 212, 154, 115, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 68, 12,
    16, 10, 0, 31, 238, 3, 253, 139, 95, 20, 212, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

const CLIENT: &str =
    r#"MediaBrowser Client="locale-test", Device="fixture", DeviceId="locale-test", Version="1""#;

async fn provider(
    State(requests): State<Arc<Mutex<Vec<String>>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    requests.lock().unwrap().push(uri.to_string());
    let origin = format!("http://{}", headers["host"].to_str().unwrap());
    let artwork = match uri.path() {
        "/artwork/types" => Some(json!({"data":[{"id":7,"name":"Poster","recordType":"season"}]})),
        "/series/5/extended" => Some(
            json!({"data":{"id":5,"seasons":[{"id":51,"number":1,"type":{"type":"official"}}]}}),
        ),
        "/series/6/extended" => Some(
            json!({"data":{"id":6,"seasons":[{"id":61,"number":1,"type":{"type":"official"}}]}}),
        ),
        "/seasons/51/extended" | "/seasons/61/extended" => Some(
            json!({"data":{"artwork":[{"type":7,"image":format!("{origin}/tvdb.png"),"language":"eng","width":2,"height":2}]}}),
        ),
        "/tvdb.png" => return ([("content-type", "image/png")], POSTER).into_response(),
        _ => None,
    };
    if let Some(artwork) = artwork {
        return Json(artwork).into_response();
    }
    if uri.path() == "/movie/603" {
        let url = reqwest::Url::parse(&format!("http://fixture{uri}")).unwrap();
        let language = url
            .query_pairs()
            .find(|(key, _)| key == "language")
            .map_or_else(|| "missing".to_owned(), |(_, value)| value.into_owned());
        return Json(json!({
            "id":603,"imdb_id":"tt0133093","title":format!("Locale {language}"),"overview":format!("Overview {language}"),
            "release_date":"1999-03-30", "vote_average":8,
            "release_dates":{"results":[
                {"iso_3166_1":"US","release_dates":[{"certification":"R"}]},
                {"iso_3166_1":"FR","release_dates":[{"certification":"12"}]},
                {"iso_3166_1":"DE","release_dates":[{"certification":"12"}]},
                {"iso_3166_1":"AR","release_dates":[{"certification":"13"}]}
            ]},"videos":{"results":[]},"credits":{"cast":[],"crew":[]}
        })).into_response();
    }
    if uri.path() == "/" {
        return Json(
            json!({"Title":"OMDb pick","imdbID":"tt0133093","Year":"1999","Type":"movie","Response":"True"}),
        ).into_response();
    }
    Json(json!({"results":[],"posters":[],"backdrops":[],"logos":[],"data":{"token":"fixture"}}))
        .into_response()
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
    let tv = tmp.path().join("tv");
    for (show, episode) in [
        ("Flat", "Flat.S01E01.mkv"),
        ("Physical", "Season 01/Physical.S01E01.mkv"),
    ] {
        let path = tv.join(show).join(episode);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fixture").unwrap();
        std::fs::write(
            tv.join(show).join("tvshow.nfo"),
            format!(
                "<tvshow><tvdbid>{}</tvdbid></tvshow>",
                if show == "Flat" { 5 } else { 6 }
            ),
        )
        .unwrap();
    }
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
    // L12: Identify uses the same saved enable lists and exact-name order as
    // automatic refresh. Shared IMDb ids make the first provider's result win.
    for (fetchers, order, expected) in [
        (json!([]), json!([]), None),
        (
            json!(["themoviedb", "the open movie database"]),
            json!(["The Open Movie Database", "TheMovieDb"]),
            Some("The Open Movie Database"),
        ),
        (
            json!(["TheMovieDb", "The Open Movie Database"]),
            json!(["TheMovieDb", "The Open Movie Database"]),
            Some("TheMovieDb"),
        ),
        (
            json!(["TheMovieDb", "The Open Movie Database"]),
            json!(["the open movie database", "TheMovieDb"]),
            Some("TheMovieDb"),
        ),
    ] {
        options["TypeOptions"][0]["MetadataFetchers"] = fetchers;
        options["TypeOptions"][0]["MetadataFetcherOrder"] = order;
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        let found: Value = api.client.post(format!("{}/Items/RemoteSearch/Movie", api.base))
            .header("Authorization", &api.auth)
            .json(&json!({"ItemId":id,"SearchInfo":{"Name":"Locale","ProviderIds":{"Tmdb":"603","Imdb":"tt0133093"}}}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        let found = found.as_array().unwrap();
        assert_eq!(found.len(), usize::from(expected.is_some()), "{found:?}");
        if let Some(expected) = expected {
            assert_eq!(found[0]["SearchProviderName"], expected, "{found:?}");
        }
    }
    // L13: both physical and path-less seasons use the selected TVDB image
    // provider with all metadata downloaders disabled. Manual selection still
    // lists images before enabling automatic acquisition.
    let mut tv_options = json!({"PathInfos":[{"Path":tv}],
        "EnableRealtimeMonitor":false,"EnableInternetProviders":true,
        "EnableChapterImageExtraction":false,"EnableTrickplayImageExtraction":false,
        "TypeOptions":[
            {"Type":"Series","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Season","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Episode","MetadataFetchers":[],"ImageFetchers":[]}
        ]
    });
    api.post(
        "/Library/VirtualFolders?name=TV&collectionType=tvshows&refreshLibrary=false",
        &json!({"LibraryOptions":tv_options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let seasons = loop {
        let result = api
            .get("/Items?recursive=true&includeItemTypes=Season&fields=Path")
            .await;
        let seasons = result["Items"].as_array().unwrap();
        if seasons.len() == 2 {
            break seasons.clone();
        }
        assert!(Instant::now() < deadline, "missing seasons: {result}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let folders = api.get("/Library/VirtualFolders").await;
    let library = &folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "TV")
        .unwrap()["ItemId"];
    for season in &seasons {
        let id = season["Id"].as_str().unwrap();
        let available = api
            .get(&format!(
                "/Items/{id}/RemoteImages?providerName=TheTVDB&includeAllLanguages=true"
            ))
            .await;
        assert_eq!(
            available["Images"].as_array().unwrap().len(),
            1,
            "{available}"
        );
        assert!(
            api.get(&format!("/Items/{id}/Images"))
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    tv_options["TypeOptions"][1]["ImageFetchers"] = json!(["TheTVDB"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":tv_options}),
    )
    .await;
    for season in &seasons {
        let id = season["Id"].as_str().unwrap();
        api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=None&imageRefreshMode=FullRefresh&replaceAllImages=true"), &Value::Null).await;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let images = api.get(&format!("/Items/{id}/Images")).await;
            if images
                .as_array()
                .unwrap()
                .iter()
                .any(|image| image["ImageType"] == "Primary")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no selected artwork for {season}: {images}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let image = api
            .client
            .get(format!("{}/Items/{id}/Images/Primary", api.base))
            .header("Authorization", &api.auth)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(image.as_ref(), POSTER);
    }
    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    mock.abort();
}
