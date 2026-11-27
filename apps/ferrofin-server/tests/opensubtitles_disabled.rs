//! Disabling the OpenSubtitles plugin takes its subtitle provider out of the
//! library options' subtitle fetchers at once, and enabling it brings it back
//! (gl-2025): upstream's `PluginManager.LoadAssemblies` never loads a disabled
//! plugin's `ISubtitleProvider`.

use std::time::Duration;

use ferrofin_server::config::Config;
use serde_json::{Value, json};

#[path = "support/boot.rs"]
mod boot;

const CLIENT: &str = r#"MediaBrowser Client="opensubtitles-test", Device="fixture", DeviceId="opensubtitles-disabled", Version="1""#;

/// The OpenSubtitles plugin's id, as `GET /Plugins` lists it.
const PLUGIN: &str = "4a3f8e216c944d17a2b80f5e9c3d7a10";

async fn send(request: reqwest::RequestBuilder) -> reqwest::Response {
    let response = request.send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response
}

async fn subtitle_fetchers(client: &reqwest::Client, base: &str, auth: &str) -> Vec<String> {
    let options: Value = send(
        client
            .get(format!(
                "{base}/Libraries/AvailableOptions?libraryContentType=movies"
            ))
            .header("Authorization", auth),
    )
    .await
    .json()
    .await
    .unwrap();
    options["SubtitleFetchers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fetcher| fetcher["Name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_opensubtitles_is_no_subtitle_fetcher() {
    let tmp = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let (server, base, _) = boot::spawn_server(&client, |port| Config {
        port,
        ..Config::test_stub(tmp.path())
    })
    .await;
    let login: Value = send(
        client
            .post(format!("{base}/Users/AuthenticateByName"))
            .header("Authorization", CLIENT)
            .json(&json!({"Username": "admin", "Pw": ""})),
    )
    .await
    .json()
    .await
    .unwrap();
    let token = login["AccessToken"].as_str().unwrap();
    let auth = format!("{CLIENT}, Token=\"{token}\"");
    let toggle = |action: &str| {
        client
            .post(format!("{base}/Plugins/{PLUGIN}/1.0.0/{action}"))
            .header("Authorization", &auth)
    };

    assert_eq!(
        subtitle_fetchers(&client, &base, &auth).await,
        ["Open Subtitles"]
    );
    send(toggle("Disable")).await;
    assert_eq!(
        subtitle_fetchers(&client, &base, &auth).await,
        Vec::<String>::new()
    );
    send(toggle("Enable")).await;
    assert_eq!(
        subtitle_fetchers(&client, &base, &auth).await,
        ["Open Subtitles"]
    );

    send(
        client
            .post(format!("{base}/System/Shutdown"))
            .header("Authorization", &auth),
    )
    .await;
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
