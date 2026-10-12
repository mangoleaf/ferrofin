//! Library music tag options take effect on the next real HTTP refresh.

use std::time::{Duration, Instant};

use axum::{Json, Router};
use ferrofin_server::config::{Config, ProviderEndpoints};
use serde_json::{Value, json};

const CLIENT: &str =
    r#"MediaBrowser Client="music-tag-test", Device="fixture", DeviceId="music-tags", Version="1""#;

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
        assert!(
            response.status().is_success(),
            "{path}: {}: {}",
            response.status(),
            response.text().await.unwrap()
        );
    }

    async fn audio(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let items = self
                .get("/Items?recursive=true&includeItemTypes=Audio&fields=Genres,ProviderIds,Etag")
                .await;
            if let Some(item) = items["Items"].as_array().unwrap().first() {
                return item.clone();
            }
            assert!(Instant::now() < deadline, "no scanned audio: {items}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn await_tags(
        &self,
        id: &str,
        artists: &[&str],
        album_artists: &[&str],
        genres: &[&str],
    ) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let item = self
                .get(&format!("/Items/{id}?Fields=Genres,ProviderIds,Etag"))
                .await;
            let album_names: Vec<_> = item["AlbumArtists"]
                .as_array()
                .unwrap()
                .iter()
                .map(|artist| artist["Name"].as_str().unwrap())
                .collect();
            if item["Artists"] == json!(artists)
                && album_names == album_artists
                && item["Genres"] == json!(genres)
            {
                return item;
            }
            assert!(
                Instant::now() < deadline,
                "expected {artists:?}/{album_artists:?}/{genres:?}: {item}"
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
{{"streams":[{{"index":0,"codec_type":"audio","codec_name":"flac","sample_rate":"44100","channels":1}}],"format":{{"format_name":"flac","duration":"1","size":"4096","bit_rate":"12800","tags":{{"title":"Metadata song","album":"Metadata album","artist":"Standard / Partner","ARTISTS":"AC/DC; Guest","album_artist":"Album / Collective","ALBUMARTISTS":"AC/DC; Album guest","genre":"Rock; Metal","musicbrainz_albumid":"11111111-1111-4111-8111-111111111111;22222222-2222-4222-8222-222222222222"}}}}}}
EOF
fi
"#
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_artist_and_delimiter_options_change_http_music_metadata_without_restarting() {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("music");
    let album = media.join("Tagged/Album");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::write(album.join("01 - Track.flac"), b"audio fixture").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(|| async {
            Json(json!({"artists":[],"releases":[],"release-groups":[],"results":[],"data":{"token":"fixture"}}))
        })).await.unwrap();
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
        musicbrainz_base_url: endpoint.clone(),
        studios_repo_url: endpoint.clone(),
        provider_endpoints: ProviderEndpoints {
            tmdb: Some(endpoint.clone()),
            tmdb_images: Some(endpoint.clone()),
            tvdb: Some(endpoint.clone()),
            omdb: Some(endpoint.clone()),
            fanart: Some(endpoint.clone()),
            audiodb: Some(endpoint.clone()),
            lrclib: Some(endpoint.clone()),
            opensubtitles: Some(endpoint),
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
        client: reqwest::Client::new(),
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
            .is_ok_and(|response| response.status().is_success())
        {
            break;
        }
        assert!(Instant::now() < deadline, "server did not start");
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
    let mut options = json!({
        "PathInfos":[{"Path":media}], "EnableRealtimeMonitor":false,
        "EnableInternetProviders":false, "SaveLocalMetadata":false, "MetadataSavers":[],
        "EnableChapterImageExtraction":false, "EnableTrickplayImageExtraction":false,
        "EnableLUFSScan":false, "PreferNonstandardArtistsTag":false, "UseCustomTagDelimiters":false,
        "TypeOptions":[
            {"Type":"MusicArtist","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"MusicAlbum","MetadataFetchers":[],"ImageFetchers":[]},
            {"Type":"Audio","MetadataFetchers":[],"ImageFetchers":[]}
        ]
    });
    api.post(
        "/Library/VirtualFolders?name=Music&collectionType=music&refreshLibrary=false",
        &json!({"LibraryOptions":options}),
    )
    .await;
    let folders = api.get("/Library/VirtualFolders").await;
    let library = &folders.as_array().unwrap()[0]["ItemId"];
    api.post("/Library/Refresh", &Value::Null).await;
    let song = api.audio().await;
    let id = song["Id"].as_str().unwrap();
    let first = api
        .await_tags(
            id,
            &["Standard / Partner"],
            &["Album / Collective"],
            &["Rock; Metal"],
        )
        .await;
    assert!(
        first["ProviderIds"].get("MusicBrainzAlbum").is_none(),
        "unsplit composite MBID is invalid: {first}"
    );
    let refresh = format!(
        "/Items/{id}/Refresh?metadataRefreshMode=FullRefresh&imageRefreshMode=None&replaceAllMetadata=true"
    );
    for (prefer, custom, delimiters, whitelist, artists, album_artists, genres) in [
        (
            true,
            false,
            vec![";", "/"],
            vec![],
            vec!["AC/DC; Guest"],
            vec!["AC/DC; Album guest"],
            vec!["Rock; Metal"],
        ),
        (
            true,
            true,
            vec![";", "/"],
            vec!["ac/dc"],
            vec!["ac/dc", "Guest"],
            vec!["ac/dc", "Album guest"],
            vec!["Rock", "Metal"],
        ),
        (
            true,
            true,
            vec![";", "/"],
            vec![],
            vec!["AC", "DC", "Guest"],
            vec!["AC", "DC", "Album guest"],
            vec!["Rock", "Metal"],
        ),
        (
            true,
            true,
            vec![";;"],
            vec![],
            vec!["AC/DC; Guest"],
            vec!["AC/DC; Album guest"],
            vec!["Rock; Metal"],
        ),
        (
            false,
            true,
            vec!["/"],
            vec![],
            vec!["Standard", "Partner"],
            vec!["Album", "Collective"],
            vec!["Rock; Metal"],
        ),
        (
            false,
            false,
            vec![";", "/"],
            vec![],
            vec!["Standard / Partner"],
            vec!["Album / Collective"],
            vec!["Rock; Metal"],
        ),
    ] {
        options["PreferNonstandardArtistsTag"] = json!(prefer);
        options["UseCustomTagDelimiters"] = json!(custom);
        options["CustomTagDelimiters"] = json!(delimiters);
        options["DelimiterWhitelist"] = json!(whitelist);
        api.post(
            "/Library/VirtualFolders/LibraryOptions",
            &json!({"Id":library,"LibraryOptions":options}),
        )
        .await;
        api.post(&refresh, &Value::Null).await;
        let song = api.await_tags(id, &artists, &album_artists, &genres).await;
        if custom && delimiters.contains(&";") {
            assert_eq!(
                song["ProviderIds"]["MusicBrainzAlbum"], "11111111-1111-4111-8111-111111111111",
                "{song}"
            );
        }
    }
    api.post("/System/Shutdown", &Value::Null).await;
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    mock.abort();
}
