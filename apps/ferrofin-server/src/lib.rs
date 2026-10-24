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

mod base_url;
pub mod bootstrap;
pub mod config;
mod encoding_options;
mod existing_database;
pub mod listen;
pub mod media_encoding;
pub mod metrics_wiring;
pub mod planner;
pub mod seed;
mod startup;
pub mod state;
#[cfg(test)]
mod test_exec;
#[cfg(test)]
mod test_logs;
pub mod tls;
pub mod vaapi_probe;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::serve::Listener as _;
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
                encoder_app_path_display: e
                    .downcast_ref::<bootstrap::ValidatedFfmpegPath>()
                    .map(|validated| validated.path.clone()),
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
                capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
                chromaprint_muxer: false,
            }
        }
    }
}

/// Logs how far into the boot each stage of [`run`] finished.
///
/// Cold start is a sequence of a handful of coarse stages (bind, database +
/// encoder probe, manager wiring, seeding, router, ready), and without a
/// per-stage timestamp the only observable is the total — which says nothing
/// about which stage to attack. At `info`: a handful of lines per boot, and
/// while a long migration runs they are what tells an operator how far the
/// server has got.
fn boot_stage(started: std::time::Instant, stage: &'static str) {
    tracing::info!(
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
    discovery: Option<tokio::task::AbortHandle>,
) {
    let reason = shutdown_signal(shutdown_rx, &signals).await;
    if let Some(discovery) = discovery {
        discovery.abort();
    }
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
/// This is the whole composition root, in order: initialise logging, check the
/// server directories (Jellyfin's `MakeSanityCheckOrThrow`), open + migrate the
/// database, discover ffmpeg (non-fatal — the API still boots without
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
/// Returns an error if a server directory cannot be created or marked, or holds
/// another kind's marker, if the database cannot be opened/migrated, manager wiring
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
        match serve_once(&config, started, &mut metrics, &signals).await {
            Ok(true) => tracing::info!("restart requested — re-creating the host in-process"),
            Ok(false) => break,
            // The one place a failed boot or serve is logged, so it reaches the
            // log file as well as the exit status `main` reports.
            Err(error) => {
                tracing::error!(
                    error = format!("{error:#}"),
                    "ferrofin-server stopped on an error"
                );
                shutdown_tracing();
                return Err(error);
            }
        }
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

/// `StorageHelper.TestCommonPathsForStorageCapacity`: Jellyfin will not start,
/// nor apply a restore, without 2 GiB free on the data, cache and
/// program-data paths. It runs after the directory check
/// (`bootstrap::check_application_paths`, upstream's order), so all three
/// already exist. A failure stops the boot before the database is opened or a
/// restore is applied.
fn check_storage_capacity(config: &Config) -> anyhow::Result<()> {
    ferrofin_core::system_manager::test_common_paths_for_storage_capacity(
        &ferrofin_core::system_manager::FsStorageProbe,
        &config.data_dir.join("data"),
        &config.cache_dir,
        &config.data_dir,
    )
    .context("the storage capacity check failed")
}

/// [`apply_scheduled_restore`] on a blocking-pool thread. Synchronous on
/// purpose (nothing may touch the tree meanwhile, and the boot waits for it),
/// but a restore copies whole trees, which on the runtime's workers would
/// leave the startup server unable to answer.
async fn apply_scheduled_restore_off_runtime(
    config: &Config,
) -> anyhow::Result<Option<ferrofin_api::handlers::backup::RestoreFailure>> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || apply_scheduled_restore(&config))
        .await
        .context("the scheduled restore task failed")?
}

