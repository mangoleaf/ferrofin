//! Startup side-effects for the Ferrofin server — port of the `Program.Main` /
//! `Startup` bring-up in `Jellyfin.Server`: initialise logging, open + migrate
//! the SQLite database, and discover + validate the ffmpeg / ffprobe binaries.
//!
//! Each helper is independently callable so the composition root (and tests)
//! can sequence them explicitly. The functions here own the *effects*; the
//! pure parsing they rely on lives in
//! [`ferrofin_mediaencoding::encoder::encoder_validator::EncoderValidator`].

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::Context as _;
use ferrofin_db::Database;
use ferrofin_mediaencoding::encoder::EncoderValidator;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// The OTLP tracer provider, kept alive for its background batch exporter and so
/// [`shutdown_tracing`] can flush it on graceful shutdown. `None` (unset) unless
/// `OTEL_EXPORTER_OTLP_ENDPOINT` was present at startup.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// The resolved, validated media-encoding tool paths.
///
/// Returned by [`discover_ffmpeg`] once both binaries pass a `-version` smoke
/// check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegPaths {
    /// The validated `ffmpeg` executable path.
    pub ffmpeg: PathBuf,
    /// The validated `ffprobe` executable path.
    pub ffprobe: PathBuf,
    /// The filter names the validated ffmpeg reported via `-filters` (empty
    /// when the probe failed). Gates capability-specific arguments like the
    /// jellyfin-ffmpeg-only `tonemapx` software tonemap.
    pub filters: Vec<String>,
    /// The encoder names the validated ffmpeg reported via `-encoders` (empty
    /// when the probe failed). Gates encoder selection like preferring
    /// `libfdk_aac` over native `aac`.
    pub encoders: Vec<String>,
}

impl FfmpegPaths {
    /// Whether the validated ffmpeg reports the filter `name`. Port of
    /// `IMediaEncoder.SupportsFilter`.
    #[must_use]
    pub fn supports_filter(&self, name: &str) -> bool {
        self.filters.iter().any(|f| f == name)
    }

    /// Whether the validated ffmpeg reports the encoder `name`. Port of
    /// `IMediaEncoder.SupportsEncoder`.
    #[must_use]
    pub fn supports_encoder(&self, name: &str) -> bool {
        self.encoders.iter().any(|e| e == name)
    }
}

