//! The server the Single Sign-On tests sign in to: wired by the composition
//! root over a fresh database, with the plugin enabled (or booted again over
//! the database an earlier boot left, with a configuration of its own), and
//! the requests a browser and its completion page make, through the real
//! router (`tower` oneshot).

#![allow(dead_code)] // Each test binary uses its own subset.

use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, header};
use axum::response::Response;
use ferrofin_api::state::AppState;
use ferrofin_db::Database;
use ferrofin_extensions::sso::EXTENSION_ID;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;
use tower::ServiceExt as _;

/// The server: its router and state, and the shutdown channel's receiver,
/// held while the router serves.
pub struct SsoServer {
    /// The real router over [`state`](Self::state).
    pub router: axum::Router,
    /// The composition root's state.
    pub state: AppState,
    _shutdown: tokio::sync::oneshot::Receiver<()>,
}

/// The server over a fresh database in `temp`, published at `published_url`,
/// with the Single Sign-On plugin enabled and no provider.
pub async fn server(temp: &tempfile::TempDir, published_url: &str) -> SsoServer {
    let server = boot(&config(temp, published_url)).await;
    // The admin's step, through the manager: `sso_settings` covers the route.
    server
        .state
        .plugins
        .enable_plugin(EXTENSION_ID)
        .await
        .expect("enable");
    server
}

/// The configuration of a server over `temp`, published at `published_url`.
pub fn config(temp: &tempfile::TempDir, published_url: &str) -> Config {
    Config {
        server_name: "sso-test".to_owned(),
        published_url: Some(published_url.to_owned()),
        ..Config::test_stub(temp.path())
    }
}

/// The server `config` describes, over its database as it is, booted as the
/// composition root wires it.
pub async fn boot(config: &Config) -> SsoServer {
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
    let (shutdown_tx, shutdown) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");
    SsoServer {
        router: ferrofin_api::create_router(wired.state.clone()),
        state: wired.state,
        _shutdown: shutdown,
    }
}

impl SsoServer {
    /// Sends `request` through the router.
    pub async fn send(&self, request: Request<Body>) -> Response {
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("response")
    }

    /// `GET uri`, with `cookie` as the `Cookie` header when given.
    pub async fn get(&self, uri: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::empty()).expect("request"))
            .await
    }

    /// What the completion page's script posts: the exchange `code`, with
    /// `cookie` (`name=value`) and the device `device_id`, from `peer` (the
    /// loopback when `None`).
    pub async fn exchange(
        &self,
        code: &str,
        cookie: Option<&str>,
        device_id: &str,
        peer: Option<IpAddr>,
    ) -> Response {
        let mut request = Request::builder()
            .method("POST")
            .uri("/sso/oidc/exchange")
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::AUTHORIZATION,
                format!(
                    r#"MediaBrowser Client="Jellyfin%20Web", Device="Web%20Browser", DeviceId="{device_id}", Version="12.4.0""#
                ),
            );
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(peer) = peer {
            request = request.extension(ConnectInfo(SocketAddr::new(peer, 50_000)));
        }
        self.send(
            request
                .body(Body::from(serde_json::json!({ "Code": code }).to_string()))
                .expect("request"),
        )
        .await
    }
}

/// `response`'s header `name`, as text.
pub fn header_text(response: &Response, name: header::HeaderName) -> String {
    response.headers()[name].to_str().expect("text").to_owned()
}

/// `response`'s body, as text.
pub async fn text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The exchange code a completion page holds.
pub fn exchange_code(html: &str) -> String {
    html.split_once("data-code=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(
            || panic!("no exchange code in {html}"),
            |(code, _)| code.to_owned(),
        )
}
