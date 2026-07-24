//! Hermit server — the composition root (port of `Jellyfin.Server`).
//!
//! Loads bootstrap [`config`], opens + migrates the SQLite DB, discovers ffmpeg,
//! constructs the concrete `hermit-core` managers and injects them as the
//! `Arc<dyn Trait>` fields of `hermit-api`'s `AppState`, seeds a default
//! administrator on a fresh install, mounts the router, and serves with graceful
//! shutdown.
//!
//! The bring-up is split into small, independently-callable pieces so the binary
//! (`main.rs`) and the First-Light integration test can each sequence them:
//!
//! - [`config`] — bootstrap configuration resolution (CLI > env > file > default).
//! - [`bootstrap`] — startup side-effects (logging, database, ffmpeg discovery).
//! - [`state::build_app_state`] — the manager-wiring composition root.
//! - [`seed::seed_default_admin`] — fresh-install administrator seeding.
//! - [`media_encoding`] — the ffmpeg-backed transcode/HLS + attachment pair.
//! - [`run`] — the end-to-end boot-and-serve entry point the binary calls.
//!
//! Port bootstrap semantics from `Jellyfin.Server`'s `Program.Main` + `Startup`.
//! See `brain/PLAN_HERMIT_PORT.md`.

pub mod bootstrap;
pub mod config;
pub mod media_encoding;
pub mod planner;
pub mod seed;
pub mod state;

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context as _;
use axum::Router;
use axum::response::Redirect;
use axum::routing::get;
use tower::Layer as _;
use tower_http::services::ServeDir;

use crate::bootstrap::{FfmpegPaths, discover_ffmpeg, init_tracing, open_database};
use crate::config::Config;
use crate::seed::{SeedOutcome, seed_default_admin};
use crate::state::build_app_state;

/// Boots the server from a resolved [`Config`] and serves until shutdown.
///
/// This is the whole composition root, in order: initialise logging, open +
/// migrate the database, discover ffmpeg (non-fatal — the API still boots without
/// it, playback just 500s until configured), wire every concrete manager into the
/// shared `AppState`, seed a default administrator when the install is fresh, mount
/// the `hermit-api` router, and `axum::serve` on the configured bind address with
/// graceful shutdown.
///
/// The binary calls this after parsing CLI flags; it is the single entry point so
/// the boot sequence has exactly one implementation.
///
/// # Errors
///
/// Returns an error if the database cannot be opened/migrated, manager wiring
/// fails, seeding fails, the listener cannot bind the configured address, or the
/// server loop errors.
pub async fn run(config: Config) -> anyhow::Result<()> {
    init_tracing(&config);
    tracing::info!(
        server_name = %config.server_name,
        data_dir = %config.data_dir.display(),
        config_dir = %config.config_dir.display(),
        cache_dir = %config.cache_dir.display(),
        web_dir = %config.web_dir.display(),
        bind = %config.bind_addr,
        port = config.port,
        https_port = config.https_port,
        base_url = %config.base_url,
        published_url = config.published_url.as_deref().unwrap_or("<auto>"),
        library_roots = config.library_roots.len(),
        admin_user = %config.admin_user,
        admin_password_set = !config.admin_password.is_empty(),
        version = env!("CARGO_PKG_VERSION"),
        "hermit-server starting"
    );

    let db = open_database(&config).await?;

    // ffmpeg is required for playback but not for the server to boot: warn and
    // continue so the API comes up even on a host without ffmpeg installed.
    // `system.json` EncoderAppPath is not yet loaded at this bootstrap stage, so
    // no persisted fallback is supplied here. When discovery fails, wire the
    // encoder with bare `ffmpeg`/`ffprobe` names so playback 500s (rather than
    // failing to boot) until a working ffmpeg is configured.
    let ffmpeg = match discover_ffmpeg(&config, None).await {
        Ok(paths) => {
            tracing::info!(
                ffmpeg = %paths.ffmpeg.display(),
                ffprobe = %paths.ffprobe.display(),
                "ffmpeg discovered",
            );
            paths
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ffmpeg unavailable — transcoding/playback will be disabled until configured",
            );
            FfmpegPaths {
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
            }
        }
    };

    // Wire every concrete manager into the shared AppState (the composition root).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, shutdown_tx)
        .await
        .context("failed to assemble application state")?;

    // Fresh-install seeding: on a database with no users, create the configured
    // default administrator (port of `UserManager.InitializeAsync` +
    // startup-wizard `UpdateStartupUser`). A no-op once any user exists.
    match seed_default_admin(wired.state.users.as_ref(), &config)
        .await
        .context("failed to seed the default administrator")?
    {
        SeedOutcome::AlreadyInitialized => {
            tracing::info!("existing users found — skipping fresh-install seeding");
        }
        SeedOutcome::SeededWithConfiguredPassword { username } => {
            tracing::info!(
                %username,
                "seeded default administrator with the configured password"
            );
        }
        SeedOutcome::SeededPasswordless { username } => {
            // Matches Jellyfin's fresh-install default: the admin has no password
            // yet. Complete setup in the browser (the web wizard sets it) or set
            // HERMIT_ADMIN_PASSWORD for a headless, ready-to-use admin.
            tracing::warn!(
                %username,
                "seeded a PASSWORDLESS default administrator — complete setup via the web \
                 wizard at /web, or set HERMIT_ADMIN_PASSWORD for a headless install"
            );
        }
    }

    let router = mount_web(
        hermit_api::create_router(wired.state.clone()),
        &config.web_dir,
    );

    // Post-startup: flip the host's core-startup flag (mirrors `CoreAppHost`
    // marking itself ready once services are registered).
    wired.app_host.mark_core_startup_complete();

    // Case-insensitive routing: Jellyfin's API is case-insensitive but axum's
    // router is not, and clients (jellyfin-web included) call some paths in
    // non-canonical case. Rewrite each request's path to its registered case
    // BEFORE routing. This must wrap the whole router as an outer layer (not
    // `Router::layer`, which runs per-matched-route, too late to re-route).
    let app = axum::middleware::from_fn(canonicalize_path_case).layer(router);

    let addr = SocketAddr::new(config.bind_addr, config.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, "hermit-server listening");

    axum::serve(
        listener,
        axum::ServiceExt::<axum::extract::Request>::into_make_service(app),
    )
    .with_graceful_shutdown(async move {
        shutdown_rx.await.ok();
        tracing::info!("graceful shutdown requested");
    })
    .await
    .context("server error")?;

    tracing::info!("hermit-server stopped");
    Ok(())
}