/// Initialises the global `tracing` subscriber from the configured log filter.
///
/// The `FERROFIN_LOG` value (already resolved into [`Config::log_level`]) is used
/// as the default `EnvFilter` directive; an explicit `RUST_LOG` in the
/// environment still takes precedence, matching the standard `tracing`
/// convention. Safe to call once at startup; a second call is a no-op because
/// the global subscriber can only be set once.
///
/// Output is written to **both** stdout (for `kubectl logs` / journald) and a
/// daily-rotating file `log_YYYY-MM-DD.log` under `{data_dir}/log` — the same
/// directory `GET /System/Logs` serves, so server logs appear in the Jellyfin
/// dashboard log viewer, matching Jellyfin's Serilog file sink. If the log
/// directory can't be prepared, logging falls back to stdout only.
///
/// Old log files are pruned to `LogFileRetentionDays` (read best-effort from
/// `{config_dir}/system.json`; `0`/negative = keep forever), and the file writer
/// is non-blocking so slow disk I/O never stalls a request thread.
///
/// Returns the [`WorkerGuard`] for the non-blocking file writer (`None` when no
/// file sink was created). **The caller must hold it until after the server
/// drains** — dropping it flushes the queue; dropping it early silently discards
/// the last buffered log lines. Also installs a panic hook so panics reach the
/// log stream instead of vanishing.
#[must_use]
pub fn init_tracing(config: &Config) -> Option<WorkerGuard> {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    install_panic_hook();

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Log dir mirrors the composition root's `{data_dir}/log` (state.rs). The
    // rolling appender needs it to exist up front. Retention prunes old
    // `log_*.log` files on rotation (the whole feature — no custom pruner).
    let retention_days = read_retention_days(&config.config_dir);
    let log_dir = config.data_dir.join("log");
    let file_appender = std::fs::create_dir_all(&log_dir).ok().and_then(|()| {
        let mut builder = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("log")
            .filename_suffix("log");
        if let Ok(keep) = usize::try_from(retention_days)
            && keep > 0
        {
            builder = builder.max_log_files(keep);
        }
        builder.build(&log_dir).ok()
    });

    // Non-blocking file writer: a background thread owns the disk I/O so a slow or
    // full disk never blocks a logging call site. The returned guard flushes the
    // queue on drop — the caller holds it until after `axum::serve` drains.
    let (file_writer, guard) = match file_appender {
        Some(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (Some(writer), Some(guard))
        }
        None => (None, None),
    };

    // JSON stdout by default (Alloy pod-log scrape → Loki, carrying the request
    // span's `trace_id` on every in-span line); `FERROFIN_LOG_FORMAT=text` restores
    // the legacy tee'd human-readable output for interactive dev. The rotating
    // FILE is ALWAYS plain text regardless — `GET /System/Logs` feeds the Jellyfin
    // dashboard log viewer, which renders raw lines (a parity surface).
    let text_mode =
        std::env::var("FERROFIN_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("text"));
    let mut layers = build_fmt_layers(text_mode, file_writer);

    // Compose the OTLP layer in only when export is configured; `None` is a no-op
    // (`Option<Layer>` implements `Layer`), so tracing stays local-only otherwise.
    if let Some(otel_layer) = build_tracer_provider().map(|provider| {
        let tracer = provider.tracer("ferrofin");
        // Stash the provider so `shutdown_tracing` can flush the batch queue.
        let _ = TRACER_PROVIDER.set(provider);
        tracing_opentelemetry::layer().with_tracer(tracer)
    }) {
        layers.push(otel_layer.boxed());
    }

    // `try_init` returns Err if a subscriber is already set (e.g. in tests); that
    // is fine — we only need one. The filter is applied as the outermost layer so
    // it gates every sink at once.
    let _ = tracing_subscriber::registry()
        .with(layers)
        .with(filter)
        .try_init();

    guard
}

/// Type alias for the boxed, subscriber-erased log layers `init_tracing` composes.
type BoxedLayer = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// Builds the fmt sink layers for the chosen format + file writer.
///
/// Default (`text_mode = false`): JSON to stdout + plain text to the file. Text
/// mode: a single human-readable layer tee'd to stdout + file. The file sink is
/// always plain text (dashboard parity); when no file writer exists, only stdout
/// is written.
fn build_fmt_layers(
    text_mode: bool,
    file_writer: Option<tracing_appender::non_blocking::NonBlocking>,
) -> Vec<BoxedLayer> {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::fmt::writer::MakeWriterExt as _;

    let mut layers: Vec<BoxedLayer> = Vec::new();
    match (text_mode, file_writer) {
        // Legacy path: one text layer tee'd to stdout + file. ANSI off so neither
        // sink carries colour escapes (the dashboard renders raw text).
        (true, Some(writer)) => layers.push(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(std::io::stdout.and(writer))
                .boxed(),
        ),
        // Legacy fallback when the log dir can't be prepared: stdout-only text,
        // ANSI left at its auto default (matches the pre-refactor behaviour).
        (true, None) => layers.push(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stdout)
                .boxed(),
        ),
        // Default: JSON to stdout (structured, `trace_id`-carrying) + plain text
        // to the file only.
        (false, Some(writer)) => {
            layers.push(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_target(true)
                    .with_writer(std::io::stdout)
                    .boxed(),
            );
            layers.push(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_ansi(false)
                    .with_writer(writer)
                    .boxed(),
            );
        }
        (false, None) => layers.push(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(true)
                .with_writer(std::io::stdout)
                .boxed(),
        ),
    }
    layers
}

/// Reads `LogFileRetentionDays` from `{config_dir}/system.json`, best-effort.
///
/// The config manager isn't constructed this early, so this is a raw
/// `serde_json::Value` read. A missing file, parse error, absent field, or
/// out-of-range number all fall back to the Jellyfin default of `3`. A present
/// value (including `0` or negative — "keep forever") is used verbatim; the
/// caller treats `<= 0` as unlimited retention.
fn read_retention_days(config_dir: &Path) -> i32 {
    const DEFAULT_RETENTION_DAYS: i32 = 3;
    std::fs::read_to_string(config_dir.join("system.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|cfg| {
            cfg.get("LogFileRetentionDays")
                .and_then(serde_json::Value::as_i64)
        })
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Installs a panic hook (once) that logs the panic through `tracing` before
/// delegating to the previous hook.
///
/// Without this, a panic bypasses tracing entirely — it never reaches the JSON
/// stdout stream, the log file, or (with export on) the active span in Tempo.
/// The previous hook is preserved so abort/backtrace behaviour is unchanged.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map_or_else(String::new, ToString::to_string);
            tracing::error!(panic = %info, location = %location, "panicked");
            previous(info);
        }));
    });
}

