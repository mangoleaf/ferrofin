//! The Ferrofin settings page in jellyfin-web's dashboard, through the
//! composition root and the real router (`tower` oneshot).
//!
//! jellyfin-web shows a plugin's Settings link when `GET /web/ConfigurationPages`
//! lists a page tagged with the plugin's id, adds a dashboard menu entry for a
//! page listed with `EnableInMainMenu`, and loads either from
//! `GET /web/ConfigurationPage?name=…`, which is anonymous. So the page must
//! be listed both ways, carry no setting, and disappear with the plugin.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use ferrofin_db::Database;
use ferrofin_extensions::ferrofin_settings::EXTENSION_ID;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use tower::ServiceExt as _;

const ADMIN_USER: &str = "admin";
const ADMIN_PASSWORD: &str = "settings-page-pw";

const CLIENT_AUTH: &str = "MediaBrowser Client=\"settings-page-test\", Device=\"test\", \
                           DeviceId=\"settings-page-test-1\", Version=\"1.0\"";

async fn body_bytes(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec()
}

async fn body_json(response: Response) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(response).await).expect("json body")
}

/// Whether `pages` (a `GET /web/ConfigurationPages` answer) holds the
/// Ferrofin settings page, tagged with the plugin's id and labelled with its
/// name, as jellyfin-web matches and shows it.
fn lists_the_settings_page(pages: &serde_json::Value) -> bool {
    pages.as_array().expect("page list").iter().any(|page| {
        page["Name"] == "Ferrofin"
            && page["DisplayName"] == "Ferrofin"
            && page["PluginId"].as_str().map(|id| id.replace('-', ""))
                == Some(EXTENSION_ID.simple().to_string())
    })
}

#[tokio::test]
// One dashboard visit, then the same visit with the plugin disabled; split,
// it would only boot the server twice.
#[allow(clippy::too_many_lines)]
async fn the_settings_page_is_listed_served_without_values_and_hidden_with_its_plugin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = Config {
        server_name: "ferrofin-settings-page-test".to_owned(),
        admin_user: ADMIN_USER.to_owned(),
        admin_password: ADMIN_PASSWORD.to_owned(),
        ..Config::test_stub(temp.path())
    };
    for dir in [&config.config_dir, &config.data_dir, &config.cache_dir] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    ferrofin_server::seed::seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .expect("seed admin");
    let router = ferrofin_api::create_router(wired.state.clone());

    let auth = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header(header::AUTHORIZATION, CLIENT_AUTH)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "Username": ADMIN_USER, "Pw": ADMIN_PASSWORD }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("authenticate");
    assert_eq!(auth.status(), StatusCode::OK);
    let token = body_json(auth).await["AccessToken"]
        .as_str()
        .expect("access token")
        .to_owned();
    let admin = format!("{CLIENT_AUTH}, Token=\"{token}\"");
    let send = |method: &str, uri: &str, authorization: Option<&str>| {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(authorization) = authorization {
            request = request.header(header::AUTHORIZATION, authorization);
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).expect("request"))
    };

    // The plugin list's Settings link and the dashboard menu's entry.
    for uri in [
        "/web/ConfigurationPages",
        "/web/ConfigurationPages?enableInMainMenu=true",
    ] {
        let pages = send("GET", uri, Some(&admin)).await.expect(uri);
        assert_eq!(pages.status(), StatusCode::OK, "{uri}");
        let pages = body_json(pages).await;
        assert!(lists_the_settings_page(&pages), "{uri}: {pages}");
    }

    // The page itself, fetched as jellyfin-web does, without a token: HTML
    // with none of the server's settings in it.
    let page = send("GET", "/web/configurationpage?name=Ferrofin", None)
        .await
        .expect("page");
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        page.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let html = String::from_utf8(body_bytes(page).await).expect("UTF-8 page");
    assert!(html.contains(r#"<div data-role="page""#), "{html}");
    for value in [
        config.data_dir.display().to_string(),
        config.config_file.display().to_string(),
        ADMIN_PASSWORD.to_owned(),
    ] {
        assert!(!html.contains(&value), "the page holds {value:?}");
    }

    // What the page renders.
    let settings = send("GET", "/Ferrofin/Settings", Some(&admin))
        .await
        .expect("settings");
    assert_eq!(settings.status(), StatusCode::OK);
    let settings = body_json(settings).await;
    assert!(
        !settings["Settings"]
            .as_array()
            .expect("settings list")
            .is_empty(),
        "{settings}"
    );

    // Disabled from the dashboard, the link, the menu entry, the page and
    // its data are gone.
    let plugins = body_json(
        send("GET", "/Plugins", Some(&admin))
            .await
            .expect("plugins"),
    )
    .await;
    let version = plugins
        .as_array()
        .expect("plugin list")
        .iter()
        .find(|plugin| plugin["Name"] == "Ferrofin")
        .and_then(|plugin| plugin["Version"].as_str())
        .unwrap_or_else(|| panic!("Ferrofin missing from /Plugins: {plugins}"))
        .to_owned();
    let disable = send(
        "POST",
        &format!("/Plugins/{EXTENSION_ID}/{version}/Disable"),
        Some(&admin),
    )
    .await
    .expect("disable");
    assert_eq!(disable.status(), StatusCode::NO_CONTENT);
    for uri in [
        "/web/ConfigurationPages",
        "/web/ConfigurationPages?enableInMainMenu=true",
    ] {
        let pages = body_json(send("GET", uri, Some(&admin)).await.expect(uri)).await;
        assert!(!lists_the_settings_page(&pages), "{uri}: {pages}");
    }
    let page = send("GET", "/web/configurationpage?name=Ferrofin", None)
        .await
        .expect("page");
    assert_eq!(page.status(), StatusCode::NOT_FOUND);
    let settings = send("GET", "/Ferrofin/Settings", Some(&admin))
        .await
        .expect("settings");
    assert_eq!(settings.status(), StatusCode::NOT_FOUND);
}
