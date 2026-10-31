//! Path spelling, end to end: Jellyfin's routes are case-insensitive (ASP.NET)
//! and axum's are not, so the server re-cases a path before routing it. Over real
//! HTTP, every contract route answers the same however it is spelled, and a
//! `/web/*` asset, whose case is the file's, is left alone.

use std::time::Duration;

use ferrofin_api::VENDORED_ROUTES;
use ferrofin_server::config::Config;
use reqwest::StatusCode;

#[path = "support/boot.rs"]
mod boot;

/// Ceiling for one request, only to turn a hang into a failure.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// `path` with every `{placeholder}` replaced by a value.
fn filled(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some((head, tail)) = rest.split_once('{') {
        out.push_str(head);
        out.push_str("00000000000000000000000000000001");
        rest = tail.split_once('}').map_or("", |(_, after)| after);
    }
    out.push_str(rest);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_answers_the_same_in_any_spelling_and_an_asset_keeps_its_case() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let web = tmp.path().join("web");
    std::fs::create_dir_all(web.join("Assets")).expect("web dir");
    std::fs::write(web.join("index.html"), "<html></html>").expect("index");
    std::fs::write(web.join("Assets").join("Logo.PNG"), "logo").expect("asset");

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("client");
    let (_server, base, _config) = boot::spawn_server(&client, |port| Config {
        server_name: "ferrofin-path-case".to_owned(),
        port,
        web_dir: web.clone(),
        ..Config::test_stub(tmp.path())
    })
    .await;
    let status = |path: String| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            client
                .get(format!("{base}{path}"))
                .send()
                .await
                .unwrap_or_else(|error| panic!("{path}: {error}"))
                .status()
        }
    };

    // GET is what can be repeated without changing what the next spelling sees.
    // Sent anonymously, so most answer 401 in every spelling, which still tells
    // a route that was found from a spelling that fell through to a 404.
    for (_, path) in VENDORED_ROUTES
        .iter()
        .filter(|(method, _)| *method == "get")
    {
        let path = filled(path);
        let canonical = status(path.clone()).await;
        for spelling in [
            path.to_ascii_lowercase(),
            path.to_ascii_uppercase(),
            format!("{path}/"),
        ] {
            assert_eq!(status(spelling.clone()).await, canonical, "{spelling}");
        }
    }

    // The asset's own spelling is the only one that finds it.
    let asset = client
        .get(format!("{base}/web/Assets/Logo.PNG"))
        .send()
        .await
        .expect("asset");
    assert_eq!(asset.status(), StatusCode::OK);
    assert_eq!(asset.text().await.expect("asset body"), "logo");
    assert_eq!(
        status("/web/assets/logo.png".to_owned()).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(status("/web/".to_owned()).await, StatusCode::OK);
}