/// Builds the OTLP tracer provider when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
///
/// Returns `None` (tracing stays local-only) when the endpoint env var is unset
/// or empty, or on any exporter-init error — the server must start regardless,
/// the same posture as metrics init. Traces are the ONLY signal carried on OTLP;
/// metrics stay on the Prometheus scrape and logs on stdout/file.
fn build_tracer_provider() -> Option<SdkTracerProvider> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let ratio = sample_ratio(std::env::var("OTEL_TRACES_SAMPLER_ARG").ok().as_deref());

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.clone())
        .build()
    {
        Ok(exporter) => exporter,
        Err(e) => {
            tracing::warn!(error = %e, %endpoint, "OTLP exporter init failed; traces disabled");
            return None;
        }
    };

    let resource = Resource::builder()
        .with_service_name("ferrofin")
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    // ParentBased(TraceIdRatioBased): honour an upstream sampling decision when
    // one arrives, else sample `ratio` of new roots. Batch (never simple) exporter
    // so span export never blocks a request thread.
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            ratio,
        ))))
        .with_resource(resource)
        .build();
    tracing::info!(%endpoint, ratio, "OTLP trace export enabled");
    Some(provider)
}

/// Parses `OTEL_TRACES_SAMPLER_ARG` into a sampling ratio in `0.0..=1.0`.
///
/// Unset, unparseable, non-finite, or out-of-range input falls back to the fleet
/// default of `0.25`; in-range values are used as-is and anything outside is
/// clamped. Sampling is the storage lever, so the default is conservative.
fn sample_ratio(env_val: Option<&str>) -> f64 {
    const DEFAULT_RATIO: f64 = 0.25;
    env_val
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|r| r.is_finite())
        .map_or(DEFAULT_RATIO, |r| r.clamp(0.0, 1.0))
}

/// Flushes and shuts down the OTLP tracer provider, if one was started.
///
/// Called after `axum::serve` drains so the last batch of spans is exported
/// rather than silently dropped. A no-op when trace export is disabled.
pub fn shutdown_tracing() {
    if let Some(provider) = TRACER_PROVIDER.get()
        && let Err(e) = provider.shutdown()
    {
        tracing::warn!(error = %e, "tracer provider shutdown failed");
    }
}

/// Opens the SQLite database at `{data_dir}/ferrofin.db` and applies migrations.
///
/// Creates the data directory (and the DB file) if missing, then runs all
/// pending migrations to bring the schema to head. Mirrors the
/// `JellyfinDbContext` migrate-on-startup step.
///
/// # Errors
///
/// Returns an error if the data directory cannot be created, the pool cannot be
/// opened, or a migration fails.
pub async fn open_database(config: &Config) -> anyhow::Result<Database> {
    std::fs::create_dir_all(&config.data_dir).with_context(|| {
        format!(
            "failed to create data directory `{}`",
            config.data_dir.display()
        )
    })?;

    let url = config.database_url();
    let db = Database::connect_sized(&url, config.db_pool)
        .await
        .with_context(|| format!("failed to open database `{url}`"))?;
    db.run_migrations()
        .await
        .context("failed to apply database migrations")?;
    // Playlist/collection membership lives in BaseItems.Data JSON (Jellyfin's
    // 10.11.8 storage); reconcile it with Ferrofin's derived cache tables —
    // imports Jellyfin-written blobs, backfills blobs for pre-Data Ferrofin rows.
    ferrofin_core::item_data::reconcile_container_data(&db)
        .await
        .context("failed to reconcile playlist/collection Data JSON")?;
    tracing::info!(database = %config.database_path().display(), "database ready");
    spawn_pool_sampler(&db);
    Ok(db)
}

/// Samples the connection pool every 5 s at `debug` level (`RUST_LOG=
/// ferrofin_server=debug`): total connections, idle connections, and — the
/// contention signal — how many are checked out. Under load, `in_use` pinned at
/// the pool cap means requests are queueing on connection acquisition, not on
/// query work (the diagnosis behind the pool-size default; see
/// `suite/perf/pool-sweep.sh`).
fn spawn_pool_sampler(db: &Database) {
    let pool = db.pool().clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            let size = pool.size();
            let idle = pool.num_idle();
            tracing::debug!(
                size,
                idle,
                in_use = size.saturating_sub(u32::try_from(idle).unwrap_or(u32::MAX)),
                "db_pool"
            );
        }
    });
}