/// Applies a restore scheduled by `POST /Backup/Restore`, before the database
/// is opened. Returns the failure of a restore rejected before it wrote
/// anything (to be recorded once the activity log exists); a restore that
/// failed part-way aborts the boot.
///
/// First it runs [`check_storage_capacity`], whose failure aborts the boot, as
/// Jellyfin does: once per process (`Program.cs:138`), and again when a restore
/// is pending (`BackupService.RestoreBackupAsync`). An API restart with nothing
/// to restore is not re-checked, as Jellyfin's restart loop is not.
fn apply_scheduled_restore(
    config: &Config,
) -> anyhow::Result<Option<ferrofin_api::handlers::backup::RestoreFailure>> {
    static STORAGE_CHECKED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let roots = boot_tree_roots(config);
    if !STORAGE_CHECKED.swap(true, std::sync::atomic::Ordering::Relaxed)
        || ferrofin_api::handlers::backup::restore_pending(&roots)
    {
        check_storage_capacity(config)?;
    }
    Ok(
        match ferrofin_api::handlers::backup::apply_pending_restore(&roots) {
            Ok(Some(archive)) => {
                tracing::warn!(archive = %archive.display(), "restored backup");
                None
            }
            Ok(None) => None,
            // Extraction began and failed: the trees may be half replaced. Jellyfin
            // throws out of startup here; booting on them would let the web client
            // (which reads "the server answers again" as success) announce a
            // restore that did not happen.
            Err(failure) if failure.partially_applied => {
                return Err(anyhow::anyhow!(
                    "the scheduled backup restore failed part-way and the data directory may be \
                 partly restored: {failure}"
                ));
            }
            // Rejected before anything was written (missing or foreign archive):
            // the server boots on its untouched data and records the failure in the
            // activity log, where an administrator will see it.
            Err(failure) => {
                tracing::error!(error = %failure, "scheduled backup restore was rejected");
                Some(failure)
            }
        },
    )
}

/// Records a rejected restore in the activity log: the only durable,
/// administrator-visible trace of it.
async fn record_restore_failure(
    activity: &dyn ferrofin_traits::activity::ActivityManager,
    failure: &ferrofin_api::handlers::backup::RestoreFailure,
) {
    let entry = ferrofin_traits::activity::ActivityLogCreate {
        name: "Restoring the backup failed".to_owned(),
        type_: "BackupRestoreFailed".to_owned(),
        overview: Some(failure.to_string()),
        severity: ferrofin_model::activity::LogLevel::Error,
        ..Default::default()
    };
    if let Err(error) = activity.create_entry(entry).await {
        tracing::warn!(%error, "could not record the failed restore in the activity log");
    }
}

