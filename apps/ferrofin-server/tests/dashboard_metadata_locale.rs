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

#[allow(
    clippy::too_many_lines,
    reason = "one offline HTTP provider fixture serves the locale and artwork request matrix"
)]
async fn provider(
    State(requests): State<Arc<Mutex<Vec<String>>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    requests.lock().unwrap().push(uri.to_string());
    let origin = format!("http://{}", headers["host"].to_str().unwrap());
    match uri.path() {
        "/login" => {
            return Json(json!({"token":"fixture","data":{"token":"fixture"}})).into_response();
        }
        "/download" => {
            return Json(json!({"link":format!("{origin}/subtitle-file")})).into_response();
        }
        "/subtitle-file" => {
            return (
                [("content-type", "text/plain")],
                "1\n00:00:00,000 --> 00:00:01,000\nDownloaded\n",
            )
                .into_response();
        }
        "/subtitles" => {
            return Json(
                json!({"data":[{"attributes":{"moviehash_match":false,"files":[{"file_id":42}]}}]}),
            )
            .into_response();
        }
        _ => {}
    }
    let image_path = uri.path().strip_prefix("/original").unwrap_or(uri.path());
    if matches!(
        image_path,
        "/large.png"
            | "/small.png"
            | "/backdrop-one.png"
            | "/backdrop-two.png"
            | "/backdrop-three.png"
            | "/backdrop-small.png"
    ) {
        let mut bytes = POSTER.to_vec();
        // Valid PNGs with distinct lengths exercise the backdrop duplicate guard.
        bytes.extend_from_slice(image_path.as_bytes());
        if image_path == "/backdrop-two.png" {
            bytes.push(0);
        }
        return ([("content-type", "image/png")], bytes).into_response();
    }
    if uri.path() == "/movie/603/similar" {
        return Json(json!({"results":[{"id":604}],"total_pages":1})).into_response();
    }
    if uri.path() == "/movie/603/images" {
        return Json(json!({
            "posters":[{"file_path":"/small.png","width":500},{"file_path":"/large.png","width":1600}],
            "backdrops":[{"file_path":"/backdrop-small.png","width":1279},
                         {"file_path":"/backdrop-one.png","width":1280},
                         {"file_path":"/backdrop-two.png","width":1920},
                         {"file_path":"/backdrop-three.png","width":3840}]
        })).into_response();
    }
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
    if uri.path().starts_with("/person/") || uri.path().starts_with("/collection/") {
        let url = reqwest::Url::parse(&format!("http://fixture{uri}")).unwrap();
        let language = url
            .query_pairs()
            .find(|(key, _)| key == "language")
            .map_or_else(|| "missing".to_owned(), |(_, value)| value.into_owned());
        let images: Vec<_> = ["fr", "de", "es", "en", "xx"]
            .into_iter()
            .map(|tag| {
                json!({
                    "file_path":format!("/locale-{tag}.png"), "iso_639_1":tag,
                    "width":1600,"height":2400,"vote_average":7,"vote_count":9
                })
            })
            .collect();
        return Json(json!({
            "id": uri.path().rsplit('/').next().unwrap().parse::<i64>().unwrap(),
            "name":format!("Collection {language}"),"overview":format!("Collection overview {language}"),
            "biography":format!("Person biography {language}"),
            "images":{"posters":images,"profiles":images,"backdrops":[]}
        })).into_response();
    }
    if image_path.starts_with("/locale-") && image_path.strip_suffix(".png").is_some() {
        return ([("content-type", "image/png")], POSTER).into_response();
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
    database_url: String,
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
                .get("/Items?recursive=true&includeItemTypes=Movie&fields=Overview")
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
for input in "$@"; do
case "$input" in
*.sub)
cat <<'EOF'
{{"streams":[{{"index":0,"codec_type":"subtitle","codec_name":"dvdsub"}}],"format":{{"format_name":"vobsub"}}}}
EOF
exit 0
;;
*.srt)
cat <<'EOF'
{{"streams":[{{"index":0,"codec_type":"subtitle","codec_name":"subrip"}}],"format":{{"format_name":"srt"}}}}
EOF
exit 0
;;
esac
done
if [ -f "{}/embedded-subtitles.json" ]; then
cat "{}/embedded-subtitles.json"
exit 0
fi
cat <<'EOF'
{{"streams":[{{"index":0,"codec_type":"video","codec_name":"h264","width":64,"height":64}}],"format":{{"format_name":"matroska,webm","duration":"60.0","size":"1024","bit_rate":"1000"}}}}
EOF
fi
"#,
        root.display(),
        root.display()
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
            lrclib: Some(endpoint.clone()),
        },
        studios_repo_url: endpoint.clone(),
        musicbrainz_base_url: endpoint,
        ..Config::test_stub(tmp.path())
    };
    let database_url = config.database_url();
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
        database_url,
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
    let expected: Value = serde_json::from_str(include_str!(
        "../../../crates/ferrofin-core/src/data/localization_options.json"
    ))
    .unwrap();
    assert_eq!(api.get("/Localization/Options").await, expected);
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
    let observed = requests.lock().unwrap().clone();
    for language in ["fr", "de", "es-AR"] {
        assert!(
            observed.iter().any(|uri| {
                if !uri.starts_with("/movie/603?") {
                    return false;
                }
                let url = reqwest::Url::parse(&format!("http://fixture{uri}")).unwrap();
                let query = url
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect::<std::collections::HashMap<_, _>>();
                query.get("language").map(String::as_str) == Some(language)
                    && query
                        .get("include_image_language")
                        .is_some_and(|value| value == &format!("{language},null,en"))
                    && query
                        .get("append_to_response")
                        .is_some_and(|value| value.split(',').any(|method| method == "images"))
            }),
            "{observed:?}"
        );
    }
    verify_server_metadata_language(&api, &mut config, &refresh).await;
    verify_by_name_metadata_languages(&api, &mut config, &requests).await;
    verify_server_metadata_country(&api, &mut config, &refresh).await;
    verify_by_name_metadata_countries(&api, &mut config).await;
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
    verify_movie_image_options(&api, library, &mut options, id).await;
    verify_nfo_savers(&api, library, &mut options, id, &media).await;
    verify_selected_nfo_user(&api, library, &mut options, id, &media).await;
    verify_nfo_image_path_setting(&api, library, &mut options, id, &media).await;
    verify_nfo_image_order_culture(&api, library, &mut options, id, &media).await;
    verify_artwork_destinations(&api, library, &mut options, id, &media).await;
    verify_indexed_artwork_deletion(&api, id).await;
    verify_indexed_artwork_upload(&api, id).await;
    verify_screenshot_replacement(&api, id).await;
    verify_similarity_selection(&api, library, &mut options, id, &media, &requests).await;
    verify_embedded_subtitle_options(&api, library, &mut options, id, &media, tmp.path()).await;
    verify_automatic_subtitle_constraints(
        &api,
        library,
        &mut options,
        id,
        &media,
        tmp.path(),
        &requests,
    )
    .await;
    verify_subtitle_destinations(&api, library, &mut options, id, &media, tmp.path()).await;
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
    verify_local_reader_order(&api, tmp.path()).await;
    verify_media_segment_library_options(&api, id).await;
    verify_special_episode_display(&api, tmp.path()).await;
    verify_date_added_policy(&api, tmp.path()).await;
    verify_root_library_location(&api).await;
    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    mock.abort();
}

async fn verify_root_library_location(api: &Api) {
    api.post(
        "/Library/VirtualFolders?name=Root-fixture&collectionType=movies&refreshLibrary=false",
        &json!({"LibraryOptions":{
            "PathInfos":[{"Path":"/"}],"EnableRealtimeMonitor":false,
            "SaveSubtitlesWithMedia":false,
            "TypeOptions":[{"Type":"Movie","MetadataFetchers":[],"ImageFetchers":[]}]
        }}),
    )
    .await;
    let folders = api.get("/Library/VirtualFolders").await;
    let root = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Root-fixture")
        .unwrap();
    assert_eq!(root["Locations"], json!(["/"]));
    assert_eq!(root["LibraryOptions"]["PathInfos"][0]["Path"], "/");
    assert!(!root["ItemId"].as_str().unwrap().is_empty());
    api.client
        .delete(format!(
            "{}/Library/VirtualFolders/Paths?name=Root-fixture&path=%2F&refreshLibrary=false",
            api.base
        ))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let folders = api.get("/Library/VirtualFolders").await;
    let root = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Root-fixture")
        .unwrap();
    assert_eq!(root["Locations"], json!([]));
    assert_eq!(root["LibraryOptions"]["PathInfos"], json!([]));
    // No refresh follows root configuration: only the disposable fixture was scanned.
}

/// Production registry, persisted producer output, live library saves and player filtering.
async fn verify_media_segment_library_options(api: &Api, item: &str) {
    let available = api
        .get("/Libraries/AvailableOptions?libraryContentType=movies")
        .await;
    assert!(
        available["MediaSegmentProviders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider["Name"] == "Intro Skipper"
                && provider["DefaultEnabled"] == true)
    );
    let folders = api.get("/Library/VirtualFolders").await;
    let folder = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Movies")
        .unwrap();
    let library = &folder["ItemId"];
    let mut options = folder["LibraryOptions"].clone();
    api.post(
        &format!("/MediaSegmentsApi/{item}"),
        &json!({"Type":"Intro", "StartTicks":0, "EndTicks":10_000_000}),
    )
    .await;
    api.post(
        &format!("/MediaSegmentsApi/{item}?providerId=wasm:unloaded"),
        &json!({"Type":"Outro", "StartTicks":20_000_000, "EndTicks":30_000_000}),
    )
    .await;
    for disabled in [true, false, true, false] {
        options["DisabledMediaSegmentProviders"] = if disabled {
            json!(["iNtRo SkIpPeR"])
        } else {
            json!([])
        };
        options["MediaSegmentProviderOrder"] = json!(["Intro Skipper"]);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        let started = Instant::now();
        let served = api.get(&format!("/MediaSegments/{item}")).await;
        println!(
            "L27 disabled={disabled}: segment GET {} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(
            served["Items"].as_array().unwrap().len(),
            usize::from(!disabled),
            "{served}"
        );
        assert!(
            api.get(&format!("/Episode/{item}/Timestamps")).await["Introduction"].is_object(),
            "management access preserves hidden detections"
        );
    }
    // The always-present core extraction task now invokes the registered provider.
    let tasks = api.get("/ScheduledTasks").await;
    let task = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["Key"] == "TaskExtractMediaSegments")
        .unwrap();
    let task_id = task["Id"].as_str().unwrap();
    let previous = task["LastExecutionResult"]["EndTimeUtc"].clone();
    api.post(&format!("/ScheduledTasks/Running/{task_id}"), &Value::Null)
        .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let task = api.get(&format!("/ScheduledTasks/{task_id}")).await;
        if task["State"] == "Idle" && task["LastExecutionResult"]["EndTimeUtc"] != previous {
            assert_eq!(task["LastExecutionResult"]["Status"], "Completed");
            break;
        }
        assert!(Instant::now() < deadline, "media segment task: {task}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let served = api.get(&format!("/MediaSegments/{item}")).await;
    assert_eq!(served["Items"].as_array().unwrap().len(), 1);
    assert_eq!(served["Items"][0]["Type"], "Intro");
}

