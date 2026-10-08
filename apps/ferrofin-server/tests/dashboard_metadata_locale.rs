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
    let observed = requests.lock().unwrap().clone();
    for language in ["fr", "de", "es-AR"] {
        assert!(
            observed.iter().any(|uri| uri.starts_with("/movie/603?")
                && uri.contains(&format!("language={language}"))),
            "{observed:?}"
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
    verify_movie_image_options(&api, library, &mut options, id).await;
    verify_nfo_savers(&api, library, &mut options, id, &media).await;
    verify_artwork_destinations(&api, library, &mut options, id, &media).await;
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
    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    mock.abort();
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
            .any(|uri| uri.starts_with("/subtitles?") && uri.contains("moviehash_match=only"))
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
            .any(|uri| uri.starts_with("/subtitles?") && !uri.contains("moviehash_match=only"))
    );

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