/// One server lifetime: boot, serve until drained, tear the host down, and
/// report whether an API restart was requested (`true` → the caller runs it
/// again). A signal-initiated drain always reports `false`.
// The boot is one ordered sequence (directories, restore, database, wiring,
// serve, drain), each step depending on the one before; split into helpers it
// would only re-thread the same state, as `build_app_state` notes.
#[allow(clippy::too_many_lines)]
async fn serve_once(
    config: &Config,
    started: std::time::Instant,
    metrics: &mut Option<ProcessMetrics>,
    signals: &Signals,
) -> anyhow::Result<bool> {
    // Process-wide SQLite setup must precede every open, the restore and the
    // existing-database check below included.
    ferrofin_db::init_sqlite();
    // Directories first (`Program.StartApp`), before a restore or an adopted database.
    bootstrap::check_application_paths(config)?;
    // A restore scheduled by `POST /Backup/Restore` (then restart) is applied
    // before the database file is opened — Jellyfin's `RestoreBackupPath`
    // sequence. It may bring back another `network.xml`, which decides the
    // addresses below and stays bound for the whole lifetime, so a pending
    // one runs before the bind; with nothing pending the storage check runs
    // after it.
    let restore_pending = ferrofin_api::handlers::backup::restore_pending(&boot_tree_roots(config));
    let early_restore = if restore_pending {
        Some(apply_scheduled_restore_off_runtime(config).await?)
    } else {
        None
    };
    // The startup server (`SetupServer`, which upstream starts before its
    // migrations): bind the HTTP port now and answer while the database is
    // adopted and migrated, so clients and probes get a 503 and a live
    // process, not a closed port. Leaving this function early drops
    // `serving`, which closes the port again. HTTPS waits for the wiring, as
    // upstream's setup server binds the HTTP port only.
    let (listeners, addresses, bound_port) = bind_http_listeners(config).await?;
    let ready = startup::Ready::default();
    let mut serving = Serving::start(
        listeners.into_iter().map(listen::ServedListener::Plain),
        &startup::dispatcher(Arc::clone(&ready)),
    );
    tracing::info!(addresses = ?addresses, "ferrofin-server listening, starting up");
    boot_stage(started, "listener bound");
    let restore_failure = match early_restore {
        Some(failure) => failure,
        None => apply_scheduled_restore_off_runtime(config).await?,
    };
    // A server that has been set up must not come up on a missing or empty
    // database: opening it would create and migrate a new one, and the seeding
    // below would give it a passwordless administrator. Checked after the
    // restore, which may have just brought the database back.
    existing_database::ensure_existing_database(config).await?;
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
    // `MediaEncoder.SetFFmpegPath`'s middle rung: the encoding configuration's
    // saved `EncoderAppPath`, between the command line and `$PATH`.
    let saved_encoder_path = bootstrap::persisted_encoder_app_path(&config.config_dir);
    // On a blocking-pool thread: adoption and the table-rebuilding migrations
    // copy the database file with blocking I/O, which on the runtime's workers
    // (one, on a one-CPU container) would leave the startup server unable to
    // answer `/health/live` for as long as the copy takes.
    let open = {
        let config = config.clone();
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || runtime.block_on(open_database(&config)))
    };
    let (db, ffmpeg, fpcalc) = tokio::join!(
        async {
            open.await
                .context("the database open task failed")
                .and_then(|opened| opened)
        },
        discover_ffmpeg(config, saved_encoder_path.as_deref()),
        ferrofin_extensions::fingerprint::discover_fpcalc_async(),
    );
    boot_stage(started, "db+encoder probe");
    let db = db?;

    // ffmpeg is required for playback but not for the server to boot: warn and
    // continue so the API comes up even on a host without ffmpeg installed
    // (a documented difference from Jellyfin, which aborts startup).
    let ffmpeg = encoder_or_disabled(ffmpeg);

    // Wire every concrete manager into the shared AppState (the composition root).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, config, &ffmpeg, fpcalc, shutdown_tx)
        .await
        .context("failed to assemble application state")?;
    boot_stage(started, "app state wired");
    if let Some(failure) = &restore_failure {
        record_restore_failure(wired.state.activity.as_ref(), failure).await;
    }

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
            let buckets = config.metrics_scan_duration_buckets.as_deref();
            enable_metrics_endpoint(&mut router, &wired.state, &db, interval, buckets, metrics)
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
    wired.app_host.set_bound_http_port(bound_port);
    let (https_listeners, https_addresses) = bind_https_listeners(config, &wired)?;
    if let Some(listen::ServedListener::Tls(listener)) = https_listeners.first() {
        wired
            .app_host
            .set_bound_https_port(listener.local_addr().port());
    }
    let discovery = start_discovery(&wired, config.discovery_bind_addr).await;
    let discovery_abort = discovery
        .as_ref()
        .map(tokio::task::JoinHandle::abort_handle);
    // The handover: the listeners serving the startup answers serve the
    // lifetime's router from the next request on, and HTTPS starts with it.
    let app = lifetime_app(router, &config.base_url, https_redirect(config, &wired));
    serving.serve(https_listeners, &app);
    // `ready` is this lifetime's own, so this is its only `set`.
    let _ = ready.set(app);
    boot_stage(started, "ready");
    tracing::info!(
        addresses = ?addresses,
        https_addresses = ?https_addresses,
        "ferrofin-server ready"
    );

    let serve_result = serving
        .drain(
            announce_shutdown(
                shutdown_rx,
                Arc::clone(&wired.state.sessions),
                signals.clone(),
                discovery_abort,
            ),
            config.shutdown_timeout_secs,
        )
        .await;

    // Tear the host down (Jellyfin: `using CoreAppHost` per iteration): stop
    // every background task, then close the pools so nothing from this lifetime
    // still holds the database file when the next one opens — or restores — it.
    let restart = wired.lifecycle.restart_requested() && !signals.fired();
    tear_down_host(wired, discovery, sampler).await;
    db.close().await;
    serve_result?;
    tracing::info!(
        uptime_s = started.elapsed().as_secs(),
        "ferrofin-server stopped"
    );
    Ok(restart)
}