/// Discovers and validates the ffmpeg / ffprobe executables. Port of
/// `MediaEncoder.SetFFmpegPath`.
///
/// Resolution order for the ffmpeg path:
/// 1. `config.ffmpeg_path` (`--ffmpeg` / `FERROFIN_FFMPEG_PATH`);
/// 2. `system_encoder_path` — the persisted `system.json` `EncoderAppPath`, if
///    the caller has one;
/// 3. `ffmpeg` resolved on `$PATH`.
///
/// The chosen candidate is validated by running `<candidate> -version` and
/// requiring exit `0` plus output beginning `ffmpeg version` (a First-Light
/// smoke check — full decoder/encoder/hwaccel enumeration is deferred). The
/// ffprobe path is `config.ffprobe_path` if set, else the ffmpeg basename with
/// `ffmpeg` replaced by `ffprobe`; it is `-version`-validated the same way.
///
/// # Errors
///
/// Returns an error if no candidate can be found, or if a found candidate fails
/// the version smoke check. The caller decides whether that aborts startup
/// (transcoding will 500 without a working ffmpeg) or merely disables playback.
pub async fn discover_ffmpeg(
    config: &Config,
    system_encoder_path: Option<&str>,
) -> anyhow::Result<FfmpegPaths> {
    let (ffmpeg, method) = resolve_ffmpeg_candidate(config, system_encoder_path)?;
    validate_binary(&ffmpeg, "ffmpeg")
        .await
        .with_context(|| format!("ffmpeg validation failed (resolved via {method})"))?;

    let ffprobe = match &config.ffprobe_path {
        Some(p) => p.clone(),
        None => derive_ffprobe_path(&ffmpeg),
    };
    validate_binary(&ffprobe, "ffprobe")
        .await
        .with_context(|| {
            format!(
                "ffprobe validation failed (derived from `{}`)",
                ffmpeg.display()
            )
        })?;

    let filters = probe_list(&ffmpeg, "-filters", EncoderValidator::get_filters_internal).await;
    let encoders = probe_list(&ffmpeg, "-encoders", EncoderValidator::get_codecs_internal).await;
    tracing::info!(
        ffmpeg = %ffmpeg.display(),
        ffprobe = %ffprobe.display(),
        filters = filters.len(),
        encoders = encoders.len(),
        tonemapx = filters.iter().any(|f| f == "tonemapx"),
        libfdk_aac = encoders.iter().any(|e| e == "libfdk_aac"),
        "media encoder ready"
    );
    Ok(FfmpegPaths {
        ffmpeg,
        ffprobe,
        filters,
        encoders,
    })
}

/// Captures `ffmpeg <flag>` (`-filters` / `-encoders`) and parses the names.
///
/// Port of `EncoderValidator.GetFFmpegFilters` / `GetCodecs` (the process
/// half; the parses are the `EncoderValidator::get_*_internal` pure fns). A
/// probe failure is not fatal — capability-gated arguments are simply skipped —
/// so errors log and return an empty list, matching the C#
/// catch-and-return-empty.
async fn probe_list(ffmpeg: &Path, flag: &str, parse: fn(&str) -> Vec<String>) -> Vec<String> {
    let output = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", flag])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => parse(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => {
            tracing::warn!(status = %out.status, flag, "ffmpeg capability probe failed; assuming none");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, flag, "error running ffmpeg capability probe");
            Vec::new()
        }
    }
}

/// Picks the ffmpeg candidate path and a human label for how it was found,
/// without touching the filesystem beyond a `$PATH` lookup.
fn resolve_ffmpeg_candidate(
    config: &Config,
    system_encoder_path: Option<&str>,
) -> anyhow::Result<(PathBuf, &'static str)> {
    if let Some(explicit) = &config.ffmpeg_path {
        return Ok((explicit.clone(), "config (--ffmpeg / FERROFIN_FFMPEG_PATH)"));
    }
    if let Some(system) = system_encoder_path.filter(|s| !s.is_empty()) {
        return Ok((PathBuf::from(system), "system.json EncoderAppPath"));
    }
    if let Some(found) = which_on_path("ffmpeg") {
        return Ok((found, "$PATH"));
    }
    anyhow::bail!(
        "no ffmpeg found: set --ffmpeg / FERROFIN_FFMPEG_PATH, configure system.json \
         EncoderAppPath, or install ffmpeg on $PATH"
    )
}

/// Derives the ffprobe path from a resolved ffmpeg path by replacing `ffmpeg`
/// with `ffprobe` in the final path component (parity with Jellyfin's
/// `FfprobePathRegex`). If the file name does not contain `ffmpeg`, the sibling
/// `ffprobe` is used.
fn derive_ffprobe_path(ffmpeg: &Path) -> PathBuf {
    let file_name = ffmpeg
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ffmpeg");
    let probe_name = if file_name.contains("ffmpeg") {
        file_name.replace("ffmpeg", "ffprobe")
    } else {
        "ffprobe".to_owned()
    };
    match ffmpeg.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(probe_name),
        _ => PathBuf::from(probe_name),
    }
}

/// Looks up `program` on the process `$PATH`, returning the first
/// executable-file match.
///
/// A dependency-free stand-in for `which::which` (not a workspace dep). Absolute
/// or relative paths containing a separator are returned as-is if they exist.
fn which_on_path(program: &str) -> Option<PathBuf> {
    which_in(program, std::env::var_os("PATH").as_deref())
}

