//! Ferrofin server — the composition root (port of `Jellyfin.Server`).
//!
//! Loads bootstrap [`config`], opens + migrates the SQLite DB, discovers ffmpeg,
//! constructs the concrete `ferrofin-core` managers and injects them as the
//! `Arc<dyn Trait>` fields of `ferrofin-api`'s `AppState`, seeds a default
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

pub mod bootstrap;
pub mod config;
pub mod media_encoding;
pub mod metrics_wiring;
pub mod planner;
pub mod seed;
pub mod state;
pub mod vaapi_probe;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::serve::ListenerExt as _;
use tower::Layer as _;
use tower_http::services::ServeDir;

use crate::bootstrap::{
    FfmpegPaths, discover_ffmpeg, init_tracing, open_database, shutdown_tracing,
};
use crate::config::Config;
use crate::seed::{SeedOutcome, seed_default_admin};
use crate::state::{WiredApp, build_app_state};

/// The version Ferrofin reports for itself (startup log line and the session
/// app-version fallback in the authorization context).
///
/// Prefers the `SERVICE_VERSION` environment variable — stamped into the release
/// image from the git tag by CI. When unset (local/dev builds), it falls back to
/// [`ferrofin_health::build_version`], the `git describe` value baked in at
/// compile time (latest tag + commits-since + HEAD sha, or the
/// `FERROFIN_GIT_DESCRIBE` build-time override for `.git`-less Docker builds),
/// so the reported version is derived from git rather than a hardcoded number
/// that goes stale. The same value is served on `GET /health/live` as `build`.
pub(crate) fn service_version() -> String {
    std::env::var("SERVICE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| ferrofin_health::build_version().to_owned())
}

/// Logs the resolved configuration at startup, grouped into a few `INFO` lines
/// (server/version, paths, network, library & admin).
fn log_startup_banner(config: &Config) {
    tracing::info!(
        server_name = %config.server_name,
        version = service_version(),
        "ferrofin-server starting"
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

/// Reports the discovered encoder, or a disabled-encoder placeholder.
///
/// ffmpeg is required for playback but not for the server to boot: on a failed
/// discovery this warns and returns bare `ffmpeg`/`ffprobe` names, so the API
/// still comes up on a host without ffmpeg installed and playback 500s (rather
/// than the process failing to start) until a working ffmpeg is configured.
///
/// `?e` prints the full anyhow context chain — which candidate was tried and how
/// it was resolved (`--ffmpeg` flag vs `$PATH`) — so "why no transcode" is
/// answerable from that one log line.
fn encoder_or_disabled(discovered: anyhow::Result<FfmpegPaths>) -> FfmpegPaths {
    match discovered {
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
                error = ?e,
                "ffmpeg unavailable — transcoding/playback will be disabled until configured",
            );
            FfmpegPaths {
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
                capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
                chromaprint_muxer: false,
            }
        }
    }
}

/// Logs how far into the boot each stage of [`run`] finished, at `debug`.
///
/// Cold start is a sequence of a handful of coarse stages (database + encoder
/// probe, manager wiring, seeding, router, bind), and without a per-stage
/// timestamp the only observable is the total — which says nothing about which
/// stage to attack. Emitted with `RUST_LOG=ferrofin_server=debug`; a disabled
/// `debug!` costs a single atomic load, so this is free in production.
fn boot_stage(started: std::time::Instant, stage: &'static str) {
    tracing::debug!(
        stage,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "boot stage"
    );
}

/// Waits for the first shutdown trigger and reports why, for the shutdown log.
///
/// `"api"` — an API-initiated shutdown/restart (`POST /System/Shutdown|Restart`
/// fires `shutdown_tx`); `"sigint"` — ctrl-c; `"sigterm"` — `docker stop`,
/// `systemctl stop`, a Kubernetes pod eviction. Before this the process
/// hard-killed on ctrl-c (no drain, no log/trace flush); now all three drain
/// cleanly.
///
/// SIGTERM matters more than SIGINT in production and was previously unhandled.
/// A container's server process is **PID 1**, and the kernel does not apply
/// default signal dispositions to PID 1 — an unhandled SIGTERM is *ignored*, so
/// `docker stop`/`docker compose stop -t N` sat out its whole grace period and
/// then SIGKILLed (observed: the benchmark's cold-leg containers all exited
/// `137`, and each restart cost the full 60 s grace). Every Kubernetes rolling
/// update hit the same path. Handling it turns that into a real drain.
async fn shutdown_signal(
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    signals: &Signals,
) -> &'static str {
    #[cfg_attr(not(unix), allow(unused_mut, unused_variables))]
    let mut streams = signals.streams.lock().await;
    // `None` (install failed) must never resolve, or the select would treat it
    // as an immediate shutdown; a never-ready future is the neutral element.
    #[cfg(unix)]
    let SignalStreams { sigterm, sigint } = &mut *streams;
    #[cfg(unix)]
    let (sigterm, sigint) = (sigterm.as_mut(), sigint.as_mut());
    #[cfg(unix)]
    let terminate = async move {
        match sigterm {
            Some(stream) => {
                stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    #[cfg(unix)]
    let interrupt = async move {
        match sigint {
            Some(stream) => {
                stream.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    #[cfg(not(unix))]
    let interrupt = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "ctrl-c listener failed");
        }
    };

    let reason = tokio::select! {
        _ = shutdown_rx => "api",
        () = terminate => "sigterm",
        () = interrupt => "sigint",
    };
    if reason != "api" {
        signals
            .fired
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    reason
}

/// The graceful-shutdown future handed to axum: waits for the first shutdown
/// trigger, then tells connected clients before the socket drains, so they
/// show "server unavailable" instead of silently hanging (C# sends
/// `ServerShuttingDown` for restart and shutdown alike — 10.11.8 has no
/// `ServerRestarting` push — so both drain the same way), and closes every
/// WebSocket: an upgraded connection is outside axum's drain, and a socket
/// that survived would stay bound to the OLD host's session manager after an
/// in-process restart (Kestrel closes them on stop; clients reconnect).
async fn announce_shutdown(
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    sessions: Arc<dyn ferrofin_traits::session::SessionManager>,
    signals: Signals,
) {
    let reason = shutdown_signal(shutdown_rx, &signals).await;
    tracing::info!(reason, "graceful shutdown requested");
    let _ = sessions
        .send_message_to_all_sessions(
            ferrofin_model::session::SessionMessageType::ServerShuttingDown,
            "",
        )
        .await;
    ferrofin_api::handlers::websocket::close_all_sockets();
}

/// Boots the server from a resolved [`Config`] and serves until shutdown.
///
/// This is the whole composition root, in order: initialise logging, open +
/// migrate the database, discover ffmpeg (non-fatal — the API still boots without
/// it, playback just 500s until configured), wire every concrete manager into the
/// shared `AppState`, seed a default administrator when the install is fresh, mount
/// the `ferrofin-api` router, and `axum::serve` on the configured bind address with
/// graceful shutdown.
///
/// The binary calls this after parsing CLI flags; it is the single entry point so
/// the boot sequence has exactly one implementation.
///
/// `POST /System/Restart` (and a scheduled backup restore) re-create the host
/// IN-PROCESS: the server drains, the state is dropped, and [`serve_once`] runs
/// again on the same configuration — Jellyfin's `Program.Main`
/// `do { … } while (_restartOnShutdown)`. A container therefore survives a
/// restart without a supervisor, exactly as a Jellyfin container does. Shutdown
/// and signals return from here and the process exits.
///
/// # Errors
///
/// Returns an error if the database cannot be opened/migrated, manager wiring
/// fails, seeding fails, the listener cannot bind the configured address, or the
/// server loop errors.
pub async fn run(config: Config) -> anyhow::Result<()> {
    // Hold the log-writer guard until after the server drains so the last
    // buffered file-log lines flush rather than being discarded on exit.
    let _log_guard = init_tracing(&config);
    log_startup_banner(&config);
    // The Prometheus pipeline is process-global (OTel meter provider + the
    // set-once HTTP instruments), so it lives across in-process restarts.
    let mut metrics = None;
    // The OS signal streams are created ONCE: a signal delivered while no
    // stream exists is dropped, so per-lifetime streams would lose a
    // `docker stop` that lands during the rebuild window of a restart.
    let signals = Signals::install();
    loop {
        let started = std::time::Instant::now();
        if !serve_once(&config, started, &mut metrics, &signals).await? {
            break;
        }
        tracing::info!("restart requested — re-creating the host in-process");
    }
    // Flush the OTLP batch queue now that the server has drained; a restart that
    // loses the last spans is a bug. No-op when trace export is disabled.
    shutdown_tracing();
    Ok(())
}

/// Marks setup complete when the wizard flag says it is not but a configured
/// administrator already exists.
///
/// Port of `ApplicationHost.EnsureStartupWizardIntegrity`
/// (ApplicationHost.cs:443-459), and it is a security fix, not cosmetics: the
/// `FirstTimeSetupOrAuth`/`FirstTimeSetupOrElevated` policies open the
/// `/Startup/*` endpoints to ANONYMOUS callers while the flag is false, so a
/// headless install that seeded its administrator from
/// `FERROFIN_ADMIN_PASSWORD` — never touching the web wizard — served its setup
/// endpoints unauthenticated for the rest of its life. It also feeds
/// `/System/Info/Public`'s `StartupWizardCompleted`, which is how a client
/// decides whether to route the user into the wizard.
///
/// A passwordless seed (no configured administrator yet) deliberately leaves the
/// flag false, so the browser wizard still works on a fresh install.
async fn ensure_startup_wizard_integrity(
    users: &dyn ferrofin_traits::library::UserManager,
    config: &dyn ferrofin_traits::configuration::ServerConfigurationManager,
) -> anyhow::Result<()> {
    let current = config.configuration().await?;
    if current.is_startup_wizard_completed {
        return Ok(());
    }
    let mut has_configured_administrator = false;
    for user in users.get_users().await? {
        // C# `!string.IsNullOrEmpty(user.Password)` — a row with an empty
        // password hash is not a configured administrator.
        if user.password.as_deref().is_none_or(str::is_empty) {
            continue;
        }
        if users
            .get_user_dto(&user, None)
            .await?
            .policy
            .is_some_and(|p| p.is_administrator)
        {
            has_configured_administrator = true;
            break;
        }
    }
    if !has_configured_administrator {
        return Ok(());
    }
    tracing::warn!(
        "the startup wizard is marked incomplete but a configured administrator already exists; \
         marking setup as completed to prevent the unauthenticated setup endpoints from being reachable"
    );
    let mut updated = (*current).clone();
    updated.is_startup_wizard_completed = true;
    config.update_configuration(&updated).await?;
    Ok(())
}

/// The process-lifetime half of the metrics wiring (see [`run`]).
struct ProcessMetrics {
    handle: ferrofin_metrics::MetricsHandle,
    gauges: metrics_wiring::SamplerGauges,
}

/// One server lifetime: boot, serve until drained, tear the host down, and
/// report whether an API restart was requested (`true` → the caller runs it
/// again). A signal-initiated drain always reports `false`.
async fn serve_once(
    config: &Config,
    started: std::time::Instant,
    metrics: &mut Option<ProcessMetrics>,
    signals: &Signals,
) -> anyhow::Result<bool> {
    // A restore scheduled by `POST /Backup/Restore` (then restart) is applied
    // here, before the database file is opened — Jellyfin's `RestoreBackupPath`
    // sequence. Synchronous on purpose: nothing may touch the tree meanwhile.
    match ferrofin_api::handlers::backup::apply_pending_restore(&boot_tree_roots(config)) {
        Ok(Some(archive)) => tracing::warn!(archive = %archive.display(), "restored backup"),
        Ok(None) => {}
        Err(e) => tracing::error!(error = %e, "scheduled backup restore failed"),
    }
    // Opening/migrating the database, probing ffmpeg and probing `fpcalc` share
    // no state, and all three are dominated by waiting (SQLite I/O; five
    // `ffmpeg`/`ffprobe` spawns; one `fpcalc` spawn). Run them CONCURRENTLY —
    // measured, this hides the whole database open AND the `fpcalc` probe behind
    // the encoder probe (six concurrent spawns finish in 27.4 ms against 27.9 ms
    // for five, so the `fpcalc` leg is free). Nothing is skipped or reordered
    // relative to what depends on it: `build_app_state` still needs all three.
    // `fpcalc` is probed unconditionally rather than only when the `chromaprint`
    // muxer turns out to be missing, because waiting to learn whether it IS
    // missing is exactly the 15 ms serial spawn this removes; the answer is
    // simply discarded when the muxer is present.
    let (db, ffmpeg, fpcalc) = tokio::join!(
        open_database(config),
        discover_ffmpeg(config, None),
        ferrofin_extensions::fingerprint::discover_fpcalc_async(),
    );
    boot_stage(started, "db+encoder probe");
    let db = db?;

    // ffmpeg is required for playback but not for the server to boot: warn and
    // continue so the API comes up even on a host without ffmpeg installed.
    // `system.json` EncoderAppPath is not yet loaded at this bootstrap stage, so
    // no persisted fallback is supplied here.
    let ffmpeg = encoder_or_disabled(ffmpeg);

    // Wire every concrete manager into the shared AppState (the composition root).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, config, &ffmpeg, fpcalc, shutdown_tx)
        .await
        .context("failed to assemble application state")?;
    boot_stage(started, "app state wired");

    // Fresh-install seeding: on a database with no users, create the configured
    // default administrator (port of `UserManager.InitializeAsync` +
    // startup-wizard `UpdateStartupUser`). A no-op once any user exists.
    match seed_default_admin(wired.state.users.as_ref(), config)
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
            // FERROFIN_ADMIN_PASSWORD for a headless, ready-to-use admin.
            tracing::warn!(
                %username,
                "seeded a PASSWORDLESS default administrator — complete setup via the web \
                 wizard at /web, or set FERROFIN_ADMIN_PASSWORD for a headless install"
            );
        }
    }

    ensure_startup_wizard_integrity(wired.state.users.as_ref(), wired.state.config.as_ref())
        .await
        .context("failed to verify the startup-wizard flag")?;

    boot_stage(started, "admin seeded");
    let mut router = mount_web(
        ferrofin_api::create_router(wired.state.clone()),
        &config.web_dir,
        Arc::clone(&wired.file_transformations),
    );

    // Optional Prometheus `/metrics` (gated on `EnableMetrics`, restart required —
    // Jellyfin semantics). Disabled ⇒ the route is never mounted (404), the global
    // meter stays the built-in noop, and no sampler task runs. The handle owns the
    // observable callbacks and is process-global, so it lives in `metrics` across
    // restarts; only the sampler is per server lifetime.
    //
    // The bootstrap `FERROFIN_ENABLE_METRICS` env / `config.toml` override wins when
    // set (declarative/GitOps deploys); otherwise defer to the persisted
    // `ServerConfiguration.EnableMetrics` (dashboard/API toggle).
    let enable_metrics = match config.enable_metrics {
        Some(forced) => forced,
        None => wired
            .state
            .config
            .configuration()
            .await
            .is_ok_and(|c| c.enable_metrics),
    };
    let sampler = enable_metrics
        .then(|| {
            // Sampler cadence is a bootstrap knob (env / config.toml), kept out of
            // the API `ServerConfiguration` so `/System/Configuration` stays
            // byte-identical to Jellyfin. `None`/0 → the sampler's 15 s default.
            let interval = config.metrics_sample_interval.unwrap_or(0);
            enable_metrics_endpoint(&mut router, &wired.state, &db, interval, metrics)
        })
        .flatten();

    // Post-startup: flip the host's core-startup flag (mirrors `CoreAppHost`
    // marking itself ready once services are registered).
    wired.app_host.mark_core_startup_complete();

    // Case-insensitive routing: Jellyfin's API is case-insensitive but axum's
    // router is not, and clients (jellyfin-web included) call some paths in
    // non-canonical case. Rewrite each request's path to its registered case
    // BEFORE routing. This must wrap the whole router as an outer layer (not
    // `Router::layer`, which runs per-matched-route, too late to re-route).

    boot_stage(started, "router mounted");
    let addr = SocketAddr::new(config.bind_addr, config.port);
    let listener = bind_listener(addr).await?;
    boot_stage(started, "listener bound");
    tracing::info!(%addr, "ferrofin-server listening");

    serve_until_drained(
        listener,
        router,
        announce_shutdown(
            shutdown_rx,
            Arc::clone(&wired.state.sessions),
            signals.clone(),
        ),
        config.shutdown_timeout_secs,
    )
    .await?;

    // Tear the host down (Jellyfin: `using CoreAppHost` per iteration): stop
    // every background task, then close the pools so nothing from this lifetime
    // still holds the database file when the next one opens — or restores — it.
    if let Some(sampler) = sampler {
        sampler.abort();
    }
    let restart = wired.lifecycle.restart_requested() && !signals.fired();
    // Running scheduled tasks (a library scan, trickplay generation) are their
    // own spawned runs holding this host's manager graph: cancel them first, as
    // Jellyfin's host dispose does, so nothing keeps working against a pool
    // that is about to close.
    cancel_running_tasks(wired.state.tasks.as_ref()).await;
    let WiredApp { background, .. } = wired;
    for task in background {
        task.abort();
    }
    db.close().await;
    tracing::info!(
        uptime_s = started.elapsed().as_secs(),
        "ferrofin-server stopped"
    );
    Ok(restart)
}

/// Serves until the shutdown trigger fires and the drain completes — or
/// `timeout_secs` after the trigger, whichever comes first (Kestrel's
/// `HostOptions.ShutdownTimeout`). Unbounded, a client mid-stream would keep
/// the old host alive — listener already closed — for as long as it kept
/// reading, so a restart during playback never came back. Dropping the serve
/// future past the deadline aborts the remaining connections.
async fn serve_until_drained(
    listener: axum::serve::TapIo<tokio::net::TcpListener, fn(&mut tokio::net::TcpStream)>,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    timeout_secs: u32,
) -> anyhow::Result<()> {
    let app = axum::middleware::from_fn(canonicalize_path_case).layer(router);
    let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel::<()>();
    let serve =
        axum::serve(
            listener,
            // `with_connect_info` so handlers can read the client's socket address
            // (e.g. `GET /System/Endpoint` reporting `IsLocal` for a loopback peer).
            axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
                SocketAddr,
            >(app),
        )
        .with_graceful_shutdown(async move {
            shutdown.await;
            let _ = drain_started_tx.send(());
        });
    let deadline = async move {
        let _ = drain_started_rx.await;
        tokio::time::sleep(std::time::Duration::from_secs(u64::from(timeout_secs))).await;
    };
    tokio::select! {
        result = serve => result.context("server error"),
        () = deadline => {
            tracing::warn!(
                timeout_s = timeout_secs,
                "shutdown timeout elapsed — aborting the remaining connections"
            );
            Ok(())
        }
    }
}

/// Cancels every scheduled task currently running (best-effort: a task that
/// finishes in between is simply no longer running).
async fn cancel_running_tasks(tasks: &dyn ferrofin_traits::tasks::TaskManager) {
    let Ok(list) = tasks.get_tasks().await else {
        return;
    };
    for task in list {
        if task.state == ferrofin_model::tasks::TaskState::Running
            && let Some(id) = task.id.as_deref()
            && let Err(e) = tasks.cancel_task(id).await
        {
            tracing::debug!(task = id, error = %e, "could not cancel a running task on shutdown");
        }
    }
}

/// The on-disk roots a scheduled backup restore writes to, resolved before the
/// configuration manager exists: the configured data/config dirs and database
/// file, plus `MetadataPath` read straight from `system.json` so a relocated
/// metadata tree is restored where the server will look for it.
fn boot_tree_roots(config: &Config) -> ferrofin_api::handlers::backup::TreeRoots {
    let mut roots = ferrofin_api::handlers::backup::TreeRoots::defaults(
        &config.data_dir,
        &config.config_dir,
        &config.database_path(),
    );
    if let Some(metadata) = std::fs::read_to_string(config.config_dir.join("system.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|cfg| cfg.get("MetadataPath")?.as_str().map(str::to_owned))
        .filter(|p| !p.trim().is_empty())
    {
        roots.metadata = PathBuf::from(metadata);
    }
    roots
}

/// The process's OS signal streams (SIGTERM + SIGINT on unix), created once
/// per process and shared by every server lifetime (the graceful-shutdown
/// future must be `'static`, hence the shared handle).
#[derive(Clone)]
struct Signals {
    streams: Arc<tokio::sync::Mutex<SignalStreams>>,
    /// Set once any signal has been observed: the loop must exit even if an
    /// API restart was requested in the same drain.
    fired: Arc<std::sync::atomic::AtomicBool>,
}

struct SignalStreams {
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigint: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn install() -> Self {
        #[cfg(unix)]
        let stream = |kind: tokio::signal::unix::SignalKind, name: &str| {
            match tokio::signal::unix::signal(kind) {
                Ok(stream) => Some(stream),
                Err(e) => {
                    tracing::warn!(error = %e, signal = name, "signal listener could not be installed");
                    None
                }
            }
        };
        Self {
            streams: Arc::new(tokio::sync::Mutex::new(SignalStreams {
                #[cfg(unix)]
                sigterm: stream(tokio::signal::unix::SignalKind::terminate(), "SIGTERM"),
                #[cfg(unix)]
                sigint: stream(tokio::signal::unix::SignalKind::interrupt(), "SIGINT"),
            })),
            fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Binds the HTTP listener, with Nagle's algorithm off on every connection it
/// accepts (see [`disable_nagle`]).
async fn bind_listener(
    addr: SocketAddr,
) -> anyhow::Result<axum::serve::TapIo<tokio::net::TcpListener, fn(&mut tokio::net::TcpStream)>> {
    Ok(tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?
        .tap_io(disable_nagle as fn(&mut tokio::net::TcpStream)))
}

/// Turns Nagle's algorithm off on every accepted connection (`TCP_NODELAY`).
///
/// Kestrel — the server Jellyfin runs on — sets `NoDelay` on accepted sockets by
/// default; `axum::serve` does not, so without this tap Ferrofin diverges from
/// Jellyfin by up to a delayed-ACK interval per response.
///
/// It matters because of how hyper writes a *streamed* response: the header
/// block goes out in one write, the body in the next. With Nagle on, the body's
/// trailing sub-MSS segment is held back until the peer acknowledges the header
/// segment — and on a reused keep-alive connection the peer has already left TCP
/// quick-ack mode, so that acknowledgement is the 40 ms delayed-ACK timer. Every
/// `ServeFile` response (posters and every other image, HLS segments,
/// subtitles, downloads) therefore paid a ~40 ms stall on a warm connection
/// while burning under 1 ms of CPU. JSON responses are unaffected — hyper emits
/// a known-length body in the same vectored write as the headers.
///
/// A failure to set the option is logged and ignored: it is an optimisation, not
/// a correctness requirement, and a connection that cannot take the sockopt must
/// still be served.
fn disable_nagle(stream: &mut tokio::net::TcpStream) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "failed to set TCP_NODELAY on an accepted connection");
    }
}

/// The vendored Jellyfin OpenAPI spec, embedded so the metrics layer can label
/// requests with their Jellyfin `controller`/`action` (from each operation's
/// first tag + `operationId`) — prometheus-net parity. Only read when metrics
/// are enabled.
const OPENAPI_SPEC: &str = include_str!("../../../contracts/jellyfin-openapi-10.11.8.json");

/// Initialises the metrics pipeline, mounts `/metrics` + the HTTP tracking layer
/// onto `router`, and spawns the background gauge sampler. Returns the
/// sampler task to abort when the server drains, or `None` if
/// init fails (logged; the server continues without metrics).
fn enable_metrics_endpoint(
    router: &mut Router,
    state: &ferrofin_api::AppState,
    db: &ferrofin_db::Database,
    sample_interval_seconds: u32,
    process: &mut Option<ProcessMetrics>,
) -> Option<tokio::task::JoinHandle<()>> {
    if process.is_none() {
        // `endpoint` labels are the axum route templates (`MatchedPath`), so key
        // the controller/action lookup by the same normalization the router applies.
        let route_labels = ferrofin_metrics::RouteLabels::from_openapi_spec(OPENAPI_SPEC, |p| {
            ferrofin_api::routes::normalize_contract_path(p)
        });
        match ferrofin_metrics::init(route_labels, tokio::runtime::Handle::current()) {
            Ok(handle) => {
                let gauges = metrics_wiring::register_gauges(&handle);
                *process = Some(ProcessMetrics { handle, gauges });
                tracing::info!("prometheus metrics enabled at /metrics");
            }
            Err(e) => {
                tracing::warn!(error = %e, "metrics init failed — continuing without");
                return None;
            }
        }
    }
    let process = process.as_ref()?;
    *router = metrics_wiring::mount(std::mem::take(router), &process.handle);
    Some(metrics_wiring::spawn_sampler(
        process.gauges.clone(),
        Arc::clone(&state.sessions),
        db.clone(),
        sample_interval_seconds,
    ))
}

/// Rewrites a request's path to its canonical Jellyfin case before routing.
///
/// Jellyfin's API is case-insensitive (ASP.NET); axum's router is case-sensitive,
/// and clients call some paths in non-canonical case (e.g. `/Localization/countries`).
/// [`ferrofin_api::routes::canonicalize_path`] re-cases route literals to the
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
    let new_path = ferrofin_api::routes::canonicalize_path(&path).unwrap_or_else(|| path.clone());
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
/// Ferrofin serves whatever static bundle the operator places in `web_dir` — e.g.
/// a built [`jellyfin-web`](https://github.com/jellyfin/jellyfin-web) `dist/`.
/// The web client talks to Ferrofin over the same-origin HTTP API, exactly as it
/// would against upstream Jellyfin. If the directory has no `index.html` the
/// server runs API-only and `/` returns `404` (the API is unaffected either way,
/// since no contract route lives under `/web` or at `/`).
fn mount_web(
    router: Router,
    web_dir: &Path,
    transformations: Arc<dyn ferrofin_traits::plugins::FileTransformationService>,
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
        // Jellyfin's `UseResponseCompression()` sits above BOTH its API and its
        // static-file middleware, and jellyfin-web's bundles are the payloads
        // that gain most from it. `nest_service` mounts a service the API
        // router's own compression layer never sees, and a transformed file is
        // answered by the middleware above rather than by `ServeDir`, so the
        // layer goes here — outside both. An API response arrives already
        // carrying `Content-Encoding`, and `tower_http` never re-encodes one,
        // so the two layers cannot compound.
        .layer(ferrofin_api::compression::compression_layer())
}

/// Serves a `/web` file through the File Transformation pipeline when a
/// registered transformation matches it; all other requests pass through to
/// the static `ServeDir` untouched.
///
/// Port of the File Transformation plugin's static-file middleware: it reads
/// the matched file, runs the pipeline over its text, and responds with the
/// transformed contents (binary or unreadable files fall through untouched).
async fn transform_web_file(
    transformations: &Arc<dyn ferrofin_traits::plugins::FileTransformationService>,
    web_root: &Path,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if req.method() == axum::http::Method::GET
        // A directory request IS the index request: jellyfin-web is an SPA
        // that browsers always enter at `/web/` (deep links are hash
        // fragments), so the bare directory must run the index.html
        // transforms — falling through to ServeDir's own index handling
        // would serve it untransformed and no client would ever see an
        // index-targeted transform (e.g. an injected plugin script tag).
        && let Some(rel) = path
            .strip_prefix("/web/")
            .map(|r| if r.is_empty() { "index.html" } else { r })
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
    use ferrofin_traits::plugins::FileTransformationService;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    /// An upper-casing test transformer.
    struct Upper;
    #[async_trait::async_trait]
    impl ferrofin_traits::plugins::FileTransformer for Upper {
        async fn transform(&self, _path: &str, contents: String) -> String {
            contents.to_uppercase()
        }
    }

    /// An identity test transformer (registration presence is what matters).
    struct Identity;
    #[async_trait::async_trait]
    impl ferrofin_traits::plugins::FileTransformer for Identity {
        async fn transform(&self, _path: &str, contents: String) -> String {
            contents
        }
    }

    /// A transformation service over the extension registry with the File
    /// Transformation plugin registered (and thus enabled by default).
    fn transformations(plugins_dir: &std::path::Path) -> Arc<dyn FileTransformationService> {
        let plugins: Arc<dyn ferrofin_traits::plugins::PluginManager> =
            Arc::new(ferrofin_core::FerrofinPluginManager::new(
                ferrofin_extensions::registered_plugins(&ferrofin_extensions::builtin_extensions()),
                plugins_dir.to_path_buf(),
            ));
        Arc::new(
            ferrofin_extensions::file_transformation::WebFileTransformationService::new(
                plugins,
                "http://127.0.0.1:0".to_owned(),
            ),
        )
    }

    #[tokio::test]
    async fn serves_web_bundle_and_redirects_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>ferrofin").unwrap();

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

    /// The static bundle is served compressed when the client negotiates it —
    /// Jellyfin gzips/brotlis jellyfin-web's assets, and `nest_service` puts
    /// `ServeDir` outside the API router's own compression layer.
    #[tokio::test]
    async fn web_assets_are_compressed_and_decode_identically() {
        use std::io::Read as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>ferrofin").unwrap();
        // Repetitive, comfortably above the minimum compressible size.
        let js = "function chunk(){return 'ferrofin';}\n".repeat(200);
        std::fs::write(dir.path().join("bundle.js"), &js).unwrap();

        let app = mount_web(Router::new(), dir.path(), transformations(dir.path()));

        let plain = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/web/bundle.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(plain.status(), StatusCode::OK);
        assert!(plain.headers().get("content-encoding").is_none());
        let plain_body = axum::body::to_bytes(plain.into_body(), usize::MAX)
            .await
            .unwrap();

        let gz = app
            .oneshot(
                Request::builder()
                    .uri("/web/bundle.js")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gz.status(), StatusCode::OK);
        assert_eq!(gz.headers()["content-encoding"], "gzip");
        let gz_body = axum::body::to_bytes(gz.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            gz_body.len() < plain_body.len(),
            "compressed asset should be smaller: {} vs {}",
            gz_body.len(),
            plain_body.len()
        );
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&gz_body[..])
            .read_to_end(&mut decoded)
            .expect("valid gzip stream");
        assert_eq!(
            decoded,
            plain_body.to_vec(),
            "decoded asset must be identical"
        );
        assert_eq!(String::from_utf8(decoded).unwrap(), js);
    }

    /// A file rewritten by the File Transformation pipeline is answered by the
    /// middleware rather than `ServeDir`, so it needs the compression layer to
    /// sit outside that middleware too.
    #[tokio::test]
    async fn transformed_web_files_are_compressed_too() {
        use std::io::Read as _;

        // Over the one-MTU compression floor (see `compression::MIN_COMPRESSIBLE_BYTES`);
        // this test is about the transformed body being encoded, not the size gate.
        // Derived, not hardcoded, so the two cannot drift apart again.
        const LINES: usize = 200;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>ferrofin").unwrap();
        std::fs::write(dir.path().join("a.js"), "hello world\n".repeat(LINES)).unwrap();

        let service = transformations(dir.path());
        service
            .add_transformation(uuid::Uuid::from_u128(11), "a.js", Arc::new(Upper))
            .await;
        let app = mount_web(Router::new(), dir.path(), Arc::clone(&service));

        let gz = app
            .oneshot(
                Request::builder()
                    .uri("/web/a.js")
                    .header("accept-encoding", "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gz.status(), StatusCode::OK);
        assert_eq!(gz.headers()["content-encoding"], "gzip");
        let gz_body = axum::body::to_bytes(gz.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&gz_body[..])
            .read_to_end(&mut decoded)
            .expect("valid gzip stream");
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "HELLO WORLD\n".repeat(LINES),
            "the transformed text must survive compression unchanged"
        );
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
        std::fs::write(dir.path().join("index.html"), "<!doctype html>ferrofin").unwrap();
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
        assert_eq!(&body[..], b"<!doctype html>ferrofin");
    }

    #[tokio::test]
    async fn directory_request_runs_the_index_transforms() {
        // Browsers enter the SPA at `/web/` — never `/web/index.html` — so
        // the bare directory request MUST carry index-targeted transforms
        // (the home-sections plugin's injected script tag regressed here).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<body></body>").unwrap();
        let service = transformations(dir.path());
        service
            .add_transformation(uuid::Uuid::from_u128(7), "index.html", Arc::new(Upper))
            .await;
        let app = mount_web(Router::new(), dir.path(), service);
        let out = app
            .oneshot(Request::builder().uri("/web/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(out.status(), StatusCode::OK);
        assert_eq!(out.headers()["content-type"], "text/html; charset=utf-8");
        let body = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"<BODY></BODY>", "index transform ran on /web/");
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

    /// The listener `run` binds really clears Nagle on the sockets it serves.
    ///
    /// Discriminating: a plain `TcpListener` accepts with `TCP_NODELAY` **off**
    /// (asserted first, so the test fails loudly if the platform ever changes
    /// that premise and makes the assertion below vacuous), and `axum::serve`
    /// adds nothing of its own. Drop the `.tap_io(disable_nagle)` from
    /// [`bind_listener`] — or stub `disable_nagle` out — and the second
    /// assertion fails.
    #[tokio::test]
    async fn accepted_connections_have_nagle_disabled() {
        use axum::serve::Listener as _;

        // Premise: a raw accepted socket has Nagle ON.
        let plain = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let plain_addr = plain.local_addr().expect("addr");
        let client = tokio::net::TcpStream::connect(plain_addr)
            .await
            .expect("connect");
        let (raw, _) = plain.accept().await.expect("accept");
        assert!(
            !raw.nodelay().expect("nodelay"),
            "a raw accepted socket is expected to have TCP_NODELAY off; if it is on by \
             default this test can no longer discriminate"
        );
        drop((raw, client, plain));

        // The real thing: whatever `run` binds must hand out no-delay sockets.
        let mut listener = super::bind_listener("127.0.0.1:0".parse().expect("addr"))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let (served, _) = listener.accept().await;
        assert!(
            served.nodelay().expect("nodelay"),
            "accepted connections must have TCP_NODELAY set"
        );
        drop((served, client));
    }
}