/// Stops lifetime-owned services and scheduled runs before closing their database.
///
/// The library scan runs on its own worker, detached from every task: it is
/// stopped explicitly — the running scan cancelled, the queued ones dropped,
/// new ones refused — and awaited (at most one item's writes), so a restart
/// never cuts a scan mid-item with the pool closing under it, and a backup
/// restore (applied by the next lifetime, after this one's teardown) never
/// races a scan writing into the tree it replaces.
async fn tear_down_host(
    wired: WiredApp,
    discovery: Option<tokio::task::JoinHandle<()>>,
    sampler: Option<tokio::task::JoinHandle<()>>,
) {
    stop_background_tasks(discovery.into_iter().chain(sampler).collect()).await;
    cancel_running_tasks(wired.state.tasks.as_ref()).await;
    wired.state.library.shutdown_scans().await;
    stop_background_tasks(wired.background).await;
}

/// Binds discovery only for an enabled server lifetime. Failure leaves HTTP up.
async fn start_discovery(
    wired: &WiredApp,
    bind_addr: Option<SocketAddr>,
) -> Option<tokio::task::JoinHandle<()>> {
    let bind_addr = bind_addr.filter(|_| wired.discovery_enabled)?;
    let host: Arc<dyn ferrofin_traits::system::ServerApplicationHost> = wired.app_host.clone();
    match ferrofin_core::ServerDiscovery::bind(bind_addr, host, wired.server_id.clone()).await {
        Ok(discovery) => {
            tracing::info!(%bind_addr, "server discovery listening");
            Some(tokio::spawn(discovery.run()))
        }
        Err(error) => {
            tracing::error!(%error, %bind_addr, "unable to bind server discovery socket");
            None
        }
    }
}

/// Aborting requests cancellation; awaiting ensures sockets and other resources
/// have actually been dropped before the next in-process lifetime starts.
async fn stop_background_tasks(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::error!(%error, "background task failed during shutdown");
        }
    }
}

/// Serves `router` on `listeners` until `shutdown` resolves, then drains for
/// at most `timeout_secs`.
#[cfg(test)]
async fn serve_until_drained(
    listeners: Vec<listen::ServedListener>,
    router: axum::Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    timeout_secs: u32,
    base: &str,
    https_redirect: Option<Arc<tls::HttpsRedirect>>,
) -> anyhow::Result<()> {
    Serving::start(listeners, &lifetime_app(router, base, https_redirect))
        .drain(shutdown, timeout_secs)
        .await
}

/// The lifetime's router behind the path-case, HTTPS-redirect and base-URL
/// rewrites, which run before routing.
fn lifetime_app(
    router: axum::Router,
    base: &str,
    https_redirect: Option<Arc<tls::HttpsRedirect>>,
) -> axum::Router {
    let app = axum::middleware::from_fn(canonicalize_path_case).layer(router);
    // Inside the base URL mount, as Jellyfin's `UseHttpsRedirection` is.
    let app =
        axum::middleware::from_fn_with_state(https_redirect, tls::redirect_to_https).layer(app);
    let app = axum::middleware::from_fn_with_state(base_url::normalize(base), base_url::rewrite)
        .layer(app);
    axum::Router::new().fallback_service(app)
}

