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
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tower::Layer as _;
use tower_http::services::ServeDir;

use crate::bootstrap::{FfmpegPaths, discover_ffmpeg, init_tracing, open_database};
use crate::config::Config;
use crate::seed::{SeedOutcome, seed_default_admin};
use crate::state::build_app_state;

/// The version Hermit reports for itself (startup log line and the session
/// app-version fallback in the authorization context).
///
/// Prefers the `SERVICE_VERSION` environment variable — stamped into the release
/// image from the git tag by CI. When unset (local/dev builds), it falls back to
/// `HERMIT_BUILD_VERSION`, a `git describe` value baked in at compile time by
/// `build.rs` (latest tag + commits-since + HEAD sha), so the reported version is
/// derived from git rather than a hardcoded number that goes stale.
pub(crate) fn service_version() -> String {
    std::env::var("SERVICE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env!("HERMIT_BUILD_VERSION").to_owned())
}

/// Logs the resolved configuration at startup, grouped into a few `INFO` lines
/// (server/version, paths, network, library & admin).
fn log_startup_banner(config: &Config) {
    tracing::info!(
        server_name = %config.server_name,
        version = service_version(),
        "hermit-server starting"
    );
    tracing::info!(
        data_dir = %config.data_dir.display(),
        config_dir = %config.config_dir.display(),
        cache_dir = %config.cache_dir.display(),
        web_dir = %config.web_dir.display(),
        "paths"
    );
    tracing::info!(
        bind = %config.bind_addr,
        port = config.port,
        https_port = config.https_port,
        base_url = %config.base_url,
        published_url = config.published_url.as_deref().unwrap_or("<auto>"),
        "network"
    );
    tracing::info!(
        library_roots = config.library_roots.len(),
        admin_user = %config.admin_user,
        admin_password_set = !config.admin_password.is_empty(),
        "library & admin"
    );
}

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
    log_startup_banner(&config);

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
                filters: Vec::new(),
                encoders: Vec::new(),
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
        Arc::clone(&wired.file_transformations),
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
        // `with_connect_info` so handlers can read the client's socket address
        // (e.g. `GET /System/Endpoint` reporting `IsLocal` for a loopback peer).
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<SocketAddr>(
            app,
        ),
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
    // Path: re-case to the registered route where recognized (asset/unknown paths
    // return `None` and keep their significant case).
    let path = request.uri().path().to_owned();
    let new_path = hermit_api::routes::canonicalize_path(&path).unwrap_or_else(|| path.clone());
    // Query keys: case-fold unconditionally. Jellyfin's API is case-insensitive,
    // but our `Query<T>` structs are `rename_all = "camelCase"`. Clients send a mix —
    // the SDK emits PascalCase (`ParentId`, `IncludeItemTypes`), legacy jQuery paths
    // camelCase. PascalCase and camelCase differ only in the first character, so
    // lowercasing each key's first char maps both onto the camelCase field names
    // (values untouched). Without this, SDK-cased filters bind to `None` and silently
    // drop — e.g. a library's own CollectionFolder leaking into an
    // `IncludeItemTypes=Movie` grid query. Applied to every request (harmless for
    // asset routes, which ignore the query) so it also covers extra, non-contract
    // routes like `/Users/{userId}/Items` that `canonicalize_path` doesn't know.
    let target = match request.uri().query() {
        Some(q) => format!("{new_path}?{}", normalize_query_keys(q)),
        None => new_path,
    };
    // Origin-form request URI (path + query, no scheme/authority) → rebuilding from
    // the parts is lossless; the rewrite is idempotent when nothing changed.
    if let Ok(uri) = target.parse() {
        *request.uri_mut() = uri;
    }
    next.run(request).await
}