/// [`which_on_path`] against an explicit `PATH` value, so the search is
/// unit-testable without mutating the process environment.
fn which_in(program: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let paths = path_var?;
    std::env::split_paths(paths).find_map(|dir| {
        let candidate = dir.join(program);
        candidate.is_file().then_some(candidate)
    })
}

/// Runs `<binary> -version` and asserts it is a genuine ffmpeg-family tool.
///
/// Requires exit status `0` and stdout beginning with `<expected_prefix> version`
/// (e.g. `ffmpeg version` / `ffprobe version`), matching the `EncoderValidator`
/// smoke-check contract. The captured output is additionally run through
/// [`EncoderValidator::validate_version_internal`] so an out-of-range or avconv
/// build is rejected the same way the runtime validator would.
///
/// # Errors
///
/// Returns an error if the process cannot be spawned, exits non-zero, or its
/// output is not recognised as a supported ffmpeg-family version.
async fn validate_binary(binary: &Path, expected_prefix: &str) -> anyhow::Result<()> {
    let output = tokio::process::Command::new(binary)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to spawn `{} -version`", binary.display()))?;

    anyhow::ensure!(
        output.status.success(),
        "`{} -version` exited with {}",
        binary.display(),
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let banner = format!("{expected_prefix} version");
    anyhow::ensure!(
        stdout.trim_start().starts_with(&banner),
        "`{} -version` output did not start with `{banner}`",
        binary.display()
    );

    // ffprobe shares ffmpeg's version banner, so validate both against the
    // ffmpeg version range using the shared pure validator.
    let validator = EncoderValidator::new(binary.to_string_lossy().into_owned());
    anyhow::ensure!(
        validator.validate_version_internal(&stdout),
        "`{}` reported an unsupported ffmpeg version (need >= {})",
        binary.display(),
        ferrofin_mediaencoding::encoder::MIN_VERSION
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_ffprobe_replaces_basename_in_dir() {
        let probe = derive_ffprobe_path(Path::new("/usr/bin/ffmpeg"));
        assert_eq!(probe, PathBuf::from("/usr/bin/ffprobe"));
    }

    #[test]
    fn derive_ffprobe_handles_suffixed_basename() {
        // A versioned/wrapped name still gets the substring replaced.
        let probe = derive_ffprobe_path(Path::new("/opt/ffmpeg/bin/ffmpeg-6.1"));
        assert_eq!(probe, PathBuf::from("/opt/ffmpeg/bin/ffprobe-6.1"));
    }

    #[test]
    fn derive_ffprobe_bare_name_has_no_dir() {
        let probe = derive_ffprobe_path(Path::new("ffmpeg"));
        assert_eq!(probe, PathBuf::from("ffprobe"));
    }

    #[test]
    fn derive_ffprobe_unusual_name_falls_back_to_sibling() {
        let probe = derive_ffprobe_path(Path::new("/usr/local/bin/avc"));
        assert_eq!(probe, PathBuf::from("/usr/local/bin/ffprobe"));
    }

    #[test]
    fn explicit_config_ffmpeg_path_is_preferred() {
        let cfg = config_with_ffmpeg(Some("/custom/ffmpeg"));
        let (path, method) = resolve_ffmpeg_candidate(&cfg, Some("/sys/ffmpeg")).unwrap();
        assert_eq!(path, PathBuf::from("/custom/ffmpeg"));
        assert!(method.contains("config"));
    }

    #[test]
    fn system_encoder_path_used_when_no_explicit() {
        let cfg = config_with_ffmpeg(None);
        let (path, method) = resolve_ffmpeg_candidate(&cfg, Some("/sys/ffmpeg")).unwrap();
        assert_eq!(path, PathBuf::from("/sys/ffmpeg"));
        assert!(method.contains("system.json"));
    }

    #[test]
    fn empty_system_encoder_path_is_ignored() {
        // With no explicit path, no system path, and (in the test sandbox) no
        // ffmpeg on PATH, resolution should fail loudly.
        let cfg = config_with_ffmpeg(None);
        let result = resolve_ffmpeg_candidate(&cfg, Some(""));
        // Either PATH has ffmpeg (found) or it errors — but it must not pick "".
        if let Ok((path, _)) = result {
            assert_ne!(path, PathBuf::from(""));
        }
    }

    #[tokio::test]
    async fn validate_binary_rejects_nonexistent() {
        let err = validate_binary(Path::new("/nonexistent/ffmpeg-xyz"), "ffmpeg")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed to spawn"));
    }

    #[tokio::test]
    async fn validate_binary_rejects_non_ffmpeg_tool() {
        // `true` exits 0 but prints no ffmpeg banner.
        if let Some(true_bin) = which_on_path("true") {
            let err = validate_binary(&true_bin, "ffmpeg").await.unwrap_err();
            assert!(err.to_string().contains("did not start with"));
        }
    }

    #[test]
    fn which_on_path_returns_absolute_path_verbatim_when_present() {
        // An input already containing a separator is returned as-is if it exists.
        let sh = which_on_path("sh").expect("sh must be on PATH in the test env");
        let again = which_on_path(sh.to_str().unwrap());
        assert_eq!(again, Some(sh));
    }

    #[test]
    fn which_on_path_absolute_missing_is_none() {
        assert_eq!(which_on_path("/definitely/not/here/nope"), None);
    }

    #[test]
    fn sample_ratio_parses_clamps_and_defaults() {
        assert!(
            (sample_ratio(None) - 0.25).abs() < f64::EPSILON,
            "unset → default"
        );
        assert!(
            (sample_ratio(Some("garbage")) - 0.25).abs() < f64::EPSILON,
            "junk → default"
        );
        assert!(
            (sample_ratio(Some("")) - 0.25).abs() < f64::EPSILON,
            "empty → default"
        );
        assert!(
            (sample_ratio(Some("nan")) - 0.25).abs() < f64::EPSILON,
            "nan → default"
        );
        assert!(
            (sample_ratio(Some(" 0.5 ")) - 0.5).abs() < f64::EPSILON,
            "trimmed + parsed"
        );
        assert!(
            (sample_ratio(Some("1.0")) - 1.0).abs() < f64::EPSILON,
            "in range"
        );
        assert!(
            sample_ratio(Some("-3")).abs() < f64::EPSILON,
            "negative clamps to 0"
        );
        assert!(
            (sample_ratio(Some("9")) - 1.0).abs() < f64::EPSILON,
            ">1 clamps to 1"
        );
    }

    /// An in-memory `MakeWriter` capturing fmt-layer output for assertions.
    #[derive(Clone, Default)]
    struct BufWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn json_stdout_line_carries_trace_id_for_in_span_events() {
        // The headline correlation requirement: with the OTel layer active and the
        // request span's trace_id recorded, every JSON log line emitted inside the
        // span must carry a 32-hex trace_id (what the Grafana Loki→Tempo derived
        // field keys on). Scoped subscriber + in-memory writer, never the global.
        use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
        use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let buf = BufWriter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_simple_exporter(InMemorySpanExporter::default())
            .build();
        let otel = tracing_opentelemetry::layer().with_tracer(provider.tracer("ferrofin"));
        let json = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(buf.clone());
        let subscriber = tracing_subscriber::registry().with(json).with(otel);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("http_request", trace_id = tracing::field::Empty);
            let _entered = span.enter();
            let span_ctx = span.context().span().span_context().clone();
            assert!(span_ctx.is_sampled(), "AlwaysOn → sampled");
            span.record("trace_id", span_ctx.trace_id().to_string());
            tracing::info!("inside the request");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = out
            .lines()
            .find(|l| l.contains("inside the request"))
            .expect("event line present in JSON output");
        let _: serde_json::Value =
            serde_json::from_str(line).expect("stdout line is valid single-line JSON");
        assert!(line.contains("trace_id"), "line carries the field: {line}");
        let has_32_hex = line
            .split(|c: char| !c.is_ascii_hexdigit())
            .any(|tok| tok.len() == 32 && tok.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(has_32_hex, "line carries a 32-hex trace id: {line}");
    }

    #[test]
    fn tracing_to_otel_bridge_exports_a_named_span() {
        // Proves the tracing→OTel version pairing actually bridges: compose the
        // OTel layer on a SCOPED subscriber (never the global set-once one), emit
        // an instrumented span, flush, and assert the in-memory exporter saw it.
        // This is the one runnable check that the feature works without a live
        // collector.
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::layer::SubscriberExt as _;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("ferrofin"));
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("bridge_probe");
            let _entered = span.enter();
        });
        provider.force_flush().unwrap();

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1, "exactly one span exported");
        assert_eq!(spans[0].name, "bridge_probe");
    }

    #[test]
    fn init_tracing_creates_log_dir_and_is_idempotent() {
        // Point data_dir at a tempdir so init_tracing prepares `{data_dir}/log`
        // — the directory `GET /System/Logs` serves. Creating it is the
        // deterministic side effect (the global subscriber is set-once, so a
        // second call is swallowed and file writes can't be asserted through it).
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config_with_ffmpeg(None);
        cfg.data_dir = tmp.path().to_path_buf();
        // Hold the returned WorkerGuard(s) — dropping early would tear down the
        // file writer's background thread. The second call is the idempotent no-op.
        let _guard = init_tracing(&cfg);
        let _guard2 = init_tracing(&cfg);
        assert!(tmp.path().join("log").is_dir(), "log directory created");
    }

    #[test]
    fn read_retention_days_defaults_missing_garbage_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("system.json");
        // Missing file → Jellyfin default 3.
        assert_eq!(read_retention_days(tmp.path()), 3);
        // Unparseable → default.
        std::fs::write(&sys, b"not json at all").unwrap();
        assert_eq!(read_retention_days(tmp.path()), 3);
        // Field absent → default.
        std::fs::write(&sys, br#"{"EnableMetrics":true}"#).unwrap();
        assert_eq!(read_retention_days(tmp.path()), 3);
    }

    #[test]
    fn read_retention_days_uses_present_value_including_forever() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("system.json");
        std::fs::write(&sys, br#"{"LogFileRetentionDays":7}"#).unwrap();
        assert_eq!(read_retention_days(tmp.path()), 7, "positive used verbatim");
        std::fs::write(&sys, br#"{"LogFileRetentionDays":0}"#).unwrap();
        assert_eq!(read_retention_days(tmp.path()), 0, "0 = keep forever");
        std::fs::write(&sys, br#"{"LogFileRetentionDays":-1}"#).unwrap();
        assert_eq!(
            read_retention_days(tmp.path()),
            -1,
            "negative = keep forever"
        );
    }

    #[test]
    fn non_blocking_file_writer_flushes_buffered_lines_on_guard_drop() {
        // The Step-4 guarantee: a line logged through the non-blocking file writer
        // lands on disk once the WorkerGuard is dropped (dropping early loses it).
        use tracing_subscriber::layer::SubscriberExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::NEVER)
            .filename_prefix("log")
            .filename_suffix("log")
            .build(tmp.path())
            .unwrap();
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("flush_probe_line");
        });
        drop(guard); // flushes the background queue

        let contents: String = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
            .collect();
        assert!(
            contents.contains("flush_probe_line"),
            "buffered line flushed to file: {contents:?}"
        );
    }

    #[test]
    fn panic_hook_logs_the_panic() {
        // The hook must route panics through tracing (they otherwise bypass every
        // sink). catch_unwind still fires the hook on this thread, where the scoped
        // subscriber captures it.
        use tracing_subscriber::layer::SubscriberExt as _;
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(buf.clone()),
        );
        install_panic_hook();
        tracing::subscriber::with_default(subscriber, || {
            let _ = std::panic::catch_unwind(|| panic!("kaboom"));
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("panicked"),
            "hook emitted the error line: {out}"
        );
        assert!(out.contains("kaboom"), "panic message captured: {out}");
    }

    #[tokio::test]
    async fn open_database_creates_data_dir_and_migrates() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("nested").join("data");
        let mut cfg = config_with_ffmpeg(None);
        cfg.data_dir = data_dir.clone();
        // The nested data dir does not exist yet; open_database must create it.
        assert!(!data_dir.exists());
        let _db = open_database(&cfg)
            .await
            .expect("database opens and migrates");
        assert!(data_dir.exists(), "data dir created");
        assert!(cfg.database_path().exists(), "sqlite file created");
    }

    /// Writes an executable fake `ffmpeg`-family shell script at `dir/name` that
    /// prints a version banner keyed to `banner_tool` (`ffmpeg` / `ffprobe`) plus
    /// the library-version lines the pure validator cross-checks. Lets the
    /// discovery happy path run without a real ffmpeg install.
    #[cfg(unix)]
    fn write_fake_ffmpeg(dir: &Path, name: &str, banner_tool: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        // First line satisfies our `<tool> version` prefix check; the `libav*`
        // lines satisfy `EncoderValidator`'s library-version cross-check for the
        // `ffprobe` banner (whose first line the `^ffmpeg version` regex skips).
        // `-filters` / `-encoders` invocations instead answer with tiny
        // capability tables so `probe_list` has something to parse.
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$2\" = -filters ]; then cat <<'EOF'\nFilters:\n\
             \x20 T.. = Timeline support\n\
             \x20T.C scale             V->V       Scale the input video size.\n\
             \x20... tonemapx          V->V       HDR to SDR tonemapping (SIMD).\n\
             EOF\nexit 0; fi\n\
             if [ \"$2\" = -encoders ]; then cat <<'EOF'\nEncoders:\n\
             \x20A..... = Audio\n\
             \x20------\n\
             \x20A....D aac                  AAC (Advanced Audio Coding)\n\
             \x20A....D libfdk_aac           Fraunhofer FDK AAC (codec aac)\n\
             EOF\nexit 0; fi\n\
             cat <<'EOF'\n{banner_tool} version 6.1.1 Copyright (c) 2000-2023\n\
             libavutil      58. 29.100\nlibavcodec     60. 31.102\nlibavformat    60. 16.100\n\
             libavdevice    60.  3.100\nlibavfilter     9. 12.100\nlibswscale      7.  5.100\n\
             libswresample   4. 12.100\nEOF\n"
        );
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discover_ffmpeg_happy_path_with_fake_binaries() {
        let tmp = tempfile::tempdir().unwrap();
        let ffmpeg = write_fake_ffmpeg(tmp.path(), "ffmpeg", "ffmpeg");
        // The derived ffprobe binary prints an ffprobe-keyed banner.
        let _ffprobe = write_fake_ffmpeg(tmp.path(), "ffprobe", "ffprobe");

        let mut cfg = config_with_ffmpeg(Some(ffmpeg.to_str().unwrap()));
        cfg.ffprobe_path = None; // force derivation from the ffmpeg basename
        let paths = discover_ffmpeg(&cfg, None)
            .await
            .expect("discovery succeeds");
        assert_eq!(paths.ffmpeg, ffmpeg);
        assert_eq!(paths.ffprobe, tmp.path().join("ffprobe"));
        // The `-filters` / `-encoders` probes parsed the fake's tables.
        assert!(paths.supports_filter("tonemapx"));
        assert!(paths.supports_filter("scale"));
        assert!(!paths.supports_filter("tonemap_cuda"));
        assert!(paths.supports_encoder("libfdk_aac"));
        assert!(!paths.supports_encoder("aac_at"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discover_ffmpeg_uses_explicit_ffprobe_path() {
        let tmp = tempfile::tempdir().unwrap();
        let ffmpeg = write_fake_ffmpeg(tmp.path(), "ffmpeg", "ffmpeg");
        let ffprobe = write_fake_ffmpeg(tmp.path(), "custom-probe", "ffprobe");

        let mut cfg = config_with_ffmpeg(Some(ffmpeg.to_str().unwrap()));
        cfg.ffprobe_path = Some(ffprobe.clone());
        let paths = discover_ffmpeg(&cfg, None)
            .await
            .expect("discovery succeeds");
        assert_eq!(paths.ffprobe, ffprobe);
    }

    #[test]
    fn resolve_ffmpeg_candidate_errors_when_nothing_found() {
        // No explicit path, no system path, and an empty `$PATH` for the lookup
        // (exercised via the pure `which_in` below) → resolution must bail. Here
        // we assert the message shape when even `$PATH` yields nothing by way of
        // a config with no ffmpeg and no system path; if the host happens to have
        // ffmpeg installed this is a no-op, so we only assert the error text when
        // it does error.
        let cfg = config_with_ffmpeg(None);
        if let Err(e) = resolve_ffmpeg_candidate(&cfg, None) {
            assert!(e.to_string().contains("no ffmpeg found"));
        }
    }

    #[test]
    fn which_in_finds_program_in_given_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("mytool");
        std::fs::write(&bin, b"x").unwrap();
        let found = which_in("mytool", Some(tmp.path().as_os_str()));
        assert_eq!(found, Some(bin));
    }

    #[test]
    fn which_in_returns_none_for_empty_path() {
        assert_eq!(which_in("mytool", None), None);
    }

    #[test]
    fn which_in_missing_program_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(which_in("nope", Some(tmp.path().as_os_str())), None);
    }

    fn config_with_ffmpeg(ffmpeg: Option<&str>) -> Config {
        Config {
            data_dir: PathBuf::from("/tmp/ferrofin"),
            config_dir: PathBuf::from("/tmp/ferrofin/config"),
            cache_dir: PathBuf::from("/tmp/ferrofin/cache"),
            web_dir: PathBuf::from("/tmp/ferrofin/web"),
            bind_addr: "0.0.0.0".parse().unwrap(),
            port: 8096,
            https_port: 8920,
            published_url: None,
            base_url: String::new(),
            omdb_api_key: String::new(),
            studios_repo_url: String::new(),
            tvdb_api_key: String::new(),
            tvdb_subscriber_pin: String::new(),
            fanart_personal_api_key: String::new(),
            musicbrainz_base_url: String::new(),
            ffmpeg_path: ffmpeg.map(PathBuf::from),
            ffprobe_path: None,
            library_roots: Vec::new(),
            server_name: "ferrofin".to_owned(),
            log_level: "info".to_owned(),
            admin_user: "admin".to_owned(),
            admin_password: String::new(),
            db_pool: None,
            enable_metrics: None,
            metrics_sample_interval: None,
            scan_progress_every: None,
            wasm_call_timeout_secs: None,
            wasm_memory_limit_mb: None,
            wasm_event_queue_capacity: None,
            wasm_private_http_allow: None,
        }
    }
}