/// A lifetime's serve loops, one per listening address, all stopped by the
/// same trigger. Dropping it aborts them and closes the listeners.
struct Serving {
    servers: tokio::task::JoinSet<std::io::Result<()>>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Serving {
    /// Starts serving `app` on every listener.
    fn start(
        listeners: impl IntoIterator<Item = listen::ServedListener>,
        app: &axum::Router,
    ) -> Self {
        let (stop, _) = tokio::sync::watch::channel(false);
        let mut serving = Self {
            servers: tokio::task::JoinSet::new(),
            stop,
        };
        serving.serve(listeners, app);
        serving
    }

    /// Serves `app` on more listeners, stopped by the same trigger.
    fn serve(
        &mut self,
        listeners: impl IntoIterator<Item = listen::ServedListener>,
        app: &axum::Router,
    ) {
        for listener in listeners {
            let mut stop = self.stop.subscribe();
            let app = app.clone();
            self.servers.spawn(async move {
                // Each connection's peer (`ConnectInfo`, which handlers read, e.g.
                // `GET /System/Endpoint`) and the address it was accepted on, which
                // the IP gate compares with it (C# `IsLocal()`).
                let make_service = tower::service_fn(
                    move |stream: axum::serve::IncomingStream<'_, listen::ServedListener>| {
                        let peer = *stream.remote_addr();
                        let local = stream.io().local_addr().ok();
                        let transport = stream.io().transport();
                        let app = app.clone();
                        async move {
                            let service = tower::ServiceBuilder::new()
                                .map_request(move |mut request: axum::extract::Request| {
                                    let extensions = request.extensions_mut();
                                    extensions.insert(axum::extract::ConnectInfo(peer));
                                    extensions.insert(transport);
                                    if let Some(local) = local {
                                        extensions.insert(
                                            ferrofin_api::ip_access::ConnectionLocalAddr(local),
                                        );
                                    }
                                    request
                                })
                                .service(app);
                            Ok::<_, std::convert::Infallible>(service)
                        }
                    },
                );
                axum::serve(listener, make_service)
                    .with_graceful_shutdown(async move {
                        let _ = stop.wait_for(|stopped| *stopped).await;
                    })
                    .await
            });
        }
    }

    /// Serves until the shutdown trigger fires and the drain completes — or
    /// `timeout_secs` after the trigger, whichever comes first (Kestrel's
    /// `HostOptions.ShutdownTimeout`). Unbounded, a client mid-stream would keep
    /// the old host alive — listener already closed — for as long as it kept
    /// reading, so a restart during playback never came back. Dropping the serve
    /// future past the deadline aborts the remaining connections.
    async fn drain(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
        timeout_secs: u32,
    ) -> anyhow::Result<()> {
        let Self { mut servers, stop } = self;
        let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            shutdown.await;
            let _ = stop.send(true);
            let _ = drain_started_tx.send(());
        });
        let deadline = async move {
            let _ = drain_started_rx.await;
            tokio::time::sleep(std::time::Duration::from_secs(u64::from(timeout_secs))).await;
        };
        let serve = async {
            while let Some(finished) = servers.join_next().await {
                finished
                    .context("a listener task failed")?
                    .context("server error")?;
            }
            Ok(())
        };
        tokio::select! {
            result = serve => result,
            () = deadline => {
                tracing::warn!(
                    timeout_s = timeout_secs,
                    "shutdown timeout elapsed — aborting the remaining connections"
                );
                Ok(())
            }
        }
    }
}

/// Plans and binds this lifetime's HTTP listeners, returning them with the
/// addresses they were planned on and the port clients are told about (the
/// first listener's; every address shares it).
async fn bind_http_listeners(
    config: &Config,
) -> anyhow::Result<(Vec<listen::Listener>, Vec<SocketAddr>, u16)> {
    // The same saved network settings `build_app_state` gives the shared
    // network manager, read before it exists.
    let paths = bootstrap::application_paths(config);
    let network =
        ferrofin_networking::NetworkManager::live(state::load_network_configuration(&paths).await);
    let addresses = listen_addresses(config, Some(&network), config.port);
    anyhow::ensure!(
        !addresses.is_empty(),
        "the network settings leave no address to listen on (IPv4 and IPv6 are both disabled)"
    );
    let listeners = listen::bind_all(&addresses)?;
    let port = listeners
        .first()
        .context("no listener was bound")?
        .local_addr()?
        .port();
    Ok((listeners, addresses, port))
}