/// Lowercases the first character of each `&`-separated query param's key,
/// preserving values verbatim. Idempotent for already-camelCase keys.
fn normalize_query_keys(query: &str) -> String {
    query
        .split('&')
        .map(|pair| {
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (pair, None),
            };
            let mut chars = key.chars();
            let key = match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_lowercase(), chars.as_str()),
                None => String::new(),
            };
            match value {
                Some(v) => format!("{key}={v}"),
                None => key,
            }
        })
        .collect::<Vec<_>>()
        .join("&")
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
fn mount_web(
    router: Router,
    web_dir: &Path,
    transformations: Arc<dyn hermit_traits::plugins::FileTransformationService>,
) -> Router {
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
    //
    // `index.html` references its assets **relatively** (`src="runtime.bundle.js"`),
    // so they only resolve when the document is loaded from the trailing-slash
    // `/web/` (base `…/web/`); loaded from a bare `/web`, the browser resolves them
    // against the server root and every asset 404s. `nest_service("/web", …)` serves
    // both `/web` and `/web/*`, so a bare `/web` would wrongly serve `index.html`
    // in-place. We can't add a `route("/web", …)` redirect (axum rejects the
    // duplicate `/web` and panics), so a thin middleware layer redirects the exact
    // `/web` path to `/web/` **before** it reaches the nested `ServeDir`.
    // The File Transformation pipeline runs as a layer in front of `ServeDir`:
    // a matching file is read, transformed, and served directly; everything
    // else falls through to the plain static serving. The pipeline self-gates
    // on the File Transformation plugin's enabled flag, so with it disabled
    // every request takes the `needs_transformation → false` fast path.
    let web_root = web_dir.to_path_buf();
    let transform_layer = axum::middleware::from_fn(move |req: Request, next: Next| {
        let transformations = Arc::clone(&transformations);
        let web_root = web_root.clone();
        async move { transform_web_file(&transformations, &web_root, req, next).await }
    });
    router
        .nest_service("/web", ServeDir::new(web_dir))
        .route("/", get(|| async { Redirect::permanent("/web/") }))
        .layer(axum::middleware::from_fn(redirect_bare_web))
        .layer(transform_layer)
}

/// Serves a `/web` file through the File Transformation pipeline when a
/// registered transformation matches it; all other requests pass through to
/// the static `ServeDir` untouched.
///
/// Port of the File Transformation plugin's static-file middleware: it reads
/// the matched file, runs the pipeline over its text, and responds with the
/// transformed contents (binary or unreadable files fall through untouched).
async fn transform_web_file(
    transformations: &Arc<dyn hermit_traits::plugins::FileTransformationService>,
    web_root: &Path,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if req.method() == axum::http::Method::GET
        && let Some(rel) = path.strip_prefix("/web/").filter(|r| !r.is_empty())
        // Reject any path that could escape the web root (`..`, absolute).
        && !Path::new(rel).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
        && transformations.needs_transformation(rel).await
        && let Ok(bytes) = tokio::fs::read(web_root.join(rel)).await
        && let Ok(text) = String::from_utf8(bytes)
    {
        let transformed = transformations.run_transformation(rel, text).await;
        return (
            [(axum::http::header::CONTENT_TYPE, web_file_mime(rel))],
            transformed,
        )
            .into_response();
    }
    next.run(req).await
}

/// The MIME type for a transformed web file, from its extension (the plain
/// `ServeDir` path normally derives this; a transformed response must match).
fn web_file_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "js" | "mjs" => "application/javascript",
        "css" => "text/css",
        "html" => "text/html; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        _ => "text/plain; charset=utf-8",
    }
}