/// Rewrites a request's path to its canonical Jellyfin case before routing.
///
/// Jellyfin's API is case-insensitive (ASP.NET); axum's router is case-sensitive,
/// and clients call some paths in non-canonical case (e.g. `/Localization/countries`).
/// [`hermit_api::routes::canonicalize_path`] re-cases route literals to the
/// registered form while preserving parameter values, returning `None` for paths
/// that match no API route (`/web/*` assets, health) so their case stays significant.
/// Applied as an OUTER layer so `next.run` re-enters routing with the rewritten path.
async fn canonicalize_path_case(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(canonical) = hermit_api::routes::canonicalize_path(request.uri().path()) {
        let query = request
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        if let Ok(uri) = format!("{canonical}{query}").parse() {
            *request.uri_mut() = uri;
        }
    }
    next.run(request).await
}

/// Mounts a static web client at `/web` (with an SPA `index.html` fallback) and
/// redirects `/` → `/web/`, when `web_dir` contains an `index.html`.
///
/// Hermit serves whatever static bundle the operator places in `web_dir` — e.g.
/// a built [`jellyfin-web`](https://github.com/jellyfin/jellyfin-web) `dist/`.
/// The web client talks to Hermit over the same-origin HTTP API, exactly as it
/// would against upstream Jellyfin. If the directory has no `index.html` the
/// server runs API-only and `/` returns `404` (the API is unaffected either way,
/// since no contract route lives under `/web` or at `/`).
fn mount_web(router: Router, web_dir: &Path) -> Router {
    let index = web_dir.join("index.html");
    if !index.is_file() {
        tracing::info!(
            web_dir = %web_dir.display(),
            "no web client bundle (index.html) found — serving API only"
        );
        return router;
    }
    tracing::info!(web_dir = %web_dir.display(), "serving static web client at /web");
    // NOTE: no SPA/history fallback. jellyfin-web uses hash routing (`/web/#!/…`)
    // and lazy webpack chunks, so a missing file MUST return 404 — falling back to
    // `index.html` (text/html) would feed HTML to a chunk load and crash the app
    // (black screen). `ServeDir` serves `index.html` for the `/web/` directory
    // request on its own (append-index-on-directories is on by default).
    // `nest_service("/web", …)` serves `/web` and `/web/*` (stripping the prefix);
    // a bare `/web` request serves `index.html`. We cannot also register a
    // `route("/web", …)` redirect — axum rejects the duplicate `/web` and panics.
    // The `/` → `/web/` redirect is enough for the normal entry point (the client's
    // relative asset URLs only resolve correctly from the trailing-slash `/web/`).
    router
        .nest_service("/web", ServeDir::new(web_dir))
        .route("/", get(|| async { Redirect::permanent("/web/") }))
}

#[cfg(test)]
mod tests {
    use super::mount_web;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn serves_web_bundle_and_redirects_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>hermit").unwrap();

        let app = mount_web(Router::new(), dir.path());

        // `/` redirects to the web client.
        let root = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(root.headers()["location"], "/web/");

        // `/web/` serves the bundle's index.html.
        let web = app
            .oneshot(Request::builder().uri("/web/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(web.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_only_when_no_bundle() {
        let dir = tempfile::tempdir().unwrap();
        // No index.html written → router is returned unchanged (no `/` route).
        let app = mount_web(Router::new(), dir.path());
        let root = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::NOT_FOUND);
    }
}