/// The HTTPS redirect this lifetime serves: Jellyfin adds
/// `UseHttpsRedirection` when `RequireHttps` is set and it listens with HTTPS.
/// It points at the bound HTTPS port, so it follows the listeners.
fn https_redirect(config: &Config, wired: &WiredApp) -> Option<Arc<tls::HttpsRedirect>> {
    use ferrofin_traits::system::ServerApplicationHost as _;
    wired.https.as_ref().filter(|https| https.require).map(|_| {
        Arc::new(tls::HttpsRedirect {
            port: wired.app_host.https_port(),
            base: base_url::normalize(&config.base_url),
            network: wired.state.network.clone(),
            config: Arc::clone(&wired.state.config),
        })
    })
}

/// Binds this lifetime's HTTPS listeners, when the server listens with HTTPS:
/// Kestrel's `UseHttps` listener on the HTTPS port of every address the HTTP
/// port is bound on (`SetupJellyfinWebServer`). Returns them with the
/// addresses they were planned on.
fn bind_https_listeners(
    config: &Config,
    wired: &WiredApp,
) -> anyhow::Result<(Vec<listen::ServedListener>, Vec<SocketAddr>)> {
    let Some(https) = wired.https.as_ref() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let network = wired
        .state
        .network
        .as_ref()
        .and_then(|network| network.read().ok());
    let addresses = listen_addresses(config, network.as_deref(), config.https_port);
    drop(network);
    let listeners = listen::bind_all(&addresses)
        .context("failed to bind the HTTPS listeners (FERROFIN_HTTPS_PORT)")?
        .into_iter()
        .map(|listener| {
            tls::TlsListener::new(listener, Arc::clone(&https.config))
                .map(listen::ServedListener::Tls)
                .context("failed to start an HTTPS listener")
        })
        .collect::<anyhow::Result<_>>()?;
    Ok((listeners, addresses))
}

