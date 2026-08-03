//! Startup side-effects for the Hermit server — port of the `Program.Main` /
//! `Startup` bring-up in `Jellyfin.Server`: initialise logging, open + migrate
//! the SQLite database, and discover + validate the ffmpeg / ffprobe binaries.
//!
//! Each helper is independently callable so the composition root (and tests)
//! can sequence them explicitly. The functions here own the *effects*; the
//! pure parsing they rely on lives in
//! [`hermit_mediaencoding::encoder::encoder_validator::EncoderValidator`].

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context as _;
use hermit_db::Database;
use hermit_mediaencoding::encoder::EncoderValidator;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

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
/// The `HERMIT_LOG` value (already resolved into [`Config::log_level`]) is used
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
pub fn init_tracing(config: &Config) {
    use tracing_subscriber::fmt::writer::MakeWriterExt as _;

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Log dir mirrors the composition root's `{data_dir}/log` (state.rs). The
    // rolling appender needs it to exist up front.
    let log_dir = config.data_dir.join("log");
    let file_appender = std::fs::create_dir_all(&log_dir).ok().and_then(|()| {
        tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("log")
            .filename_suffix("log")
            .build(&log_dir)
            .ok()
    });

    // `try_init` returns Err if a subscriber is already set (e.g. in tests);
    // that is fine — we only need one.
    //
    // ponytail: blocking file writes + no retention pruning. Fine at this log
    // volume; add `.max_log_files(n)` (from `log_file_retention_days`) or a
    // non-blocking `WorkerGuard` if the log dir grows unbounded or write
    // latency shows up.
    match file_appender {
        // ANSI off so the file (and stdout, tee'd through one writer) carry no
        // colour escapes — the dashboard renders raw text.
        Some(appender) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(true)
                .with_ansi(false)
                .with_writer(std::io::stdout.and(appender))
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(true)
                .try_init();
        }
    }
}

/// Opens the SQLite database at `{data_dir}/hermit.db` and applies migrations.
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
    tracing::info!(database = %config.database_path().display(), "database ready");
    spawn_pool_sampler(&db);
    Ok(db)
}

/// Samples the connection pool every 5 s at `debug` level (`RUST_LOG=
/// hermit_server=debug`): total connections, idle connections, and — the
/// contention signal — how many are checked out. Under load, `in_use` pinned at
/// the pool cap means requests are queueing on connection acquisition, not on
/// query work (the diagnosis behind the pool-size default; see
/// `benchmark/pool-sweep.sh`).
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
/// 1. `config.ffmpeg_path` (`--ffmpeg` / `HERMIT_FFMPEG_PATH`);
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
        return Ok((explicit.clone(), "config (--ffmpeg / HERMIT_FFMPEG_PATH)"));
    }
    if let Some(system) = system_encoder_path.filter(|s| !s.is_empty()) {
        return Ok((PathBuf::from(system), "system.json EncoderAppPath"));
    }
    if let Some(found) = which_on_path("ffmpeg") {
        return Ok((found, "$PATH"));
    }
    anyhow::bail!(
        "no ffmpeg found: set --ffmpeg / HERMIT_FFMPEG_PATH, configure system.json \
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
        hermit_mediaencoding::encoder::MIN_VERSION
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
    fn init_tracing_creates_log_dir_and_is_idempotent() {
        // Point data_dir at a tempdir so init_tracing prepares `{data_dir}/log`
        // — the directory `GET /System/Logs` serves. Creating it is the
        // deterministic side effect (the global subscriber is set-once, so a
        // second call is swallowed and file writes can't be asserted through it).
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config_with_ffmpeg(None);
        cfg.data_dir = tmp.path().to_path_buf();
        init_tracing(&cfg);
        init_tracing(&cfg);
        assert!(tmp.path().join("log").is_dir(), "log directory created");
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
            data_dir: PathBuf::from("/tmp/hermit"),
            config_dir: PathBuf::from("/tmp/hermit/config"),
            cache_dir: PathBuf::from("/tmp/hermit/cache"),
            web_dir: PathBuf::from("/tmp/hermit/web"),
            bind_addr: "0.0.0.0".parse().unwrap(),
            port: 8096,
            https_port: 8920,
            published_url: None,
            base_url: String::new(),
            omdb_api_key: String::new(),
            ffmpeg_path: ffmpeg.map(PathBuf::from),
            ffprobe_path: None,
            library_roots: Vec::new(),
            server_name: "hermit".to_owned(),
            log_level: "info".to_owned(),
            admin_user: "admin".to_owned(),
            admin_password: String::new(),
            db_pool: None,
        }
    }
}