/// Redirects the exact path `/web` (no trailing slash) to `/web/`, so
/// jellyfin-web's relative asset URLs resolve under `/web/` rather than the
/// server root. All other requests (including `/web/` and `/web/*`) pass through
/// untouched.
async fn redirect_bare_web(req: Request, next: Next) -> Response {
    if req.uri().path() == "/web" {
        return Redirect::permanent("/web/").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::mount_web;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hermit_traits::plugins::FileTransformationService;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    /// An upper-casing test transformer.
    struct Upper;
    #[async_trait::async_trait]
    impl hermit_traits::plugins::FileTransformer for Upper {
        async fn transform(&self, _path: &str, contents: String) -> String {
            contents.to_uppercase()
        }
    }

    /// An identity test transformer (registration presence is what matters).
    struct Identity;
    #[async_trait::async_trait]
    impl hermit_traits::plugins::FileTransformer for Identity {
        async fn transform(&self, _path: &str, contents: String) -> String {
            contents
        }
    }

    /// A transformation service over the extension registry with the File
    /// Transformation plugin registered (and thus enabled by default).
    fn transformations(plugins_dir: &std::path::Path) -> Arc<dyn FileTransformationService> {
        let plugins: Arc<dyn hermit_traits::plugins::PluginManager> =
            Arc::new(hermit_core::HermitPluginManager::new(
                hermit_extensions::registered_plugins(&hermit_extensions::builtin_extensions()),
                plugins_dir.to_path_buf(),
            ));
        Arc::new(
            hermit_extensions::file_transformation::WebFileTransformationService::new(
                plugins,
                "http://127.0.0.1:0".to_owned(),
            ),
        )
    }

    #[tokio::test]
    async fn serves_web_bundle_and_redirects_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>hermit").unwrap();

        let app = mount_web(Router::new(), dir.path(), transformations(dir.path()));

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
            .clone()
            .oneshot(Request::builder().uri("/web/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(web.status(), StatusCode::OK);

        // A bare `/web` (no trailing slash) redirects to `/web/` so the bundle's
        // relative asset URLs resolve under `/web/` rather than the server root.
        let bare = app
            .clone()
            .oneshot(Request::builder().uri("/web").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(bare.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(bare.headers()["location"], "/web/");

        // A relative asset resolves under `/web/` and is served from the bundle.
        std::fs::write(dir.path().join("runtime.bundle.js"), "//js").unwrap();
        let asset = app
            .oneshot(
                Request::builder()
                    .uri("/web/runtime.bundle.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_only_when_no_bundle() {
        let dir = tempfile::tempdir().unwrap();
        // No index.html written → router is returned unchanged (no `/` route).
        let app = mount_web(Router::new(), dir.path(), transformations(dir.path()));
        let root = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn web_file_is_served_through_the_transformation_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>hermit").unwrap();
        std::fs::write(dir.path().join("a.js"), "hello world").unwrap();

        let service = transformations(dir.path());
        // Register an in-process transformer for `a.js` only.
        service
            .add_transformation(uuid::Uuid::from_u128(9), "a.js", Arc::new(Upper))
            .await;

        let app = mount_web(Router::new(), dir.path(), Arc::clone(&service));

        // The matching file is transformed and typed by extension.
        let hit = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web/a.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hit.status(), StatusCode::OK);
        assert_eq!(hit.headers()["content-type"], "application/javascript");
        let body = axum::body::to_bytes(hit.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"HELLO WORLD");

        // A non-matching file falls through to plain static serving.
        let miss = app
            .oneshot(
                Request::builder()
                    .uri("/web/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(miss.status(), StatusCode::OK);
        let body = axum::body::to_bytes(miss.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"<!doctype html>hermit");
    }

    #[tokio::test]
    async fn traversal_paths_never_reach_the_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "x").unwrap();
        let service = transformations(dir.path());
        // A greedy pattern that would match anything, including `..` paths.
        service
            .add_transformation(uuid::Uuid::from_u128(9), ".*", Arc::new(Identity))
            .await;
        let app = mount_web(Router::new(), dir.path(), service);
        // `..` components must not be read+served by the transform branch.
        let out = app
            .oneshot(
                Request::builder()
                    .uri("/web/../secret.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(out.status(), StatusCode::OK);
    }

    #[test]
    fn normalizes_pascal_query_keys_to_camel() {
        use super::normalize_query_keys;
        // PascalCase (SDK) keys fold to camelCase; values (incl. their case) survive.
        assert_eq!(
            normalize_query_keys("ParentId=AbC-123&IncludeItemTypes=Movie&Recursive=true"),
            "parentId=AbC-123&includeItemTypes=Movie&recursive=true"
        );
        // Already-camelCase keys are untouched (idempotent).
        assert_eq!(
            normalize_query_keys("parentId=x&sortBy=y"),
            "parentId=x&sortBy=y"
        );
        // Valueless flags and empty segments don't panic.
        assert_eq!(normalize_query_keys("Foo&bar"), "foo&bar");
    }
}