/// The addresses this lifetime listens on, on `port`: the operator's startup
/// bind when they pinned one, otherwise Jellyfin's `GetAllBindInterfaces` over
/// the saved network settings (`LocalNetworkAddresses`, `EnableIPv4`/`EnableIPv6`).
fn listen_addresses(
    config: &Config,
    network: Option<&ferrofin_networking::NetworkManager>,
    port: u16,
) -> Vec<SocketAddr> {
    let interfaces: Vec<listen::BindInterface> = network
        .map(|network| {
            network
                .get_all_bind_interfaces(false)
                .into_iter()
                .map(|interface| listen::BindInterface {
                    address: interface.address,
                    index: u32::try_from(interface.index).unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    listen::plan_addresses(
        config.bind_addr_explicit.then_some(config.bind_addr),
        port,
        &interfaces,
    )
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
    if let Some(metadata) =
        ferrofin_api::handlers::backup::restored_metadata_root(&config.config_dir)
    {
        roots.metadata = metadata;
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
pub(crate) fn disable_nagle(stream: &mut tokio::net::TcpStream) {
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
    scan_duration_buckets: Option<&[f64]>,
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
                metrics_wiring::install_subsystem_instruments(scan_duration_buckets);
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

/// Rewrites a request's path to its canonical Jellyfin case before routing
/// (the query string is passed through unchanged).
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
    // return `None` and keep their significant case). Query keys are left as
    // sent: `ferrofin_api::extract::Query` binds them to each handler's members
    // ignoring case, as ASP.NET does. Preserve the original query for
    // authentication, streaming URLs and plugin forwarding.
    // Jellyfin's `/health` is an ASP.NET route too, so its case is not
    // significant either, unlike Ferrofin's own `/health/live` and `/health/ready`.
    let path = request.uri().path();
    let canonical = if path.eq_ignore_ascii_case("/health") || path.eq_ignore_ascii_case("/health/")
    {
        Some(path.to_ascii_lowercase())
    } else {
        ferrofin_api::routes::canonicalize_path(path)
    };
    if let Some(new_path) = canonical
        && new_path != path
    {
        let target = match request.uri().query() {
            Some(q) => format!("{new_path}?{q}"),
            None => new_path,
        };
        // Origin-form request URI (path + query, no scheme/authority), so
        // rebuilding it from the parts is lossless.
        if let Ok(uri) = target.parse() {
            *request.uri_mut() = uri;
        }
    }
    next.run(request).await
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

    /// Every connection carries its peer (`ConnectInfo`, which handlers read)
    /// and the address it was accepted on (`ConnectionLocalAddr`, which the IP
    /// gate compares with it — C# `IsLocal()`).
    #[tokio::test]
    async fn each_connection_carries_its_peer_and_local_address() {
        use axum::extract::ConnectInfo;
        use ferrofin_api::ip_access::ConnectionLocalAddr;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = super::listen::bind("127.0.0.1:0".parse().expect("address")).expect("bind");
        let address = axum::serve::Listener::local_addr(&listener).expect("local address");
        let router = Router::new().route(
            "/probe",
            axum::routing::get(
                |ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
                 axum::Extension(ConnectionLocalAddr(local)): axum::Extension<
                    ConnectionLocalAddr,
                >| async move { format!("peer={} local={local}", peer.ip()) },
            ),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(super::serve_until_drained(
            vec![crate::listen::ServedListener::Plain(listener)],
            router,
            async move {
                let _ = stopped.await;
            },
            1,
            "",
            None,
        ));

        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        stream
            .write_all(b"GET /probe HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .expect("request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response");
        let _ = stop.send(());
        server.await.expect("join").expect("serve");

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(
            response.ends_with(&format!("peer=127.0.0.1 local={address}")),
            "{response}"
        );
    }

    #[test]
    fn disabled_encoder_retains_only_a_successfully_validated_ffmpeg_display() {
        let validated = std::path::PathBuf::from("/owned/ffmpeg");
        let partial = anyhow::anyhow!("ffprobe validation failed").context(
            super::bootstrap::ValidatedFfmpegPath {
                path: validated.clone(),
            },
        );
        let disabled = super::encoder_or_disabled(Err(partial));
        assert_eq!(disabled.encoder_app_path_display, Some(validated));
        assert_eq!(disabled.ffmpeg, std::path::PathBuf::from("ffmpeg"));
        assert_eq!(disabled.ffprobe, std::path::PathBuf::from("ffprobe"));
        assert!(!disabled.chromaprint_muxer);
        let invalid = super::encoder_or_disabled(Err(anyhow::anyhow!("ffmpeg validation failed")));
        assert_eq!(invalid.encoder_app_path_display, None);
    }

    #[tokio::test]
    async fn background_teardown_releases_sockets_before_returning() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = socket.local_addr().expect("address");
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(socket);
        });
        super::stop_background_tasks(vec![task]).await;
        // A child another test forks at this moment holds a copy of every
        // open socket until it execs, so the port may stay taken for a few
        // milliseconds after this process closed it; one never released
        // stays taken.
        let mut rebound = tokio::net::UdpSocket::bind(address).await;
        for _ in 0..100 {
            if rebound.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            rebound = tokio::net::UdpSocket::bind(address).await;
        }
        drop(rebound.expect("released before return"));
    }

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

    /// A host reached locally at an address nothing listens on: these tests
    /// register no relative endpoint.
    struct NoLocalApi;
    #[async_trait::async_trait]
    impl ferrofin_traits::system::LocalApiAccess for NoLocalApi {
        async fn get_api_url_for_local_access(
            &self,
        ) -> Result<String, ferrofin_traits::error::ServiceError> {
            Ok("http://127.0.0.1:0".to_owned())
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
                Arc::new(NoLocalApi),
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

    /// The listener `run` binds really clears Nagle on the sockets it serves.
    ///
    /// Discriminating: a plain `TcpListener` accepts with `TCP_NODELAY` **off**
    /// (asserted first, so the test fails loudly if the platform ever changes
    /// that premise and makes the assertion below vacuous), and `axum::serve`
    /// adds nothing of its own. Drop the `.tap_io(disable_nagle)` from
    /// [`listen::bind`] — or stub `disable_nagle` out — and the second
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
        let mut listener = super::listen::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
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