/// Save limits, refresh through HTTP, and inspect both rows and served bytes.
async fn verify_movie_image_options(api: &Api, library: &Value, options: &mut Value, id: &str) {
    options["TypeOptions"][0]["MetadataFetchers"] = json!([]);
    options["TypeOptions"][0]["ImageFetchers"] = json!(["TheMovieDb"]);
    for (primary, backdrops, min_width, expected) in [
        (0, 2, 1280, "/backdrop-one.png"),
        (1, 1, 3000, "/backdrop-three.png"),
    ] {
        options["TypeOptions"][0]["ImageOptions"] = json!([
            {"Type":"Primary","Limit":primary,"MinWidth":1000},
            {"Type":"Backdrop","Limit":backdrops,"MinWidth":min_width}
        ]);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=None&imageRefreshMode=FullRefresh&replaceAllImages=true"), &Value::Null).await;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let images = api.get(&format!("/Items/{id}/Images")).await;
            let images = images.as_array().unwrap();
            if images
                .iter()
                .filter(|image| image["ImageType"] == "Primary")
                .count()
                == primary
                && images
                    .iter()
                    .filter(|image| image["ImageType"] == "Backdrop")
                    .count()
                    == backdrops
            {
                break;
            }
            assert!(Instant::now() < deadline, "wrong acquisition: {images:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut received = Vec::new();
        for index in 0..backdrops {
            let bytes = api
                .client
                .get(format!("{}/Items/{id}/Images/Backdrop/{index}", api.base))
                .header("Authorization", &api.auth)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .bytes()
                .await
                .unwrap();
            received.push(bytes.to_vec());
        }
        let mut wanted = POSTER.to_vec();
        wanted.extend_from_slice(expected.as_bytes());
        let mut wanted = vec![wanted];
        if backdrops == 2 {
            let mut second = POSTER.to_vec();
            second.extend_from_slice(b"/backdrop-two.png\0");
            wanted.push(second);
        }
        // The HTTP indices address persisted rows; compare the acquired set.
        received.sort();
        wanted.sort();
        assert_eq!(received, wanted);
        if primary > 0 {
            let bytes = api
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
            let mut wanted = POSTER.to_vec();
            wanted.extend_from_slice(b"/large.png");
            assert_eq!(
                bytes.as_ref(),
                wanted,
                "minimum width skipped the small poster"
            );
        }
    }
    let manual = api
        .get(&format!(
            "/Items/{id}/RemoteImages?providerName=TheMovieDb&includeAllLanguages=true"
        ))
        .await;
    assert_eq!(
        manual["Images"].as_array().unwrap().len(),
        6,
        "manual chooser is not capped"
    );
}

/// Competing local sources change the scanned title when their saved order changes.
async fn verify_local_reader_order(api: &Api, root: &std::path::Path) {
    let books = root.join("books");
    std::fs::create_dir_all(&books).unwrap();
    std::fs::write(
        books.join("Ordered.cbz"),
        b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
    )
    .unwrap();
    std::fs::write(
        books.join("Ordered.xml"),
        "<ComicInfo><Title>Comic</Title></ComicInfo>",
    )
    .unwrap();
    std::fs::write(books.join("Ordered.opf"), r#"<package xmlns:dc="http://purl.org/dc/elements/1.1/"><metadata><dc:title>Sidecar</dc:title></metadata></package>"#).unwrap();
    let available = api
        .get("/Libraries/AvailableOptions?libraryContentType=books&isNewLibrary=true")
        .await;
    let names: Vec<_> = available["MetadataReaders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reader| reader["Name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Comic Provider", "EPUB Metadata", "Open Packaging Format"]
    );
    let mut options = json!({"PathInfos":[{"Path":books}],"EnableRealtimeMonitor":false,
        "LocalMetadataReaderOrder":["Open Packaging Format","Comic Provider"],
        "TypeOptions":[{"Type":"Book","MetadataFetchers":[],"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Books&collectionType=books&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    api.post("/Library/Refresh", &Value::Null).await;
    let book = await_book(api, "Sidecar").await;
    let id = book["Id"].as_str().unwrap();
    let folders = api.get("/Library/VirtualFolders").await;
    let library = &folders
        .as_array()
        .unwrap()
        .iter()
        .find(|folder| folder["Name"] == "Books")
        .unwrap()["ItemId"];
    let mut server = api.get("/System/Configuration").await;
    server["MetadataOptions"] =
        serde_json::to_value([ferrofin_model::configuration::MetadataOptions {
            item_type: Some("Book".to_owned()),
            local_metadata_reader_order: vec!["Open Packaging Format".to_owned()],
            ..Default::default()
        }])
        .unwrap();
    api.post("/System/Configuration", &server).await;
    for (order, expected) in [
        (json!(["Comic Provider", "Open Packaging Format"]), "Comic"),
        (Value::Null, "Sidecar"),
        (json!([]), "Comic"),
    ] {
        options["LocalMetadataReaderOrder"] = order;
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"), &Value::Null).await;
        assert_eq!(await_book(api, expected).await["Id"], id);
    }
}

async fn await_book(api: &Api, name: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = api.get("/Items?recursive=true&includeItemTypes=Book").await;
        if let Some(book) = result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|book| book["Name"] == name)
        {
            return book.clone();
        }
        assert!(Instant::now() < deadline, "expected {name}: {result}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Selected savers run after refresh and edit; errors do not undo the edit.
#[allow(clippy::too_many_lines)]
async fn verify_nfo_savers(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
) {
    let nfo = media.join("movie.nfo");
    options["TypeOptions"][0]["MetadataFetchers"] = json!(["TheMovieDb"]);
    options["TypeOptions"][0]["ImageFetchers"] = json!([]);
    options["MetadataSavers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let original = std::fs::read_to_string(&nfo).unwrap();
    let item_url = format!("/Items/{id}");
    let mut body = api.get(&item_url).await;
    body["Name"] = json!("Saver disabled");
    api.post(&item_url, &body).await;
    assert_eq!(std::fs::read_to_string(&nfo).unwrap(), original);
    options["MetadataSavers"] = json!(["nfo"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    std::fs::remove_file(&nfo).unwrap();
    api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"),&Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let saved = loop {
        if let Ok(xml) = std::fs::read_to_string(&nfo)
            && xml.contains("<title>Locale es-AR</title>")
        {
            break xml;
        }
        assert!(
            Instant::now() < deadline,
            "automatic refresh did not save NFO"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(saved.contains("<tmdbid>603</tmdbid>"));
    assert!(saved.contains("<fileinfo>"));
    assert!(saved.contains("<art>"));
    std::fs::write(
        &nfo,
        saved.replace(
            "</movie>",
            "<custom><nested>keep me</nested></custom><genre>stale</genre></movie>",
        ),
    )
    .unwrap();
    let mut body = api.get(&item_url).await;
    body["Name"] = json!("Saved edit");
    body["Genres"] = json!([]);
    body["LockData"] = json!(true);
    body["ProviderIds"] = json!({"Tmdb":"603","Imdb":"tt0133093","Custom":"changed"});
    api.post(&item_url, &body).await;
    let saved = std::fs::read_to_string(&nfo).unwrap();
    assert!(saved.contains("<title>Saved edit</title>"));
    assert!(saved.contains("<nested>keep me</nested>"));
    assert!(saved.contains("<customid>changed</customid>"));
    assert!(saved.contains("<lockdata>true</lockdata>"));
    assert!(!saved.contains("stale"));
    // Selecting no saver suppresses writes even when the legacy flag is on.
    options["MetadataSavers"] = json!([]);
    options["SaveLocalMetadata"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    body["Name"] = json!("Disabled again");
    api.post(&item_url, &body).await;
    assert_eq!(std::fs::read_to_string(&nfo).unwrap(), saved);
    // Without an explicit list, an edit updates an existing sidecar only.
    options["MetadataSavers"] = Value::Null;
    options["SaveLocalMetadata"] = json!(false);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    body["Name"] = json!("Legacy existing");
    api.post(&item_url, &body).await;
    assert!(
        std::fs::read_to_string(&nfo)
            .unwrap()
            .contains("<title>Legacy existing</title>")
    );
    std::fs::remove_file(&nfo).unwrap();
    body["Name"] = json!("Legacy absent");
    api.post(&item_url, &body).await;
    assert!(!nfo.exists());
    options["MetadataSavers"] = json!(["Nfo"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    api.post(
        "/System/Configuration/xbmcmetadata",
        &json!({"SaveImagePathsInNfo":false}),
    )
    .await;
    api.post(&item_url, &body).await;
    assert!(!std::fs::read_to_string(&nfo).unwrap().contains("<art>"));
    std::fs::remove_file(&nfo).unwrap();
    std::fs::create_dir(&nfo).unwrap();
    body["Name"] = json!("Edit survives saver failure");
    api.post(&item_url, &body).await;
    assert_eq!(
        api.get(&item_url).await["Name"],
        "Edit survives saver failure"
    );
    std::fs::remove_dir(&nfo).unwrap();
    options["MetadataSavers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
}

async fn upload_artwork(api: &Api, id: &str, kind: &str) {
    let response = api
        .client
        .post(format!("{}/Items/{id}/Images/{kind}", api.base))
        .header("Authorization", &api.auth)
        .header("Content-Type", "image/png")
        .body("iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGP4z8AARAwQCgAf7gP9i18U1AAAAABJRU5ErkJggg==")
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert!(
        status.is_success(),
        "artwork upload: {status}: {}",
        response.text().await.unwrap()
    );
}

// Keep N04's next append slot stable across its independent configuration phases.
async fn prepare_backdrop_append(api: &Api, id: &str, index: usize) {
    let endpoint = format!("/Items/{id}/Images");
    let images = api.get(&endpoint).await;
    let count = images
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == "Backdrop")
        .count();
    for remove in (index..count).rev() {
        let response = api
            .client
            .delete(format!("{}{endpoint}/Backdrop/{remove}", api.base))
            .header("Authorization", &api.auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    }
    for _ in count..index {
        upload_artwork(api, id, "Backdrop").await;
    }
}

async fn verify_indexed_artwork_upload(api: &Api, id: &str) {
    let endpoint = format!("/Items/{id}/Images");
    for index in [0, 1, -1, i32::MAX] {
        let before = api.get(&endpoint).await;
        let previous: Vec<_> = before
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ImageType"] == "Backdrop")
            .map(|image| image["Path"].clone())
            .collect();
        upload_artwork(api, id, &format!("Backdrop/{index}")).await;
        let after = api.get(&endpoint).await;
        let actual: Vec<_> = after
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ImageType"] == "Backdrop")
            .map(|image| image["Path"].clone())
            .collect();
        assert_eq!(actual.len(), previous.len() + 1);
        assert_eq!(&actual[..previous.len()], previous);
        for path in actual {
            assert!(std::path::Path::new(path.as_str().unwrap()).is_file());
        }
    }
}

async fn verify_screenshot_replacement(api: &Api, id: &str) {
    let endpoint = format!("/Items/{id}/Images");
    let mut saved_path = None;
    for suffix in [
        "Screenshot",
        "Screenshot",
        "Screenshot/0",
        "Screenshot/-1",
        "Screenshot/2147483647",
    ] {
        upload_artwork(api, id, suffix).await;
        let images = api.get(&endpoint).await;
        let screenshots: Vec<_> = images
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ImageType"] == "Screenshot")
            .collect();
        assert_eq!(screenshots.len(), 1);
        let path = screenshots[0]["Path"].as_str().unwrap();
        assert_eq!(
            std::path::Path::new(path).file_name().unwrap(),
            "screenshot.png"
        );
        if let Some(previous) = &saved_path {
            assert_eq!(previous, path);
        }
        saved_path = Some(path.to_owned());
        assert!(std::path::Path::new(path).is_file());
        let dto = api.get(&format!("/Items/{id}")).await;
        assert!(dto["ImageTags"]["Screenshot"].as_str().is_some());
    }
    let before = api.get(&endpoint).await;
    let response = api
        .client
        .post(format!(
            "{}{endpoint}/Screenshot/0/Index?newIndex=1",
            api.base
        ))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(api.get(&endpoint).await, before);
}

async fn verify_indexed_artwork_deletion(api: &Api, id: &str) {
    let kind = "Backdrop";
    upload_artwork(api, id, kind).await;
    upload_artwork(api, id, kind).await;
    let endpoint = format!("/Items/{id}/Images");
    let before = api.get(&endpoint).await;
    let images: Vec<_> = before
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == kind)
        .collect();
    let deleted = images[1]["Path"].as_str().unwrap();
    let expected: Vec<_> = images
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, image)| image["Path"].clone())
        .collect();
    for index in [-1, i32::MAX, 1] {
        let response = api
            .client
            .delete(format!("{}{endpoint}/{kind}/{index}", api.base))
            .header("Authorization", &api.auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        if index != 1 {
            assert_eq!(api.get(&endpoint).await, before);
        }
    }
    assert!(!std::path::Path::new(deleted).exists());
    let after = api.get(&endpoint).await;
    let actual: Vec<_> = after
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == kind)
        .map(|image| image["Path"].clone())
        .collect();
    assert_eq!(actual, expected);
    for (index, path) in actual.iter().enumerate() {
        assert!(std::path::Path::new(path.as_str().unwrap()).is_file());
        let response = api
            .client
            .get(format!(
                "{}{endpoint}/{kind}/{index}?format=Original",
                api.base
            ))
            .header("Authorization", &api.auth)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        assert!(!response.bytes().await.unwrap().is_empty());
    }
    // Omitting the index removes the first slot, leaving the other types alone.
    let unrelated: Vec<_> = after
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] != kind)
        .cloned()
        .collect();
    api.client
        .delete(format!("{}{endpoint}/{kind}", api.base))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let final_images = api.get(&endpoint).await;
    assert_eq!(
        final_images
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ImageType"] != kind)
            .cloned()
            .collect::<Vec<_>>(),
        unrelated
    );
    assert_eq!(
        final_images
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ImageType"] == kind)
            .count(),
        actual.len() - 1
    );
}

// Keep the live option transitions and their disk assertions in one scenario.
#[allow(clippy::too_many_lines)]
async fn verify_artwork_destinations(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
) {
    options["SaveLocalMetadata"] = json!(false);
    options["TypeOptions"][0]["ImageFetchers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    upload_artwork(api, id, "Primary").await;
    assert!(!media.join("folder.png").exists());
    options["SaveLocalMetadata"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    upload_artwork(api, id, "Primary").await;
    assert_eq!(std::fs::read(media.join("folder.png")).unwrap(), POSTER);
    // ImageSaver retains existing local art even during replacement refresh.
    // Remove it to exercise a genuinely new automatic acquisition.
    std::fs::remove_file(media.join("folder.png")).unwrap();
    options["TypeOptions"][0]["ImageFetchers"] = json!(["TheMovieDb"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=None&imageRefreshMode=FullRefresh&replaceAllImages=true"),&Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::fs::read(media.join("folder.png")).is_ok_and(|bytes| bytes.ends_with(b"/large.png"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "automatic artwork not saved beside media"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    options["TypeOptions"][0]["ImageFetchers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let mut config = api.get("/System/Configuration").await;
    config["ImageSavingConvention"] = json!("Compatible");
    api.post("/System/Configuration", &config).await;
    upload_artwork(api, id, "Primary").await;
    assert_eq!(std::fs::read(media.join("poster.png")).unwrap(), POSTER);
    assert!(!media.join("folder.png").exists());
    api.post(
        "/System/Configuration/xbmcmetadata",
        &json!({"EnableExtraThumbsDuplication":true}),
    )
    .await;
    prepare_backdrop_append(api, id, 0).await;
    upload_artwork(api, id, "Backdrop/0").await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert_eq!(std::fs::read(media.join("fanart.png")).unwrap(), POSTER);
    assert_eq!(
        std::fs::read(media.join("extrafanart/fanart1.png")).unwrap(),
        POSTER
    );
    assert_eq!(
        std::fs::read(media.join("extrathumbs/thumb1.png")).unwrap(),
        POSTER
    );
    assert_eq!(
        api.get(&format!("/Items/{id}/Images"))
            .await
            .as_array()
            .unwrap()
            .iter()
            .filter(|image| image["ImageType"] == "Backdrop")
            .count(),
        2
    );
    let image_paths = api.get(&format!("/Items/{id}/Images")).await;
    let backdrops: Vec<_> = image_paths
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == "Backdrop")
        .collect();
    assert_eq!(
        backdrops[0]["Path"],
        media.join("fanart.png").to_string_lossy().as_ref()
    );
    assert_eq!(
        backdrops[1]["Path"],
        media
            .join("extrafanart/fanart1.png")
            .to_string_lossy()
            .as_ref()
    );
    upload_artwork(api, id, "Backdrop/0").await;
    let image_paths = api.get(&format!("/Items/{id}/Images")).await;
    let backdrops: Vec<_> = image_paths
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == "Backdrop")
        .collect();
    assert_eq!(
        backdrops[0]["Path"],
        media.join("fanart.png").to_string_lossy().as_ref()
    );
    assert_eq!(
        backdrops[1]["Path"],
        media
            .join("extrafanart/fanart1.png")
            .to_string_lossy()
            .as_ref()
    );
    verify_extra_thumbs_duplication(api, library, options, id, media).await;
    std::fs::remove_file(media.join("poster.png")).unwrap();
    std::fs::create_dir(media.join("poster.png")).unwrap();
    upload_artwork(api, id, "Primary").await;
    let served = api
        .client
        .get(format!(
            "{}/Items/{id}/Images/Primary?format=Original",
            api.base
        ))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(served.as_ref(), POSTER);
    std::fs::remove_dir(media.join("poster.png")).unwrap();
    config["ImageSavingConvention"] = json!("Legacy");
    api.post("/System/Configuration", &config).await;
    options["SaveLocalMetadata"] = json!(false);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
}

// Verify the live option independently of L17's enabled-destination coverage.
#[allow(clippy::too_many_lines)]
async fn verify_extra_thumbs_duplication(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
) {
    let original = api.get("/System/Configuration/xbmcmetadata").await;
    let server = api.get("/System/Configuration").await;
    let original_options = options.clone();
    let duplicate = media.join("extrathumbs/thumb1.png");
    std::fs::write(&duplicate, b"existing duplicate").unwrap();
    let mut configuration = original.clone();
    configuration["EnableExtraThumbsDuplication"] = json!(false);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    prepare_backdrop_append(api, id, 1).await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert_eq!(
        std::fs::read(&duplicate).unwrap(),
        b"existing duplicate",
        "disabling retains existing duplicate bytes"
    );
    configuration["EnableExtraThumbsDuplication"] = json!(true);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    prepare_backdrop_append(api, id, 1).await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert_eq!(std::fs::read(&duplicate).unwrap(), POSTER);
    configuration["EnableExtraThumbsDuplication"] = json!(false);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    std::fs::remove_file(&duplicate).unwrap();
    prepare_backdrop_append(api, id, 1).await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert!(!duplicate.exists(), "disabling applies on the next upload");
    configuration["EnableExtraThumbsDuplication"] = json!(true);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    prepare_backdrop_append(api, id, 0).await;
    upload_artwork(api, id, "Backdrop/0").await;
    assert!(
        !media.join("extrathumbs/thumb0.png").exists(),
        "the first backdrop has a single output"
    );
    let mut config = server.clone();
    config["ImageSavingConvention"] = json!("Legacy");
    api.post("/System/Configuration", &config).await;
    prepare_backdrop_append(api, id, 1).await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert!(!duplicate.exists(), "Legacy uses one local output");
    config["ImageSavingConvention"] = json!("Compatible");
    api.post("/System/Configuration", &config).await;
    options["SaveLocalMetadata"] = json!(false);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    prepare_backdrop_append(api, id, 1).await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert!(
        !duplicate.exists(),
        "internal saving does not create a local duplicate"
    );
    options["SaveLocalMetadata"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    prepare_backdrop_append(api, id, 1).await;
    let images = api.get(&format!("/Items/{id}/Images")).await;
    std::fs::create_dir(&duplicate).unwrap();
    let response = api.client.post(format!("{}/Items/{id}/Images/Backdrop/1",api.base))
        .header("Authorization", &api.auth).header("Content-Type","image/png")
        .body("iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGP4z8AARAwQCgAf7gP9i18U1AAAAABJRU5ErkJggg==")
        .send().await.unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "a failed second output cannot silently fall back"
    );
    assert_eq!(
        api.get(&format!("/Items/{id}/Images")).await,
        images,
        "failed duplication must not publish a new image path"
    );
    std::fs::remove_dir(&duplicate).unwrap();
    prepare_backdrop_append(api, id, 1).await;
    upload_artwork(api, id, "Backdrop/1").await;
    assert_eq!(
        std::fs::read(&duplicate).unwrap(),
        POSTER,
        "a repaired destination succeeds without restart"
    );
    api.post("/System/Configuration/xbmcmetadata", &original)
        .await;
    api.post("/System/Configuration", &server).await;
    *options = original_options;
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
}

async fn verify_similarity_selection(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
    requests: &Mutex<Vec<String>>,
) {
    options["TypeOptions"][0]["MetadataFetchers"] = json!([]);
    options["TypeOptions"][0]["ImageFetchers"] = json!([]);
    options["TypeOptions"][0]["SimilarItemProviders"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let related = media.parent().unwrap().join("Remote similar");
    std::fs::create_dir_all(&related).unwrap();
    std::fs::write(related.join("Related.mkv"), b"fixture").unwrap();
    std::fs::write(
        related.join("movie.nfo"),
        "<movie><title>Remote similar</title><tmdbid>604</tmdbid></movie>",
    )
    .unwrap();
    api.post("/Library/Refresh", &Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(60);
    let candidate = loop {
        let items = api
            .get("/Items?recursive=true&includeItemTypes=Movie&fields=ProviderIds")
            .await;
        if let Some(item) = items["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["ProviderIds"]["Tmdb"] == "604")
        {
            break item["Id"].as_str().unwrap().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "related movie was not discovered: {items}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let path = format!("/Items/{id}/Similar?limit=1");
    let similar_calls = || {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|uri| uri.starts_with("/movie/603/similar"))
            .count()
    };
    let before = similar_calls();
    api.get(&path).await;
    assert_eq!(similar_calls(), before, "unchecked remote provider ran");
    options["TypeOptions"][0]["SimilarItemProviders"] = json!(["themoviedb"]);
    options["TypeOptions"][0]["SimilarItemProviderOrder"] =
        json!(["THEMOVIEDB", "Local Genre/Tag"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let result = api.get(&path).await;
    assert_eq!(result["Items"][0]["Id"], candidate, "{result}");
    assert_eq!(similar_calls(), before + 1);
    options["TypeOptions"][0]["SimilarItemProviders"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    api.get(&path).await;
    assert_eq!(
        similar_calls(),
        before + 1,
        "disabled provider/cache still ran"
    );
}

/// Each saved mode changes the next real refresh, preserves external subtitles,
/// and retains the merged stream indexes rather than closing gaps after filtering.
async fn verify_embedded_subtitle_options(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
    root: &std::path::Path,
) {
    let snapshot = root.join("embedded-subtitles.json");
    std::fs::write(&snapshot, json!({
        "streams":[
            {"index":0,"codec_type":"video","codec_name":"h264","width":64,"height":64},
            {"index":1,"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"eng"}},
            {"index":2,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle"},
            {"index":3,"codec_type":"subtitle"}
        ],
        "format":{"format_name":"matroska,webm","duration":"60.0","size":"1024","bit_rate":"1000"}
    }).to_string()).unwrap();
    let external = media.join("Locale.eng.srt");
    std::fs::write(&external, "1\n00:00:00,000 --> 00:00:01,000\nExternal\n").unwrap();
    let path = format!("/Items/{id}?fields=MediaSources,MediaStreams,Etag");
    let mut edited = api.get(&path).await;
    edited["LockData"] = json!(false);
    api.post(&format!("/Items/{id}"), &edited).await;
    for (mode, expected) in [
        ("AllowAll", vec![0, 1, 2, 3]),
        ("AllowText", vec![0, 1, 2]),
        ("AllowImage", vec![0, 1, 3]),
        ("AllowNone", vec![0, 1]),
        ("AllowAll", vec![0, 1, 2, 3]),
    ] {
        options["AllowEmbeddedSubtitles"] = json!(mode);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        let previous = api.get(&path).await["Etag"].clone();
        api.post(
            &format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None"),
            &Value::Null,
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let details = api.get(&path).await;
            let streams = details["MediaStreams"].as_array().unwrap();
            let indexes = streams
                .iter()
                .map(|s| s["Index"].as_i64().unwrap())
                .collect::<Vec<_>>();
            if indexes == expected && details["Etag"] != previous {
                assert_eq!(streams[0]["IsExternal"], true, "{mode}: {details}");
                assert_eq!(streams[0]["Path"], external.to_string_lossy().as_ref());
                assert_eq!(streams[1]["Type"], "Video");
                assert_eq!(details["HasSubtitles"], true);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{mode}: expected indexes {expected:?}, got {details}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    std::fs::remove_file(snapshot).unwrap();
    std::fs::remove_file(external).unwrap();
}

// Etag proves this refresh wrote the item; Idle also waits for download/save
// work that happens after item persistence rather than racing the provider.
async fn await_subtitle_refresh(api: &Api, library: &Value, id: &str) {
    let path = format!("/Items/{id}?fields=MediaStreams,Etag");
    let previous = api.get(&path).await["Etag"].clone();
    api.post(
        &format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None"),
        &Value::Null,
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let item = api.get(&path).await;
        let folders = api.get("/Library/VirtualFolders").await;
        if item["Etag"] != previous
            && folders
                .as_array()
                .unwrap()
                .iter()
                .any(|folder| &folder["ItemId"] == library && folder["RefreshStatus"] == "Idle")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "subtitle refresh did not finish: {item}, {folders}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn verify_automatic_subtitle_constraints(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
    root: &std::path::Path,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let original = options.clone();
    api.post(
        "/Plugins/4a3f8e216c944d17a2b80f5e9c3d7a10/Configuration",
        &json!({"Username":"fixture","Password":"fixture"}),
    )
    .await;
    std::fs::write(media.join("Locale.mkv"), vec![0_u8; 131_072]).unwrap();
    let snapshot = root.join("embedded-subtitles.json");
    std::fs::write(&snapshot, json!({
        "streams":[
            {"index":0,"codec_type":"video","codec_name":"h264","width":64,"height":64},
            {"index":1,"codec_type":"audio","codec_name":"aac","disposition":{"default":1},"tags":{"language":"fra"}},
            {"index":2,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle","tags":{"language":"spa"}}
        ],
        "format":{"format_name":"matroska,webm","duration":"60.0","size":"131072","bit_rate":"1000"}
    }).to_string()).unwrap();
    let mut saved = api.get(&format!("/Items/{id}")).await;
    saved["ProviderIds"] = json!({"Tmdb":"603", "IMDB":"tt0133093", "Custom":"subtitle-fixture"});
    api.post(&format!("/Items/{id}"), &saved).await;
    options["AllowEmbeddedSubtitles"] = json!("AllowNone");
    options["DisabledSubtitleFetchers"] = json!([]);
    options["SubtitleDownloadLanguages"] = json!(["eng"]);
    options["RequirePerfectSubtitleMatch"] = json!(true);
    options["SkipSubtitlesIfAudioTrackMatches"] = json!(false);
    options["SkipSubtitlesIfEmbeddedSubtitlesPresent"] = json!(false);
    options["SaveSubtitlesWithMedia"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    requests.lock().unwrap().clear();
    await_subtitle_refresh(api, library, id).await;
    assert!(
        !media.join("Locale.eng.srt").exists(),
        "perfect matching rejects the provider's title-only candidate"
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("/subtitles?")
                && uri.contains("moviehash_match=only")
                && uri.contains("imdb_id=0133093"))
    );
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri == "/download")
    );

    options["RequirePerfectSubtitleMatch"] = json!(false);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    requests.lock().unwrap().clear();
    await_subtitle_refresh(api, library, id).await;
    assert_eq!(
        std::fs::read_to_string(media.join("Locale.eng.srt")).unwrap(),
        "1\n00:00:00,000 --> 00:00:01,000\nDownloaded\n"
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("/subtitles?")
                && !uri.contains("moviehash_match=only")
                && uri.contains("imdb_id=0133093"))
    );

    // Manual search candidates carry the upstream MD5 provider namespace, which
    // routes both the raw-content and download endpoints; the readable provider
    // name is not an alias.
    let found = api
        .get(&format!("/Items/{id}/RemoteSearch/Subtitles/eng"))
        .await;
    let candidate = found[0]["Id"].as_str().unwrap().to_owned();
    let (namespace, local) = candidate.split_once('_').unwrap();
    assert_eq!(namespace, "29c0c3633fefe8ab4d5b9af1b0275f3d", "{found}");
    assert!(local.ends_with("-eng-42"), "{found}");
    for (path, expected) in [
        (format!("/Providers/Subtitles/Subtitles/{candidate}"), 200),
        (
            format!("/Providers/Subtitles/Subtitles/OpenSubtitles_{local}"),
            400,
        ),
    ] {
        let response = api
            .client
            .get(format!("{}{path}", api.base))
            .header("Authorization", &api.auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }

    for (language, flag) in [
        ("fra", "SkipSubtitlesIfAudioTrackMatches"),
        ("spa", "SkipSubtitlesIfEmbeddedSubtitlesPresent"),
    ] {
        options["SubtitleDownloadLanguages"] = json!([language]);
        options[flag] = json!(true);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        requests.lock().unwrap().clear();
        await_subtitle_refresh(api, library, id).await;
        assert!(
            !media.join(format!("Locale.{language}.srt")).exists(),
            "{flag} honors the matching raw probe stream"
        );
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|uri| uri.starts_with("/subtitles?"))
        );
        options[flag] = json!(false);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        await_subtitle_refresh(api, library, id).await;
        assert!(
            media.join(format!("Locale.{language}.srt")).is_file(),
            "changing {flag} affects the next refresh"
        );
    }

    // Matching external VobSub is an image subtitle, not a text match and not
    // an embedded stream. Neither suppression rule should prevent a request.
    std::fs::write(media.join("Locale.por.sub"), b"external VobSub").unwrap();
    options["SubtitleDownloadLanguages"] = json!(["por"]);
    options["SkipSubtitlesIfEmbeddedSubtitlesPresent"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    await_subtitle_refresh(api, library, id).await;
    assert!(media.join("Locale.por.sub").is_file());
    assert!(
        media.join("Locale.por.srt").is_file(),
        "external image subtitle does not satisfy the text language requirement"
    );

    options["SubtitleDownloadLanguages"] = json!(["ger"]);
    options["DisabledSubtitleFetchers"] = json!(["OPENSUBTITLES"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    requests.lock().unwrap().clear();
    await_subtitle_refresh(api, library, id).await;
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("/subtitles?"))
    );
    assert!(!media.join("Locale.ger.srt").exists());
    options["DisabledSubtitleFetchers"] = json!([]);
    options["SubtitleDownloadLanguages"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    requests.lock().unwrap().clear();
    await_subtitle_refresh(api, library, id).await;
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|uri| uri.starts_with("/subtitles?"))
    );
    for extension in ["eng.srt", "fra.srt", "spa.srt", "por.srt", "por.sub"] {
        std::fs::remove_file(media.join(format!("Locale.{extension}"))).unwrap();
    }
    std::fs::remove_file(snapshot).unwrap();
    *options = original;
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
}

/// The configured destination and collision names reach the real upload route.
#[allow(clippy::too_many_lines)]
async fn verify_subtitle_destinations(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
    root: &std::path::Path,
) {
    const CONTENT: &[u8] = b"1\n00:00:00,000 --> 00:00:01,000\nSubtitle fixture\n";
    // L20 removes its fixture sidecars; reconcile their persisted streams
    // before this independent destination scenario starts.
    await_subtitle_refresh(api, library, id).await;
    let initial = api.get(&format!("/Items/{id}?Fields=MediaStreams")).await;
    assert!(
        initial["MediaStreams"]
            .as_array()
            .unwrap()
            .iter()
            .all(|stream| stream["Type"] != "Subtitle")
    );
    let mut upload = json!({
        "Language":"ENG", "Format":"SRT", "IsForced":false,
        "IsHearingImpaired":false,
        "Data":"MQowMDowMDowMCwwMDAgLS0+IDAwOjAwOjAxLDAwMApTdWJ0aXRsZSBmaXh0dXJlCg=="
    });
    let dashless = uuid::Uuid::parse_str(id).unwrap().simple().to_string();
    let internal = root
        .join("data/metadata/library")
        .join(&dashless[..2])
        .join(&dashless);
    let endpoint = format!("/Videos/{id}/Subtitles");
    let mut expected_paths = Vec::new();
    for (with_media, filename) in [
        (false, "Locale.eng.srt"),
        (false, "Locale.eng.0.srt"),
        (true, "Locale.eng.srt"),
        (true, "Locale.eng.0.srt"),
        (false, "Locale.eng.1.srt"),
    ] {
        options["SaveSubtitlesWithMedia"] = json!(with_media);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        api.post(&endpoint, &upload).await;
        let destination = if with_media { media } else { &internal };
        let path = destination.join(filename);
        assert_eq!(std::fs::read(&path).unwrap(), CONTENT);
        expected_paths.push(path);
        if !with_media {
            assert!(
                !media.join("Locale.eng.1.srt").exists(),
                "disabled media writes must not consume the media folder's next filename"
            );
        }
        for path in &expected_paths {
            assert_eq!(std::fs::read(path).unwrap(), CONTENT, "{}", path.display());
        }
    }

    options["SaveSubtitlesWithMedia"] = json!(true);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    upload["Language"] = json!("pt-BR");
    upload["IsForced"] = json!(true);
    upload["IsHearingImpaired"] = json!(true);
    api.post(&endpoint, &upload).await;
    let path = media.join("Locale.pt-br.forced.sdh.srt");
    assert_eq!(std::fs::read(&path).unwrap(), CONTENT);
    expected_paths.push(path);
    let item = api.get(&format!("/Items/{id}?Fields=MediaStreams")).await;
    let subtitles: Vec<_> = item["MediaStreams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|stream| stream["Type"] == "Subtitle")
        .collect();
    assert_eq!(subtitles.len(), expected_paths.len(), "{item}");
    for path in &expected_paths {
        assert!(
            subtitles
                .iter()
                .any(|stream| stream["Path"] == path.to_str().unwrap()),
            "missing {}: {item}",
            path.display()
        );
    }
    assert!(subtitles.iter().any(|stream| stream["Language"] == "pt-br"
        && stream["IsForced"] == true
        && stream["IsHearingImpaired"] == true));

    // A directory at the exact filename is a deterministic write failure.
    // Selecting media storage must return it rather than retrying metadata.
    let blocked = media.join("Locale.fra.srt");
    std::fs::create_dir(&blocked).unwrap();
    upload["Language"] = json!("fra");
    upload["IsForced"] = json!(false);
    upload["IsHearingImpaired"] = json!(false);
    let response = api
        .client
        .post(format!("{}{endpoint}", api.base))
        .header("Authorization", &api.auth)
        .json(&upload)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(!internal.join("Locale.fra.srt").exists());
    std::fs::remove_dir(blocked).unwrap();

    for (language, format) in [("x/../../outside/pwned", "srt"), ("eng", "strm")] {
        upload["Language"] = json!(language);
        upload["Format"] = json!(format);
        let response = api
            .client
            .post(format!("{}{endpoint}", api.base))
            .header("Authorization", &api.auth)
            .json(&upload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    }
    let item = api.get(&format!("/Items/{id}?Fields=MediaStreams")).await;
    assert_eq!(
        item["MediaStreams"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|stream| stream["Type"] == "Subtitle")
            .count(),
        expected_paths.len(),
        "failed uploads must not append a stream: {item}"
    );
}

async fn nfo_delete(api: &Api, route: &str) {
    api.client
        .delete(format!("{}{route}", api.base))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

async fn await_nfo_contents(path: &std::path::Path, predicate: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let xml = std::fs::read_to_string(path).unwrap();
        if predicate(&xml) {
            return xml;
        }
        assert!(
            Instant::now() < deadline,
            "user-data mutation did not update the NFO: {xml}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// No item metadata edit occurs between these mutations and their sidecar
// assertions. Generic UserData updates/start/progress intentionally do not
// trigger the pinned hosted saver; rating/played/finished events do.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn verify_nfo_user_data_notifications(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    user_a: &str,
    user_b: &str,
    nfo: &std::path::Path,
    configuration: &mut Value,
) {
    configuration["UserId"] = json!(user_a);
    api.post("/System/Configuration/xbmcmetadata", configuration)
        .await;
    let baseline = std::fs::read_to_string(nfo).unwrap();
    let before_invalid_rating = api
        .get(&format!("/UserItems/{id}/UserData?userId={user_a}"))
        .await;
    let invalid = api
        .client
        .post(format!(
            "{}/UserItems/{id}/UserData?userId={user_a}",
            api.base
        ))
        .header("Authorization", &api.auth)
        .json(&json!({"Rating":11.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        api.get(&format!("/UserItems/{id}/UserData?userId={user_a}"))
            .await,
        before_invalid_rating
    );
    api.post(
        &format!("/UserItems/{id}/UserData?userId={user_a}"),
        &json!({"PlayCount":11,"Rating":6.5,"Played":false,"PlaybackPositionTicks":300_000_000}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(nfo).unwrap(),
        baseline,
        "UpdateUserData does not trigger an automatic export"
    );
    nfo_delete(api, &format!("/UserFavoriteItems/{id}?userId={user_a}")).await;
    await_nfo_contents(nfo, |xml| {
        xml.contains("<playcount>11</playcount>")
            && xml.contains("<isuserfavorite>false</isuserfavorite>")
            && xml.contains("<userrating>6.5</userrating>")
            && xml.contains("<position>30</position>")
    })
    .await;
    for (likes, rating) in [(true, "10"), (false, "1")] {
        api.post(
            &format!("/UserItems/{id}/Rating?userId={user_a}&likes={likes}"),
            &Value::Null,
        )
        .await;
        await_nfo_contents(nfo, |xml| {
            xml.contains(&format!("<userrating>{rating}</userrating>"))
        })
        .await;
    }
    nfo_delete(api, &format!("/UserItems/{id}/Rating?userId={user_a}")).await;
    await_nfo_contents(nfo, |xml| !xml.contains("<userrating>")).await;
    api.post(
        &format!("/UserPlayedItems/{id}?userId={user_a}&datePlayed=2022-03-04T05%3A06%3A07Z"),
        &Value::Null,
    )
    .await;
    await_nfo_contents(nfo, |xml| {
        xml.contains("<watched>true</watched>")
            && xml.contains("<playcount>12</playcount>")
            && xml.contains("<lastplayed>2022-03-04 05:06:07</lastplayed>")
            && xml.contains("<position>0</position>")
    })
    .await;
    nfo_delete(api, &format!("/UserPlayedItems/{id}?userId={user_a}")).await;
    await_nfo_contents(nfo, |xml| {
        xml.contains("<watched>false</watched>")
            && xml.contains("<playcount>0</playcount>")
            && !xml.contains("<lastplayed>")
    })
    .await;
    // Events from another user still export the configured hidden/disabled user.
    configuration["UserId"] = json!(user_b);
    api.post("/System/Configuration/xbmcmetadata", configuration)
        .await;
    api.post(
        &format!("/UserFavoriteItems/{id}?userId={user_a}"),
        &Value::Null,
    )
    .await;
    let selected_b = await_nfo_contents(nfo, |xml| {
        xml.contains("<playcount>2</playcount>") && xml.contains("<userrating>2.5</userrating>")
    })
    .await;
    configuration["UserId"] = json!(" \t\r\n");
    api.post("/System/Configuration/xbmcmetadata", configuration)
        .await;
    api.post(
        &format!("/UserItems/{id}/Rating?userId={user_a}&likes=true"),
        &Value::Null,
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(nfo).unwrap(),
        selected_b,
        "a blank live selection disables the notification consumer"
    );
    configuration["UserId"] = json!(user_a);
    api.post("/System/Configuration/xbmcmetadata", configuration)
        .await;
    options["MetadataSavers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    nfo_delete(api, &format!("/UserFavoriteItems/{id}?userId={user_a}")).await;
    assert_eq!(
        std::fs::read_to_string(nfo).unwrap(),
        selected_b,
        "the event still honors the live owning-library saver selection"
    );
    options["MetadataSavers"] = json!(["Nfo"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    api.post(
        &format!("/UserFavoriteItems/{id}?userId={user_a}"),
        &Value::Null,
    )
    .await;
    await_nfo_contents(nfo, |xml| {
        xml.contains("<isuserfavorite>true</isuserfavorite>")
            && xml.contains("<playcount>0</playcount>")
    })
    .await;
    // The authenticated admin's playback supplies a real finished event. Start
    // and progress may persist state but must leave the previous NFO untouched.
    let admin = api.get("/Users/Me").await["Id"]
        .as_str()
        .unwrap()
        .to_owned();
    configuration["UserId"] = json!(admin);
    api.post("/System/Configuration/xbmcmetadata", configuration)
        .await;
    let prior_playback = std::fs::read_to_string(nfo).unwrap();
    let runtime = api.get(&format!("/Items/{id}")).await["RunTimeTicks"]
        .as_i64()
        .unwrap_or_default();
    api.post("/Sessions/Playing", &json!({"ItemId":id,"MediaSourceId":id,"PlayMethod":"DirectPlay","PositionTicks":0,"CanSeek":true})).await;
    assert_eq!(
        std::fs::read_to_string(nfo).unwrap(),
        prior_playback,
        "PlaybackStart is not an NFO save reason"
    );
    api.post("/Sessions/Playing/Progress", &json!({"ItemId":id,"MediaSourceId":id,"PlayMethod":"DirectPlay","PositionTicks":runtime / 2,"IsPaused":false})).await;
    assert_eq!(
        std::fs::read_to_string(nfo).unwrap(),
        prior_playback,
        "PlaybackProgress is not an NFO save reason"
    );
    api.post(
        "/Sessions/Playing/Stopped",
        &json!({"ItemId":id,"MediaSourceId":id,"PositionTicks":runtime / 2,"Failed":false}),
    )
    .await;
    await_nfo_contents(nfo, |xml| {
        xml.contains("<playcount>1</playcount>")
            && xml.contains("<isuserfavorite>false</isuserfavorite>")
    })
    .await;
    let finished = std::fs::read_to_string(nfo).unwrap();
    api.post("/Sessions/Playing", &json!({"ItemId":id,"MediaSourceId":id,"PlayMethod":"DirectPlay","PositionTicks":0,"CanSeek":true})).await;
    assert_eq!(std::fs::read_to_string(nfo).unwrap(), finished);
    api.post(
        "/Sessions/Playing/Stopped",
        &json!({"ItemId":id,"MediaSourceId":id,"Failed":false}),
    )
    .await;
    await_nfo_contents(nfo, |xml| {
        xml.contains("<playcount>3</playcount>")
            && xml.contains("<watched>true</watched>")
            && xml.contains("<position>0</position>")
    })
    .await;
    // Detached saver failures are proven with a held/erroring core listener
    // and the native fixture's actual error receipt. Restoring a blocked NFO
    // immediately after HTTP would race the source async-void consumer.
}

// Named UserId is an export selection. The normal pinned NFO provider parses
// into an empty-ID temporary item, so its watched/playcount/lastplayed imports
// cannot persist into the real item's user data.
#[allow(clippy::too_many_lines)]
async fn verify_selected_nfo_user(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
) {
    let original_options = options.clone();
    let original_nfo_options = api.get("/System/Configuration/xbmcmetadata").await;
    options["MetadataSavers"] = json!(["Nfo"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    for name in ["NFO selected A", "NFO selected B"] {
        api.post("/Users/New", &json!({"Name":name})).await;
    }
    let users = api.get("/Users").await;
    let user_a = users
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["Name"] == "NFO selected A")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    let user_b = users
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["Name"] == "NFO selected B")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    api.post(&format!("/UserItems/{id}/UserData?userId={user_a}"), &json!({"IsFavorite":true,"Rating":8.5,"Played":true,"PlayCount":7,"PlaybackPositionTicks":42_000_000,"LastPlayedDate":"2020-01-02T03:04:05Z"})).await;
    api.post(
        &format!("/UserItems/{id}/UserData?userId={user_b}"),
        &json!({"IsFavorite":false,"Rating":2.5,"Played":false,"PlayCount":2}),
    )
    .await;
    let original_user_b_policy = api.get(&format!("/Users/{user_b}")).await["Policy"].clone();
    let mut policy = original_user_b_policy.clone();
    policy["IsHidden"] = json!(true);
    policy["IsDisabled"] = json!(true);
    policy["EnableAllFolders"] = json!(false);
    policy["EnabledFolders"] = json!([]);
    api.post(&format!("/Users/{user_b}/Policy"), &policy).await;
    let nfo = media.join("movie.nfo");
    let mut item = api.get(&format!("/Items/{id}")).await;
    let mut configuration = original_nfo_options.clone();
    let guid = uuid::Uuid::parse_str(user_a).unwrap();
    let (first, second, third, bytes) = guid.as_fields();
    let hex_bytes = bytes
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(",");
    let hex_guid = format!("{{0x{first:08x},0x{second:04x},0x{third:04x},{{{hex_bytes}}}}}");
    for (index, (selected, count, favorite)) in [
        (user_a.to_owned(), 7, "true"),
        (user_b.to_owned(), 2, "false"),
        (user_a.to_owned(), 7, "true"),
        (format!("\u{0085} \t{user_a}\r\n"), 7, "true"),
        (guid.simple().to_string(), 7, "true"),
        (format!("{{{guid}}}"), 7, "true"),
        (format!("({guid})"), 7, "true"),
        (hex_guid, 7, "true"),
    ]
    .into_iter()
    .enumerate()
    {
        configuration["UserId"] = json!(selected);
        api.post("/System/Configuration/xbmcmetadata", &configuration)
            .await;
        item["Name"] = json!(format!("NFO user selection {index}"));
        api.post(&format!("/Items/{id}"), &item).await;
        let xml = std::fs::read_to_string(&nfo).unwrap();
        assert!(
            xml.contains(&format!("<playcount>{count}</playcount>")),
            "{xml}"
        );
        assert!(
            xml.contains(&format!("<isuserfavorite>{favorite}</isuserfavorite>")),
            "{xml}"
        );
        assert!(
            xml.contains(&format!("<watched>{favorite}</watched>")),
            "{xml}"
        );
        let (rating, position) = if count == 7 {
            ("8.5", "4.2")
        } else {
            ("2.5", "0")
        };
        assert!(
            xml.contains(&format!("<userrating>{rating}</userrating>")),
            "{xml}"
        );
        assert!(
            xml.contains(&format!("<position>{position}</position>")),
            "{xml}"
        );
        let total: f64 = xml
            .split("<total>")
            .nth(1)
            .unwrap()
            .split("</total>")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let runtime = item["RunTimeTicks"].as_f64().unwrap_or_default();
        assert!((total - runtime / 10_000_000.0).abs() < 1e-9, "{xml}");
        if count == 7 {
            assert!(
                xml.contains("<lastplayed>2020-01-02 03:04:05</lastplayed>"),
                "{xml}"
            );
        } else {
            assert!(
                !xml.contains("<lastplayed>"),
                "the preceding selected user's date must be removed: {xml}"
            );
        }
    }
    verify_nfo_user_data_notifications(
        api,
        library,
        options,
        id,
        user_a,
        user_b,
        &nfo,
        &mut configuration,
    )
    .await;
    let saved = std::fs::read_to_string(&nfo).unwrap();
    for selected in [
        "invalid-user-id".to_owned(),
        format!("urn:uuid:{guid}"),
        uuid::Uuid::nil().to_string(),
    ] {
        configuration["UserId"] = json!(selected);
        api.post("/System/Configuration/xbmcmetadata", &configuration)
            .await;
        item["Name"] = json!(format!(
            "Metadata edit survives failing selection {selected}"
        ));
        api.post(&format!("/Items/{id}"), &item).await;
        assert_eq!(api.get(&format!("/Items/{id}")).await["Name"], item["Name"]);
        assert_eq!(
            std::fs::read_to_string(&nfo).unwrap(),
            saved,
            "failing selection must preserve the complete existing NFO"
        );
    }
    for selected in [
        "",
        " \t\u{0085}\u{00a0}\r\n",
        "00000000-0000-0000-0000-000000000001",
    ] {
        configuration["UserId"] = json!(selected);
        api.post("/System/Configuration/xbmcmetadata", &configuration)
            .await;
        api.post(&format!("/Items/{id}"), &item).await;
        let xml = std::fs::read_to_string(&nfo).unwrap();
        for field in [
            "playcount",
            "isuserfavorite",
            "watched",
            "userrating",
            "lastplayed",
            "resume",
        ] {
            assert!(
                !xml.contains(&format!("<{field}>")),
                "no user export for absent/unknown selection: {xml}"
            );
        }
    }
    // Restore B's access before its user-scoped API snapshots: the export
    // intentionally bypasses visibility, while these API reads do not.
    api.post(&format!("/Users/{user_b}/Policy"), &original_user_b_policy)
        .await;
    // Preserve both real users while a deliberately conflicting sidecar is
    // imported through the actual metadata refresh and local reader.
    configuration["UserId"] = json!(user_a);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    let a = api
        .get(&format!("/UserItems/{id}/UserData?userId={user_a}"))
        .await;
    let b = api
        .get(&format!("/UserItems/{id}/UserData?userId={user_b}"))
        .await;
    options["MetadataSavers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    item["LockData"] = json!(false);
    api.post(&format!("/Items/{id}"), &item).await;
    std::fs::write(&nfo, "<movie><title>NFO import probe</title><isuserfavorite>false</isuserfavorite><userrating>1</userrating><resume><position>999</position><total>999</total></resume><watched>false</watched><playcount>99</playcount><lastplayed>2025-06-07 08:09:10</lastplayed></movie>").unwrap();
    let previous = api.get(&format!("/Items/{id}")).await["Etag"].clone();
    api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"), &Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let current = api.get(&format!("/Items/{id}")).await;
        if current["Etag"] != previous && current["Name"] == "NFO import probe" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "NFO import must apply the actual title before checking unchanged user data"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        api.get(&format!("/UserItems/{id}/UserData?userId={user_a}"))
            .await,
        a
    );
    assert_eq!(
        api.get(&format!("/UserItems/{id}/UserData?userId={user_b}"))
            .await,
        b
    );
    api.post("/System/Configuration/xbmcmetadata", &original_nfo_options)
        .await;
    *options = original_options;
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
}

// Independently verify the live NFO artwork switch through actual image, item,
// cast and named-configuration APIs, including its ImageUpdate threshold.
#[allow(clippy::too_many_lines)]
async fn verify_nfo_image_path_setting(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
) {
    let original_options = options.clone();
    let original_configuration = api.get("/System/Configuration/xbmcmetadata").await;
    options["MetadataSavers"] = json!([]);
    options["TypeOptions"][0]["MetadataFetchers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let item_url = format!("/Items/{id}");
    let mut item = api.get(&item_url).await;
    item["LockData"] = json!(false);
    api.post(&item_url, &item).await;
    let nfo = media.join("movie.nfo");
    std::fs::write(&nfo, "<movie><title>NFO image paths fixture</title><actor><name>NFO Actor</name><role>Reader</role><type>Actor</type></actor></movie>").unwrap();
    let previous = item["Etag"].clone();
    api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"), &Value::Null).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let actor = loop {
        let found = api.get("/Persons?searchTerm=NFO%20Actor").await;
        let current = api.get(&item_url).await;
        if let Some(actor) = found["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|actor| actor["Name"] == "NFO Actor")
            && current["Name"] == "NFO image paths fixture"
            && current["Etag"] != previous
            && current["People"].as_array().is_some_and(|people| {
                people.iter().any(|person| {
                    person["Id"] == actor["Id"]
                        && person["Name"] == "NFO Actor"
                        && person["Role"] == "Reader"
                })
            })
        {
            break actor["Id"].as_str().unwrap().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "the real NFO import did not materialize its credited person"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    upload_artwork(api, &actor, "Primary").await;
    upload_artwork(api, id, "Primary").await;
    let actor_images = api.get(&format!("/Items/{actor}/Images")).await;
    let actor_path = actor_images
        .as_array()
        .unwrap()
        .iter()
        .find(|image| image["ImageType"] == "Primary")
        .unwrap()["Path"]
        .as_str()
        .unwrap();
    options["MetadataSavers"] = json!(["Nfo"]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let mut configuration = original_configuration.clone();
    for (enabled, image_kind) in [(false, "Logo"), (true, "Backdrop"), (false, "Thumb")] {
        configuration["SaveImagePathsInNfo"] = json!(enabled);
        api.post("/System/Configuration/xbmcmetadata", &configuration)
            .await;
        item = api.get(&item_url).await;
        item["Name"] = json!(format!("NFO artwork {enabled}"));
        api.post(&item_url, &item).await;
        let before = std::fs::read_to_string(&nfo).unwrap();
        assert_eq!(before.contains("<art>"), enabled, "{before}");
        assert_eq!(
            before.contains(actor_path),
            enabled,
            "the real credited person's local image follows the setting: {before}"
        );
        assert!(before.contains("<name>NFO Actor</name>"));
        let previous_images = api.get(&format!("/Items/{id}/Images")).await;
        upload_artwork(api, id, image_kind).await;
        let after = std::fs::read_to_string(&nfo).unwrap();
        if enabled {
            let images = api.get(&format!("/Items/{id}/Images")).await;
            let new_path = images
                .as_array()
                .unwrap()
                .iter()
                .find(|image| {
                    image["ImageType"] == image_kind
                        && !previous_images.as_array().unwrap().iter().any(|old| {
                            old["ImageType"] == image_kind && old["Path"] == image["Path"]
                        })
                })
                .unwrap()["Path"]
                .as_str()
                .unwrap();
            assert!(
                after.contains(new_path),
                "ImageUpdate must refresh the NFO when image paths are enabled: {after}"
            );
            assert!(after.contains(actor_path));
            for image in images.as_array().unwrap() {
                let image_path = image["Path"].as_str().unwrap();
                if image["ImageType"] == "Primary" {
                    assert!(
                        after.contains(&format!("<poster>{image_path}</poster>")),
                        "{after}"
                    );
                } else if image["ImageType"] == "Backdrop" {
                    assert!(
                        after.contains(&format!("<fanart>{image_path}</fanart>")),
                        "{after}"
                    );
                } else {
                    assert!(
                        !after.contains(image_path),
                        "pinned art exports only poster/fanart: {after}"
                    );
                }
            }
            assert_ne!(after, before);
            // Banner triggers an enabled saver, but is not an exported field;
            // the source's byte comparison leaves an identical document intact.
            upload_artwork(api, id, "Banner").await;
            let banner_images = api.get(&format!("/Items/{id}/Images")).await;
            let banner_path = banner_images
                .as_array()
                .unwrap()
                .iter()
                .find(|image| image["ImageType"] == "Banner")
                .unwrap()["Path"]
                .as_str()
                .unwrap();
            let unchanged = std::fs::read_to_string(&nfo).unwrap();
            assert_eq!(unchanged, after, "Banner is not an NFO art field");
            assert!(!unchanged.contains(banner_path));
        } else {
            assert_eq!(
                after, before,
                "ImageUpdate remains below the saver threshold when paths are off"
            );
            assert!(!after.contains(actor_path));
        }
    }
    verify_nfo_path_substitution_option(api, id, actor_path, &nfo).await;
    // The existing saver logs a filesystem error without undoing the image.
    configuration["SaveImagePathsInNfo"] = json!(true);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    let before = std::fs::read_to_string(&nfo).unwrap();
    let old_images = api.get(&format!("/Items/{id}/Images")).await;
    let old_backdrops: Vec<_> = old_images
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == "Backdrop")
        .collect();
    std::fs::remove_file(&nfo).unwrap();
    std::fs::create_dir(&nfo).unwrap();
    upload_artwork(api, id, "Backdrop").await;
    assert!(nfo.is_dir());
    let current_images = api.get(&format!("/Items/{id}/Images")).await;
    let new_backdrops: Vec<_> = current_images
        .as_array()
        .unwrap()
        .iter()
        .filter(|image| image["ImageType"] == "Backdrop")
        .collect();
    assert_eq!(
        new_backdrops.len(),
        old_backdrops.len() + 1,
        "the failing saver must not undo the newly appended image"
    );
    let new_image = new_backdrops
        .iter()
        .find(|image| !old_backdrops.iter().any(|old| old["Path"] == image["Path"]))
        .unwrap();
    assert!(std::path::Path::new(new_image["Path"].as_str().unwrap()).is_file());
    let served = api
        .client
        .get(format!(
            "{}/Items/{id}/Images/Backdrop/{}",
            api.base,
            old_backdrops.len()
        ))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        !served.is_empty(),
        "the newly saved image must remain readable through HTTP"
    );
    std::fs::remove_dir(&nfo).unwrap();
    std::fs::write(&nfo, before).unwrap();
    api.post(
        "/System/Configuration/xbmcmetadata",
        &original_configuration,
    )
    .await;
    *options = original_options;
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
}

// The pin retains EnablePathSubstitution as a persisted UI option, but its
// saver always maps local image paths through the server substitutions.
#[allow(clippy::too_many_lines)]
async fn verify_nfo_path_substitution_option(
    api: &Api,
    id: &str,
    actor_path: &str,
    nfo: &std::path::Path,
) {
    let server = api.get("/System/Configuration").await;
    let original = api.get("/System/Configuration/xbmcmetadata").await;
    let images = api.get(&format!("/Items/{id}/Images")).await;
    let poster = images
        .as_array()
        .unwrap()
        .iter()
        .find(|image| image["ImageType"] == "Primary")
        .unwrap()["Path"]
        .clone();
    let backdrop = images
        .as_array()
        .unwrap()
        .iter()
        .find(|image| image["ImageType"] == "Backdrop")
        .unwrap()["Path"]
        .clone();
    let mut mapped = server.clone();
    mapped["PathSubstitutions"] = json!([
        {"From":poster,"To":"/nfo-substitution/poster.png"},
        {"From":backdrop,"To":"/nfo-substitution/fanart.png"},
        {"From":actor_path,"To":"/nfo-substitution/actor.png"}
    ]);
    api.post("/System/Configuration", &mapped).await;
    let mut configuration = original.clone();
    configuration["SaveImagePathsInNfo"] = json!(true);
    for (phase, enabled) in [false, true, false].into_iter().enumerate() {
        configuration["EnablePathSubstitution"] = json!(enabled);
        api.post("/System/Configuration/xbmcmetadata", &configuration)
            .await;
        assert_eq!(
            api.get("/System/Configuration/xbmcmetadata").await["EnablePathSubstitution"],
            enabled
        );
        let mut item = api.get(&format!("/Items/{id}")).await;
        item["Name"] = json!(format!("NFO substitution {phase}"));
        api.post(&format!("/Items/{id}"), &item).await;
        let xml = std::fs::read_to_string(nfo).unwrap();
        assert!(
            xml.contains(&format!("<title>NFO substitution {phase}</title>")),
            "{xml}"
        );
        assert!(
            xml.contains("<poster>/nfo-substitution/poster.png</poster>"),
            "{xml}"
        );
        assert!(
            xml.contains("<fanart>/nfo-substitution/fanart.png</fanart>"),
            "{xml}"
        );
        assert!(
            xml.contains("<thumb>/nfo-substitution/actor.png</thumb>"),
            "{xml}"
        );
        assert_eq!(api.get(&format!("/Items/{id}/Images")).await, images);
        for path in [
            poster.as_str().unwrap(),
            backdrop.as_str().unwrap(),
            actor_path,
        ] {
            assert!(
                std::path::Path::new(path).is_file(),
                "mapping must only change exported XML"
            );
        }
    }
    configuration["SaveImagePathsInNfo"] = json!(false);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    let mut item = api.get(&format!("/Items/{id}")).await;
    item["Name"] = json!("NFO substitution image fields disabled");
    api.post(&format!("/Items/{id}"), &item).await;
    let xml = std::fs::read_to_string(nfo).unwrap();
    assert!(!xml.contains("<art>") && !xml.contains("<thumb>"), "{xml}");
    api.post("/System/Configuration", &server).await;
    configuration["SaveImagePathsInNfo"] = json!(true);
    api.post("/System/Configuration/xbmcmetadata", &configuration)
        .await;
    item["Name"] = json!("NFO substitution mappings removed");
    api.post(&format!("/Items/{id}"), &item).await;
    let xml = std::fs::read_to_string(nfo).unwrap();
    assert!(xml.contains(&format!("<poster>{}</poster>", poster.as_str().unwrap())));
    assert!(xml.contains(&format!("<thumb>{actor_path}</thumb>")));
    assert!(!xml.contains("/nfo-substitution/"));
    api.post("/System/Configuration/xbmcmetadata", &original)
        .await;
}

fn mapped_nfo_fanarts(xml: &str) -> Vec<String> {
    xml.split("<fanart>")
        .skip(1)
        .filter_map(|tail| tail.split_once("</fanart>").map(|(path, _)| path))
        .filter(|path| path.starts_with("/nfo-order/"))
        .map(str::to_owned)
        .collect()
}

// Exact original-path ordering, live Images switch and inherited request culture
// reach the real metadata, image-update, queued refresh and detached saver paths.
#[allow(clippy::too_many_lines)]
async fn verify_nfo_image_order_culture(
    api: &Api,
    library: &Value,
    options: &mut Value,
    id: &str,
    media: &std::path::Path,
) {
    let original_options = options.clone();
    let original_server = api.get("/System/Configuration").await;
    let original_nfo = api.get("/System/Configuration/xbmcmetadata").await;
    let extra = media.join("extrafanart");
    std::fs::create_dir_all(&extra).unwrap();
    let a = extra.join("ä.png");
    let z = extra.join("z.png");
    for path in [&a, &z] {
        std::fs::write(path, POSTER).unwrap();
    }
    let mut server = original_server.clone();
    server["PathSubstitutions"] = json!([
        {"From":a,"To":"/nfo-order/Z-export.png"},
        {"From":z,"To":"/nfo-order/A-export.png"}
    ]);
    // RequestLocalizationOptions captured en-US at startup. The live UI
    // localization setting changes independently and cannot replace that default.
    server["UICulture"] = json!("sv");
    api.post("/System/Configuration", &server).await;
    let mut nfo_options = original_nfo.clone();
    nfo_options["SaveImagePathsInNfo"] = json!(true);
    nfo_options["UserId"] = api.get("/Users/Me").await["Id"].clone();
    api.post("/System/Configuration/xbmcmetadata", &nfo_options)
        .await;
    options["MetadataSavers"] = json!(["Nfo"]);
    options["TypeOptions"][0]["MetadataFetchers"] = json!([]);
    options["TypeOptions"][0]["ImageFetchers"] = json!([]);
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    let nfo = media.join("movie.nfo");
    let en = vec![
        "/nfo-order/Z-export.png".to_owned(),
        "/nfo-order/A-export.png".to_owned(),
    ];
    let sv = vec![
        "/nfo-order/A-export.png".to_owned(),
        "/nfo-order/Z-export.png".to_owned(),
    ];
    api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=FullRefresh&replaceAllMetadata=true"), &Value::Null).await;
    await_nfo_contents(&nfo, |xml| mapped_nfo_fanarts(xml) == en).await;
    let images = api.get(&format!("/Items/{id}/Images")).await;
    for path in [&a, &z] {
        assert!(
            images
                .as_array()
                .unwrap()
                .iter()
                .any(|image| image["ImageType"] == "Backdrop"
                    && image["Path"] == path.to_str().unwrap()),
            "real local artwork must be cataloged: {images}"
        );
    }
    // This verifies Ferrofin's implemented queued caller path. The native full
    // library proof starts persistent workers with aligned Swedish culture;
    // it does not establish that every worker inherits the latest request.
    api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true&culture=sv-SE"), &Value::Null).await;
    await_nfo_contents(&nfo, |xml| mapped_nfo_fanarts(xml) == sv).await;
    let response = api.client.post(format!("{}/Items/{id}/Images/Logo?culture=en-US", api.base))
        .header("Authorization", &api.auth).header("Content-Type", "image/png")
        .header("Accept-Language", "sv").header("Cookie", ".AspNetCore.Culture=c=sv|uic=sv")
        .body("iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGP4z8AARAwQCgAf7gP9i18U1AAAAABJRU5ErkJggg==")
        .send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.headers()["Content-Language"], "en-US");
    assert_eq!(
        mapped_nfo_fanarts(&std::fs::read_to_string(&nfo).unwrap()),
        en,
        "HTTP query culture precedes cookie and Accept-Language during ImageUpdate"
    );
    // A rating/favorite notification returns before filesystem work finishes;
    // observe the actual XML without an intervening metadata edit.
    api.post(
        &format!("/UserFavoriteItems/{id}?culture=sv-SE"),
        &Value::Null,
    )
    .await;
    await_nfo_contents(&nfo, |xml| mapped_nfo_fanarts(xml) == sv).await;
    let response = api.client.post(format!("{}/Items/{id}/Images/Logo", api.base))
        .header("Authorization", &api.auth).header("Content-Type", "image/png")
        .header("Cookie", ".aspnetcore.cULTURE=c=en-US|uic=en-US")
        .header("Accept-Language", "sv")
        .body("iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR4nGP4z8AARAwQCgAf7gP9i18U1AAAAABJRU5ErkJggg==")
        .send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        mapped_nfo_fanarts(&std::fs::read_to_string(&nfo).unwrap()),
        en,
        "HTTP cookie precedes Accept-Language"
    );
    // /Library/Refresh adds a scheduled-task spawn before the scan queue.
    // A fresh local image makes this default scan produce an actual update.
    let later = extra.join("later.png");
    std::fs::write(&later, POSTER).unwrap();
    let tasks = api.get("/ScheduledTasks").await;
    let previous_end = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["Key"] == "RefreshLibrary")
        .unwrap()["LastExecutionResult"]["EndTimeUtc"]
        .clone();
    api.post("/Library/Refresh?culture=sv-SE", &Value::Null)
        .await;
    await_nfo_contents(&nfo, |xml| mapped_nfo_fanarts(xml) == sv).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = api.get("/ScheduledTasks").await;
        let task = tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["Key"] == "RefreshLibrary")
            .unwrap();
        if task["State"] == "Idle" && task["LastExecutionResult"]["EndTimeUtc"] != previous_end {
            assert_eq!(task["LastExecutionResult"]["Status"], "Completed");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fresh full-library task must finish: {tasks}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let images = api.get(&format!("/Items/{id}/Images")).await;
    assert!(
        images
            .as_array()
            .unwrap()
            .iter()
            .any(|image| image["ImageType"] == "Backdrop"
                && image["Path"] == later.to_str().unwrap()),
        "scheduled refresh must actually catalog the new local artwork: {images}"
    );
    nfo_options["SaveImagePathsInNfo"] = json!(false);
    api.post("/System/Configuration/xbmcmetadata", &nfo_options)
        .await;
    let mut item = api.get(&format!("/Items/{id}")).await;
    item["Name"] = json!("NFO order switch disabled");
    api.post(&format!("/Items/{id}?culture=sv-SE"), &item).await;
    assert!(!std::fs::read_to_string(&nfo).unwrap().contains("<art>"));
    api.post("/System/Configuration/xbmcmetadata", &original_nfo)
        .await;
    api.post("/System/Configuration", &original_server).await;
    *options = original_options;
    api.post(
        "/Library/VirtualFolders/LibraryOptions",
        &json!({"Id":library,"LibraryOptions":options}),
    )
    .await;
    for path in [&a, &z, &later] {
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_dir(extra).unwrap();
}

/// A language-only save reaches an existing inherited item without restarting.
async fn verify_server_metadata_language(api: &Api, config: &mut Value, refresh: &str) {
    assert_eq!(config["MetadataCountryCode"], "AR");
    for (language, name) in [("fr", "Locale fr"), ("es-419", "Locale es-AR")] {
        config["PreferredMetadataLanguage"] = json!(language);
        api.post("/System/Configuration", config).await;
        api.post(refresh, &Value::Null).await;
        let item = api.await_movie(name, "AR-13").await;
        assert_eq!(item["Overview"], name.replace("Locale", "Overview"));
        assert_eq!(
            api.get("/System/Configuration").await["MetadataCountryCode"],
            "AR"
        );
    }
}

async fn seed_by_name_locale_items(
    api: &Api,
    provider_offset: u32,
) -> (
    ferrofin_core::FerrofinItemPersistenceService,
    [uuid::Uuid; 2],
) {
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    let db = ferrofin_db::Database::connect(&api.database_url)
        .await
        .unwrap();
    let persistence = ferrofin_core::FerrofinItemPersistenceService::new(db);
    let ids = [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
    for ((id, kind), provider) in ids
        .into_iter()
        .zip(["Person", "Movies.BoxSet"])
        .zip([287 + provider_offset, 2344 + provider_offset])
    {
        persistence
            .save_items(&[ferrofin_db::entities::base_items::BaseItemEntity {
                id: ferrofin_db::store::guid_to_db(id),
                type_: format!("MediaBrowser.Controller.Entities.{kind}"),
                name: Some(format!("Locale {kind}")),
                is_folder: kind == "Movies.BoxSet",
                ..Default::default()
            }])
            .await
            .unwrap();
        persistence
            .replace_provider_ids(id, &[("Tmdb".to_owned(), provider.to_string())])
            .await
            .unwrap();
    }
    (persistence, ids)
}

async fn await_by_name_overview(api: &Api, id: uuid::Uuid, expected: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let item = api.get(&format!("/Items/{id}")).await;
        if item["Overview"] == expected {
            return item;
        }
        assert!(Instant::now() < deadline, "expected {expected}: {item}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Pathless people/collections use the live server pair. Artwork tags survive
/// parsing and change the first selection, including regional prefix matching.
#[allow(clippy::too_many_lines)]
async fn verify_by_name_metadata_languages(
    api: &Api,
    config: &mut Value,
    requests: &Mutex<Vec<String>>,
) {
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    let (persistence, ids) = seed_by_name_locale_items(api, 0).await;
    for (language, resolved, image_tag) in [
        ("fr", "fr", "fr"),
        ("de", "de", "de"),
        ("es-419", "es-AR", "es-419"),
    ] {
        config["PreferredMetadataLanguage"] = json!(language);
        api.post("/System/Configuration", config).await;
        for (id, label) in ids
            .into_iter()
            .zip(["Person biography", "Collection overview"])
        {
            requests.lock().unwrap().clear();
            api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=FullRefresh&replaceAllMetadata=true&replaceAllImages=true"), &Value::Null).await;
            let item = await_by_name_overview(api, id, &format!("{label} {resolved}")).await;
            assert_eq!(
                item["Id"]
                    .as_str()
                    .map(|raw| uuid::Uuid::parse_str(raw).unwrap()),
                Some(id)
            );
            assert!(item["ImageTags"]["Primary"].is_string(), "{item}");
            let expected_image = if language == "es-419" { "es" } else { language };
            assert!(
                requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|uri| uri.starts_with(&format!("/original/locale-{expected_image}.png"))),
                "automatic acquisition must select the newly requested language"
            );
            let provider_path = if label == "Person biography" {
                "/person/"
            } else {
                "/collection/"
            };
            let provider_queries: Vec<_> = requests
                .lock()
                .unwrap()
                .iter()
                .filter(|uri| uri.starts_with(provider_path))
                .map(|uri| {
                    reqwest::Url::parse(&format!("http://fixture{uri}"))
                        .unwrap()
                        .query_pairs()
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect::<std::collections::HashMap<_, _>>()
                })
                .collect();
            let provider_languages: Vec<_> = provider_queries
                .iter()
                .map(|query| query.get("language").cloned())
                .collect();
            assert!(
                provider_languages
                    .iter()
                    .any(|language| language.as_deref() == Some(resolved)),
                "metadata request must carry the resolved locale: {provider_languages:?}"
            );
            if label == "Collection overview" {
                assert!(
                    provider_queries
                        .iter()
                        .any(
                            |query| query.get("language").map(String::as_str) == Some(resolved)
                                && query.get("include_image_language").map(String::as_str)
                                    == Some(format!("{resolved},null,en").as_str())
                        ),
                    "metadata collection image list: {provider_queries:?}"
                );
                assert!(
                    provider_queries
                        .iter()
                        .any(|query| !query.contains_key("language")
                            && !query.contains_key("include_image_language")),
                    "BoxSet artwork must request all languages: {provider_queries:?}"
                );
            } else {
                assert!(
                    provider_queries
                        .iter()
                        .all(|query| !query.contains_key("include_image_language")),
                    "Person uses normalized language without an image-language list: {provider_queries:?}"
                );
            }
            let images = api
                .get(&format!("/Items/{id}/RemoteImages?providerName=TheMovieDb"))
                .await;
            let images = images["Images"].as_array().unwrap();
            assert_eq!(images.len(), 3, "preferred/untagged/English: {images:?}");
            assert_eq!(images[0]["Language"], image_tag);
            assert_eq!(images[0]["Width"], 1600);
            assert!(images[1]["Language"].is_null() || images[1]["Language"] == "");
            let image = api
                .client
                .get(format!("{}/Items/{id}/Images/Primary", api.base))
                .header("Authorization", &api.auth)
                .send()
                .await
                .unwrap();
            assert_eq!(image.status(), reqwest::StatusCode::OK);
            assert_eq!(image.bytes().await.unwrap().as_ref(), POSTER);
        }
        assert_eq!(
            api.get("/System/Configuration").await["MetadataCountryCode"],
            "AR"
        );
    }
    persistence.delete_items(&ids).await.unwrap();
}

/// Country-only changes reach both provider certificates and the live rating list.
async fn verify_server_metadata_country(api: &Api, config: &mut Value, refresh: &str) {
    config["PreferredMetadataLanguage"] = json!("fr");
    api.post("/System/Configuration", config).await;
    for (country, rating, listed) in [
        ("DE", "FSK-12", "FSK-12"),
        ("FR", "FR-12", "TP"),
        ("AR", "AR-13", "ATP"),
    ] {
        config["MetadataCountryCode"] = json!(country);
        api.post("/System/Configuration", config).await;
        let ratings = api.get("/Localization/ParentalRatings").await;
        assert!(
            ratings
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["Name"] == listed),
            "{country}: {ratings}"
        );
        if country != "DE" {
            assert!(
                ratings
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|entry| entry["Name"] != "FSK-12"),
                "stale German dataset: {ratings}"
            );
        }
        api.post(refresh, &Value::Null).await;
        api.await_movie("Locale fr", rating).await;
        assert_eq!(
            api.get("/System/Configuration").await["PreferredMetadataLanguage"],
            "fr"
        );
    }
    config["PreferredMetadataLanguage"] = json!("es-419");
    api.post("/System/Configuration", config).await;
    api.post(refresh, &Value::Null).await;
    api.await_movie("Locale es-AR", "AR-13").await;
}

/// Cold provider IDs prove the country input to regional language resolution.
/// The pinned client's warm cache omits country from its key; keep that broader
/// cache behavior separate from the setting's actual metadata lookup input.
async fn verify_by_name_metadata_countries(api: &Api, config: &mut Value) {
    use ferrofin_traits::persistence::ItemPersistenceService as _;
    assert_eq!(config["PreferredMetadataLanguage"], "es-419");
    for (offset, country, expected) in [(70_000, "AR", "es-AR"), (71_000, "FR", "es-MX")] {
        config["MetadataCountryCode"] = json!(country);
        api.post("/System/Configuration", config).await;
        let (persistence, ids) = seed_by_name_locale_items(api, offset).await;
        for (id, label) in ids
            .into_iter()
            .zip(["Person biography", "Collection overview"])
        {
            api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=ValidationOnly&replaceAllMetadata=true"), &Value::Null).await;
            await_by_name_overview(api, id, &format!("{label} {expected}")).await;
        }
        assert_eq!(
            api.get("/System/Configuration").await["PreferredMetadataLanguage"],
            "es-419"
        );
        persistence.delete_items(&ids).await.unwrap();
    }
    config["MetadataCountryCode"] = json!("AR");
    api.post("/System/Configuration", config).await;
}

/// Live dashboard display policy positions physical specials in episode lists and NextUp.
#[allow(clippy::too_many_lines)]
async fn verify_special_episode_display(api: &Api, root: &std::path::Path) {
    let tv = root.join("special-display");
    let show = tv.join("Special Show");
    std::fs::create_dir_all(&show).unwrap();
    std::fs::write(
        show.join("tvshow.nfo"),
        "<tvshow><title>Special Show</title></tvshow>",
    )
    .unwrap();
    for (season, episode, title, aired) in [
        (
            0,
            1,
            "Z Special",
            "<airsbefore_season>1</airsbefore_season><airsbefore_episode>2</airsbefore_episode><tag>hidden-special</tag>",
        ),
        (1, 1, "One", ""),
        (1, 2, "Two", ""),
        (2, 1, "Other", ""),
    ] {
        let folder = show.join(format!("Season {season:02}"));
        std::fs::create_dir_all(&folder).unwrap();
        let stem = format!("Special Show S{season:02}E{episode:02}");
        std::fs::write(folder.join(format!("{stem}.mkv")), b"fixture").unwrap();
        std::fs::write(folder.join(format!("{stem}.nfo")), format!("<episodedetails><title>{title}</title><season>{season}</season><episode>{episode}</episode>{aired}</episodedetails>")).unwrap();
    }
    let options = json!({"PathInfos":[{"Path":tv}],"EnableInternetProviders":false,
        "EnableRealtimeMonitor":false,"MetadataSavers":[],"EnableChapterImageExtraction":false,
        "EnableTrickplayImageExtraction":false,
        "TypeOptions":[{"Type":"Series","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Season","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Episode","MetadataFetchers":[],"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Specials&collectionType=tvshows&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    let tasks = api.get("/ScheduledTasks").await;
    let task = tasks["Items"]
        .as_array()
        .or_else(|| tasks.as_array())
        .unwrap()
        .iter()
        .find(|task| task["Key"] == "RefreshLibrary")
        .expect("registered refresh task");
    let task_id = task["Id"].as_str().unwrap();
    let previous = task["LastExecutionResult"]["EndTimeUtc"].clone();
    api.post(&format!("/ScheduledTasks/Running/{task_id}"), &Value::Null)
        .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let task = api.get(&format!("/ScheduledTasks/{task_id}")).await;
        if task["State"] == "Idle" && task["LastExecutionResult"]["EndTimeUtc"] != previous {
            assert_eq!(task["LastExecutionResult"]["Status"], "Completed", "{task}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "specials refresh did not complete: {task}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let series = loop {
        let rows = api
            .get("/Items?recursive=true&includeItemTypes=Series")
            .await;
        if let Some(series) = rows["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["Name"] == "Special Show")
        {
            let episodes = api
                .get(&format!(
                    "/Items?recursive=true&includeItemTypes=Episode&seriesId={}",
                    series["Id"].as_str().unwrap()
                ))
                .await;
            if ["One", "Two", "Other", "Z Special"].iter().all(|name| {
                episodes["Items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["Name"] == *name)
            }) {
                break series.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "special show metadata did not settle: {rows}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let series_id = series["Id"].as_str().unwrap();
    let seasons = api.get(&format!("/Shows/{series_id}/Seasons")).await;
    let season_id = seasons["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|season| season["IndexNumber"] == 1)
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    let names = |result: &Value| {
        result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["Name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let users = api.get("/Users").await;
    let user = users
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["Name"] == "admin")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    let first = api
        .get(&format!("/Shows/{series_id}/Episodes?season=1"))
        .await;
    let first_id = first["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Name"] == "One")
        .unwrap()["Id"]
        .as_str()
        .unwrap();
    api.post(
        &format!("/Users/{user}/PlayedItems/{first_id}"),
        &Value::Null,
    )
    .await;
    let mut config = api.get("/System/Configuration").await;
    for enabled in [false, true, false] {
        config["DisplaySpecialsWithinSeasons"] = json!(enabled);
        api.post("/System/Configuration", &config).await;
        let wanted = if enabled {
            vec!["One", "Z Special", "Two"]
        } else {
            vec!["One", "Two"]
        };
        for query in ["season=1".to_owned(), format!("seasonId={season_id}")] {
            let result = api
                .get(&format!("/Shows/{series_id}/Episodes?{query}"))
                .await;
            assert_eq!(names(&result), wanted, "{query}: {result}");
            assert_eq!(result["TotalRecordCount"], wanted.len());
        }
        for recursive in [false, true] {
            let result = api
                .get(&format!(
                    "/Items?parentId={season_id}&recursive={recursive}&collapseBoxSetItems=true"
                ))
                .await;
            assert_eq!(names(&result), wanted, "Season.GetItemsInternal: {result}");
            assert_eq!(result["TotalRecordCount"], wanted.len());
            let page = api
                .get(&format!(
                    "/Items?parentId={season_id}&recursive={recursive}&startIndex=1&limit=1"
                ))
                .await;
            assert_eq!(names(&page), [if enabled { "Z Special" } else { "Two" }]);
            assert_eq!(page["TotalRecordCount"], wanted.len());
        }
        let unplayed = api
            .get(&format!("/Items?parentId={season_id}&filters=IsUnplayed"))
            .await;
        assert_eq!(
            names(&unplayed),
            if enabled {
                vec!["Z Special", "Two"]
            } else {
                vec!["Two"]
            }
        );
        let wrong_kind = api
            .get(&format!(
                "/Items?parentId={season_id}&includeItemTypes=Movie"
            ))
            .await;
        assert!(names(&wrong_kind).is_empty());
        assert_eq!(wrong_kind["TotalRecordCount"], 0);
        let descending = api
            .get(&format!(
                "/Items?parentId={season_id}&sortBy=AiredEpisodeOrder&sortOrder=Descending"
            ))
            .await;
        assert_eq!(
            names(&descending),
            if enabled {
                vec!["Two", "Z Special", "One"]
            } else {
                vec!["Two", "One"]
            }
        );
        let all_ids = api
            .get(&format!("/Shows/{series_id}/Episodes?season=0"))
            .await;
        let special_id = all_ids["Items"][0]["Id"].as_str().unwrap();
        let explicit = api.get(&format!("/Items?parentId={season_id}&ids={first_id},{special_id}&collapseBoxSetItems=true&sortBy=SortName")).await;
        let mut requested_names = names(&explicit);
        requested_names.sort();
        assert_eq!(
            requested_names,
            ["One", "Z Special"],
            "HTTP explicit IDs bypass aired selection and parent scope"
        );
        assert_eq!(explicit["TotalRecordCount"], 2);
        // SeasonId follows the season's actual Series, independent of route id.
        let result = api
            .get(&format!(
                "/Shows/{}/Episodes?seasonId={season_id}",
                uuid::Uuid::nil()
            ))
            .await;
        assert_eq!(names(&result), wanted);
        assert_eq!(
            names(
                &api.get(&format!("/Shows/{series_id}/Episodes?season=0"))
                    .await
            ),
            ["Z Special"]
        );
        assert_eq!(
            names(
                &api.get(&format!("/Shows/{series_id}/Episodes?season=2"))
                    .await
            ),
            ["Other"]
        );
        assert!(
            api.get(&format!("/Shows/{series_id}/Episodes?season=99"))
                .await["Items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let all = api.get(&format!("/Shows/{series_id}/Episodes")).await;
        assert_eq!(
            all["TotalRecordCount"], 4,
            "positioned specials are not duplicated"
        );
        let nil_adjacent = api
            .get(&format!(
                "/Shows/{series_id}/Episodes?season=1&adjacentTo={}",
                uuid::Uuid::nil()
            ))
            .await;
        assert_eq!(names(&nil_adjacent), wanted);
        assert_eq!(nil_adjacent["TotalRecordCount"], wanted.len());
        assert_eq!(
            names(&all),
            if enabled {
                vec!["One", "Z Special", "Two", "Other"]
            } else {
                vec!["Z Special", "One", "Two", "Other"]
            }
        );
        let page = api
            .get(&format!(
                "/Shows/{series_id}/Episodes?season=1&startIndex=1&limit=1"
            ))
            .await;
        assert_eq!(names(&page), [if enabled { "Z Special" } else { "Two" }]);
        assert_eq!(page["TotalRecordCount"], wanted.len());
        let next = api
            .get(&format!("/Shows/NextUp?userId={user}&seriesId={series_id}"))
            .await;
        assert_eq!(names(&next), [if enabled { "Z Special" } else { "Two" }]);
    }
    api.post("/Users/New", &json!({"Name":"specials-restricted"}))
        .await;
    let restricted_users = api.get("/Users").await;
    let restricted = restricted_users
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["Name"] == "specials-restricted")
        .unwrap();
    let restricted_id = restricted["Id"].as_str().unwrap();
    let mut restricted_policy = restricted["Policy"].clone();
    restricted_policy["EnableAllFolders"] = json!(true);
    restricted_policy["BlockedTags"] = json!(["hidden-special"]);
    api.post(
        &format!("/Users/{restricted_id}/Policy"),
        &restricted_policy,
    )
    .await;
    config["DisplaySpecialsWithinSeasons"] = json!(true);
    api.post("/System/Configuration", &config).await;
    for path in [
        format!("/Items?parentId={season_id}&userId={restricted_id}"),
        format!("/Shows/{series_id}/Episodes?seasonId={season_id}&userId={restricted_id}"),
    ] {
        let result = api.get(&path).await;
        assert_eq!(
            names(&result),
            ["One", "Two"],
            "hidden positioned specials stay hidden: {result}"
        );
        assert_eq!(result["TotalRecordCount"], 2);
    }
    config["DisplaySpecialsWithinSeasons"] = json!(false);
    api.post("/System/Configuration", &config).await;
    // Visibility applies to season resolution before its series episodes are read.
    api.post("/Users/New", &json!({"Name":"specials-hidden"}))
        .await;
    let users = api.get("/Users").await;
    let hidden = users
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["Name"] == "specials-hidden")
        .unwrap();
    let hidden_id = hidden["Id"].as_str().unwrap();
    let mut policy = hidden["Policy"].clone();
    policy["EnableAllFolders"] = json!(false);
    policy["EnabledFolders"] = json!([]);
    api.post(&format!("/Users/{hidden_id}/Policy"), &policy)
        .await;
    let response = api
        .client
        .get(format!(
            "{}/Shows/{series_id}/Episodes?seasonId={season_id}&userId={hidden_id}",
            api.base
        ))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let response = api
        .client
        .get(format!(
            "{}/Items?parentId={season_id}&userId={hidden_id}",
            api.base
        ))
        .header("Authorization", &api.auth)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let result: Value = response.json().await.unwrap();
    assert_eq!(
        names(&result),
        ["One", "Two"],
        "Items checks the season's own visibility, not standalone library access: {result}"
    );
    assert_eq!(result["TotalRecordCount"], 2);

    // A season is not ICollectionFolder: its own parent gate does not apply
    // EnabledFolders. An inherited/own parental-policy block does apply, even
    // when the request authenticates as admin and selects another user.
    let mut season = api.get(&format!("/Items/{season_id}")).await;
    season["Tags"] = json!(["hidden-season-parent"]);
    api.post(&format!("/Items/{season_id}"), &season).await;
    policy["EnableAllFolders"] = json!(true);
    policy["BlockedTags"] = json!(["hidden-season-parent"]);
    api.post(&format!("/Users/{hidden_id}/Policy"), &policy)
        .await;
    for (path, expected) in [
        (
            format!("/Items?parentId={season_id}&userId={hidden_id}"),
            reqwest::StatusCode::UNAUTHORIZED,
        ),
        (
            format!("/Shows/{series_id}/Episodes?seasonId={season_id}&userId={hidden_id}"),
            reqwest::StatusCode::NOT_FOUND,
        ),
    ] {
        let response = api
            .client
            .get(format!("{}{path}", api.base))
            .header("Authorization", &api.auth)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            expected,
            "selected-user own-Season parental block must apply: {path}"
        );
    }
    let result = api.get(&format!("/Items?parentId={season_id}")).await;
    assert_eq!(names(&result), ["One", "Two"]);
    assert_eq!(result["TotalRecordCount"], 2);
    policy["BlockedTags"] = json!([]);
    api.post(&format!("/Users/{hidden_id}/Policy"), &policy)
        .await;
    for path in [
        format!("/Items?parentId={season_id}&userId={hidden_id}"),
        format!("/Shows/{series_id}/Episodes?seasonId={season_id}&userId={hidden_id}"),
    ] {
        let result = api.get(&path).await;
        assert_eq!(names(&result), ["One", "Two"], "{path}: {result}");
        assert_eq!(result["TotalRecordCount"], 2);
    }
}

/// A fresh registered-task execution proves the resolver and item save finished.
async fn date_policy_scan(api: &Api) {
    let tasks = api.get("/ScheduledTasks").await;
    let task = tasks["Items"]
        .as_array()
        .or_else(|| tasks.as_array())
        .unwrap()
        .iter()
        .find(|task| task["Key"] == "RefreshLibrary")
        .unwrap();
    let id = task["Id"].as_str().unwrap();
    let previous = task["LastExecutionResult"]["EndTimeUtc"].clone();
    api.post(&format!("/ScheduledTasks/Running/{id}"), &Value::Null)
        .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let task = api.get(&format!("/ScheduledTasks/{id}")).await;
        let end = &task["LastExecutionResult"]["EndTimeUtc"];
        if task["State"] == "Idle" && end.is_string() && *end != previous {
            assert_eq!(task["LastExecutionResult"]["Status"], "Completed", "{task}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fresh date policy scan did not complete: {task}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn date_policy_ticks(item: &Value) -> i128 {
    let value =
        ferrofin_model::json::datetime::parse(item["DateCreated"].as_str().unwrap()).unwrap();
    i128::from(value.timestamp()) * 10_000_000 + i128::from(value.timestamp_subsec_nanos() / 100)
}

/// Re-stat after every mtime edit: Linux uses the source's min(ctime,mtime).
fn date_policy_creation_ticks(file: &std::path::Path) -> i128 {
    let stat = std::fs::metadata(file).unwrap();
    let fallback = || {
        use std::os::unix::fs::MetadataExt as _;
        let change = std::time::UNIX_EPOCH
            + Duration::new(
                u64::try_from(stat.ctime()).unwrap(),
                u32::try_from(stat.ctime_nsec()).unwrap(),
            );
        change.min(stat.modified().unwrap())
    };
    let creation = if cfg!(target_os = "linux") {
        fallback()
    } else {
        stat.created().unwrap_or_else(|_| fallback())
    };
    i128::try_from(
        creation
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            / 100,
    )
    .unwrap()
}

/// Live named configuration covers new resolver imports and changed/unchanged existing files.
#[allow(clippy::too_many_lines)]
async fn verify_date_added_policy(api: &Api, root: &std::path::Path) {
    let media = root.join("date-added");
    std::fs::create_dir_all(&media).unwrap();
    let mut metadata = api.get("/System/Configuration/metadata").await;
    metadata["UseFileCreationTimeForDateAdded"] = json!(false);
    api.post("/System/Configuration/metadata", &metadata).await;
    let options = json!({"PathInfos":[{"Path":media}],"EnableInternetProviders":false,
        "EnableRealtimeMonitor":false,"MetadataSavers":[],"EnableChapterImageExtraction":false,
        "EnableTrickplayImageExtraction":false,"TypeOptions":[{"Type":"Movie","MetadataFetchers":[],"ImageFetchers":[]}]});
    api.post(
        "/Library/VirtualFolders?name=Dates&collectionType=movies&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    let mut first = None;
    // Same server/library: false -> true -> false applies to newly resolved files.
    for (phase, creation) in [
        ("Import false", false),
        ("Import true", true),
        ("Import false again", false),
    ] {
        metadata["UseFileCreationTimeForDateAdded"] = json!(creation);
        api.post("/System/Configuration/metadata", &metadata).await;
        let folder = media.join(phase);
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("Dated.mkv");
        std::fs::write(&file, b"dated fixture").unwrap();
        let nfo = folder.join("Dated.nfo");
        std::fs::write(&nfo, format!("<movie><title>{phase}</title></movie>")).unwrap();
        let born = date_policy_creation_ticks(&file);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::SystemTime::now();
        date_policy_scan(api).await;
        let finished = std::time::SystemTime::now();
        let result = api
            .get("/Items?recursive=true&includeItemTypes=Movie&fields=DateCreated,Etag")
            .await;
        let item = result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["Name"] == phase)
            .unwrap_or_else(|| panic!("import phase did not persist: {result}"))
            .clone();
        let date = date_policy_ticks(&item);
        if creation {
            assert_eq!(date, born, "new creation-enabled import: {item}");
        } else {
            let lower = i128::try_from(
                started
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
                    / 100,
            )
            .unwrap();
            let upper = i128::try_from(
                finished
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
                    / 100,
            )
            .unwrap();
            assert!(
                date > born && date >= lower && date <= upper,
                "new scan-dated import: {item}, born {born}"
            );
        }
        if first.is_none() {
            first = Some((item, file, nfo));
        }
    }
    let (item, file, nfo) = first.unwrap();
    let id = item["Id"].as_str().unwrap();
    let path = format!("/Items/{id}?fields=DateCreated,Etag");
    // The metadata editor permits an exact .NET tick. Preserve all seven decimal
    // digits in unchanged/disabled refreshes; millisecond/microsecond checks hide loss.
    let exact = "2001-02-03T04:05:06.1234567Z";
    let mut edited = api.get(&path).await;
    let original_name = edited["Name"].clone();
    edited["DateCreated"] = json!(exact);
    api.post(&format!("/Items/{id}"), &edited).await;
    let saved = api.get(&path).await;
    assert_eq!(
        saved["Name"], original_name,
        "date edit must preserve the title"
    );
    let mut current_date = date_policy_ticks(&saved);
    assert_eq!(
        current_date,
        date_policy_ticks(&json!({"DateCreated":exact}))
    );
    for (index, (creation, changed)) in [
        (false, false),
        (true, false),
        (true, true),
        (false, false),
        (false, true),
    ]
    .into_iter()
    .enumerate()
    {
        metadata["UseFileCreationTimeForDateAdded"] = json!(creation);
        api.post("/System/Configuration/metadata", &metadata).await;
        assert_eq!(
            date_policy_ticks(&api.get(&path).await),
            current_date,
            "save alone must retain existing dates"
        );
        if changed {
            let offset = if creation { 3600 } else { 7200 };
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(offset))
                .unwrap();
        }
        let expected = if creation && changed {
            date_policy_creation_ticks(&file)
        } else {
            current_date
        };
        let phase = format!("Existing date phase {index}");
        std::fs::write(&nfo, format!("<movie><title>{phase}</title></movie>")).unwrap();
        let previous = api.get(&path).await["Etag"].clone();
        // FullRefresh without replacement only fills missing fields, so replace
        // metadata to make the fresh NFO title prove this refresh completed.
        api.post(&format!("/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"), &Value::Null).await;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let updated = api.get(&path).await;
            // A queued request or unrelated earlier save cannot satisfy the new
            // NFO title plus final row Etag. Name and DateCreated persist together.
            if updated["Name"] == phase
                && updated["Etag"].is_string()
                && updated["Etag"] != previous
            {
                assert_eq!(
                    date_policy_ticks(&updated),
                    expected,
                    "creation={creation}, changed={changed}: {updated}"
                );
                current_date = expected;
                break;
            }
            assert!(
                Instant::now() < deadline,
                "actual date refresh did not persist: {updated}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
