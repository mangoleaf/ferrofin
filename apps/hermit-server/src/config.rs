//! Bootstrap configuration for the Hermit server — port of the environment /
//! command-line half of `Jellyfin.Server`'s `StartupHelpers` +
//! `ServerApplicationPaths` seeding.
//!
//! This is the *bootstrap-only* config: the small set of knobs the host needs
//! to find its data directory, bind a socket, and locate ffmpeg **before** the
//! runtime [`HermitServerConfigurationManager`] takes over the persistent
//! `system.json` / `branding.json`. It is deliberately distinct from that
//! runtime configuration: a missing `config.toml` is not an error.
//!
//! Precedence, highest wins: **CLI flags**, then **`HERMIT_*` env vars**, then
//! **`config.toml`**, then **built-in defaults**. The optional `config.toml` is
//! loaded via the `config` crate; env and CLI are layered on top explicitly so
//! each flat `HERMIT_*` name maps to exactly the field the design specifies
//! (the crate's nested prefix/separator scheme does not fit the flat,
//! individually-named vars).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;

/// Default TCP port for the HTTP endpoint.
///
/// `8096` is the Jellyfin parity value (`ServerConfiguration.HttpServerPortNumber`).
/// FLAGGED as a magic number: parity-driven, not physically required — a
/// candidate setting is already exposed as [`Config::port`].
pub const DEFAULT_HTTP_PORT: u16 = 8096;

/// Default TCP port for the HTTPS endpoint (TLS termination is deferred).
///
/// `8920` is the Jellyfin parity value (`ServerConfiguration.HttpsServerPortNumber`).
/// FLAGGED as a magic number, same rationale as [`DEFAULT_HTTP_PORT`].
pub const DEFAULT_HTTPS_PORT: u16 = 8920;

/// Default bind address — all interfaces, matching Jellyfin's default host.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0";

/// Default log filter (RUST_LOG / `EnvFilter` syntax).
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default administrator username seeded on a fresh install.
///
/// FLAGGED for confirmation: `admin` is a convention, not a required value.
pub const DEFAULT_ADMIN_USER: &str = "admin";

/// Fallback program-data root when `$XDG_DATA_HOME` is unset and no
/// `$HOME/.local/share` can be derived — the FHS system location.
const SYSTEM_DATA_DIR: &str = "/var/lib/hermit";

/// The file stem of the SQLite database inside the data directory.
const DATABASE_FILE_NAME: &str = "hermit.db";

/// Command-line arguments — the highest-precedence configuration layer.
///
/// Only the knobs a operator commonly overrides on the command line are
/// exposed as flags; everything else is env / file / default. Each flag, when
/// present, overrides the corresponding env var and `config.toml` value.
#[derive(Debug, Default, Parser)]
#[command(name = "hermit-server", about = "Hermit media server", version)]
pub struct Cli {
    /// Program-data root directory (overrides `HERMIT_DATA_DIR`).
    #[arg(long = "data-dir", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Address to bind the HTTP listener to (overrides `HERMIT_BIND_ADDR`).
    #[arg(long = "bind", value_name = "ADDR")]
    pub bind_addr: Option<String>,

    /// HTTP port to listen on (overrides `HERMIT_PORT`).
    #[arg(long = "port", value_name = "PORT")]
    pub port: Option<u16>,

    /// Publicly reachable base URL advertised to clients
    /// (overrides `HERMIT_PUBLISHED_URL`).
    #[arg(long = "published-url", value_name = "URL")]
    pub published_url: Option<String>,

    /// Explicit path to the `ffmpeg` executable (overrides `HERMIT_FFMPEG_PATH`).
    #[arg(long = "ffmpeg", value_name = "PATH")]
    pub ffmpeg_path: Option<PathBuf>,

    /// Path to an optional `config.toml` bootstrap file.
    ///
    /// When omitted, `{data_dir}/config.toml` is consulted if present.
    #[arg(long = "config", value_name = "FILE")]
    pub config_file: Option<PathBuf>,
}

/// The subset of [`Config`] fields that may appear in `config.toml`.
///
/// All fields are optional: `config.toml` is a sparse override layer, and the
/// whole file is optional. Field names match the `HERMIT_*` env var stems
/// (lower-cased, prefix stripped) so the two layers describe the same knobs.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    web_dir: Option<PathBuf>,
    bind_addr: Option<String>,
    port: Option<u16>,
    https_port: Option<u16>,
    published_url: Option<String>,
    base_url: Option<String>,
    omdb_api_key: Option<String>,
    studios_repo_url: Option<String>,
    tvdb_api_key: Option<String>,
    tvdb_subscriber_pin: Option<String>,
    fanart_personal_api_key: Option<String>,
    musicbrainz_base_url: Option<String>,
    ffmpeg_path: Option<PathBuf>,
    ffprobe_path: Option<PathBuf>,
    library_roots: Option<Vec<PathBuf>>,
    server_name: Option<String>,
    log_level: Option<String>,
    admin_user: Option<String>,
    admin_password: Option<String>,
    db_pool: Option<DbPoolFileValue>,
    enable_metrics: Option<bool>,
    metrics_sample_interval: Option<u32>,
    scan_progress_every: Option<u32>,
}

/// The `db_pool` value in `config.toml`: an explicit SQLite connection count,
/// or the literal string `"auto"` for the built-in sizing formula
/// (`hermit_db`'s `default_pool_size`, derived from the mixed-load pool sweep —
/// see `suite/perf/pool-sweep.sh`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DbPoolFileValue {
    /// An explicit connection count (`db_pool = 16`).
    Count(u32),
    /// A sizing mode by name — only `"auto"` is valid (`db_pool = "auto"`).
    Mode(String),
}

/// The resolved bootstrap configuration, after layering CLI > env > file >
/// defaults and deriving the sub-directories under `data_dir`.
///
/// This is the value the composition root threads into
/// `HermitServerApplicationPaths::new`, `HermitServerConfigurationManager::load`,
/// ffmpeg discovery, and fresh-install seeding.
// Field names deliberately mirror the `HERMIT_*` env-var stems / `config.toml`
// keys (`config_dir`, `cache_dir`, ...), so the `_dir` suffix repeating the
// struct name is intentional and keeps the three config layers 1:1.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone)]
pub struct Config {
    /// Program-data root; derives the `data`/`config`/`cache`/`log`/`web`
    /// sub-directories. From `--data-dir` / `HERMIT_DATA_DIR`, else
    /// `$XDG_DATA_HOME/hermit`, else [`SYSTEM_DATA_DIR`].
    pub data_dir: PathBuf,

    /// Configuration directory holding `system.json` + `branding.json`.
    /// From `HERMIT_CONFIG_DIR`, else `{data_dir}/config`.
    pub config_dir: PathBuf,

    /// Cache directory, also the transcode/segment cache root.
    /// From `HERMIT_CACHE_DIR`, else `{data_dir}/cache`.
    pub cache_dir: PathBuf,

    /// Static web-client directory (`ServeDir` root). Optional for an
    /// API-only First-Light server. From `HERMIT_WEB_DIR`, else `{data_dir}/web`.
    pub web_dir: PathBuf,

    /// Address the HTTP listener binds to. Parsed from `bind_addr`.
    pub bind_addr: IpAddr,

    /// HTTP port to listen on. Default [`DEFAULT_HTTP_PORT`].
    pub port: u16,

    /// HTTPS port (TLS deferred). Default [`DEFAULT_HTTPS_PORT`].
    pub https_port: u16,

    /// Publicly reachable server URL advertised to clients
    /// (`HostNetworkInfo.published_server_url`). `None` = auto-detect.
    pub published_url: Option<String>,

    /// URL path prefix the server is mounted under
    /// (`HostNetworkInfo.base_url`). Empty string = root.
    pub base_url: String,

    /// OMDb (omdbapi.com) API key, enabling the Rotten Tomatoes critic rating.
    /// Empty = disabled (RT ratings stay unpopulated). From `HERMIT_OMDB_KEY` or
    /// `config.toml`.
    pub omdb_api_key: String,

    /// Studio Images artwork repository base URL. Empty = the built-in Jellyfin
    /// `emby-artwork` studios tree. From `HERMIT_STUDIOS_REPO_URL` or
    /// `config.toml`.
    pub studios_repo_url: String,

    /// TheTVDB API key. Empty = the built-in Jellyfin project key (TV metadata
    /// works with no configuration). From `HERMIT_TVDB_KEY` or `config.toml`.
    pub tvdb_api_key: String,

    /// TheTVDB subscriber PIN (for a user's paid subscription tier). Empty =
    /// non-subscriber. From `HERMIT_TVDB_PIN` or `config.toml`.
    pub tvdb_subscriber_pin: String,

    /// fanart.tv personal API key (`client_key`), raising rate limits and
    /// unlocking fresher artwork. Empty = the built-in key only. From
    /// `HERMIT_FANART_KEY` or `config.toml`.
    pub fanart_personal_api_key: String,

    /// MusicBrainz web-service base URL. Empty = `https://musicbrainz.org`. From
    /// `HERMIT_MUSICBRAINZ_URL` or `config.toml` (point at a mirror to lift the
    /// 1 req/sec limit).
    pub musicbrainz_base_url: String,

    /// Explicit `ffmpeg` executable path. `None` falls back to `system.json`
    /// then `$PATH` during discovery.
    pub ffmpeg_path: Option<PathBuf>,

    /// Explicit `ffprobe` executable path. `None` derives it from the resolved
    /// ffmpeg path during discovery.
    pub ffprobe_path: Option<PathBuf>,

    /// Initial media library roots used to seed a fresh install.
    pub library_roots: Vec<PathBuf>,

    /// This server's advertised machine / friendly name. Defaults to the host
    /// name.
    pub server_name: String,

    /// Log filter in `EnvFilter` (RUST_LOG) syntax.
    pub log_level: String,

    /// Administrator username seeded on a fresh install.
    pub admin_user: String,

    /// Administrator password seeded on a fresh install. Empty forces a
    /// password change on first login.
    pub admin_password: String,

    /// SQLite connection-pool size override. `None` = `auto` (the sizing
    /// formula in `hermit_db`). Resolved `HERMIT_DB_POOL` env > `db_pool` in
    /// `config.toml` (integer or `"auto"`) > auto, matching the layering of
    /// every other knob.
    pub db_pool: Option<u32>,

    /// Bootstrap override for the metrics endpoint toggle. `Some(true)`/`Some(false)`
    /// force it on/off regardless of the persisted `ServerConfiguration.EnableMetrics`;
    /// `None` defers to `system.json`. Resolved `HERMIT_ENABLE_METRICS` env >
    /// `enable_metrics` in `config.toml` > `None`. This exists for declarative
    /// (GitOps/container) deploys where flipping a `system.json` field or calling the
    /// API is impractical; the persisted toggle and dashboard still work when it is
    /// unset. It is a bootstrap knob only — NOT added to the API `ServerConfiguration`,
    /// so `/System/Configuration` stays byte-identical to Jellyfin.
    pub enable_metrics: Option<bool>,

    /// Metrics gauge-sampler interval, in seconds. `None` = the 15 s default
    /// (aligned with the Prometheus scrape interval). Resolved
    /// `HERMIT_METRICS_SAMPLE_INTERVAL` env > `metrics_sample_interval` in
    /// `config.toml` > default. Only consulted when `EnableMetrics` is set; kept
    /// out of the API `ServerConfiguration` so `/System/Configuration` stays
    /// byte-identical to Jellyfin.
    pub metrics_sample_interval: Option<u32>,

    /// Library-scan progress cadence: emit a progress `info!` every N items.
    /// `None` = the 100-item default; `0` disables progress logs. Resolved in
    /// order: `HERMIT_SCAN_PROGRESS_EVERY` env, then `scan_progress_every` in
    /// `config.toml`, else the default. A logging knob only — mistuning it changes
    /// log density, never scan correctness.
    pub scan_progress_every: Option<u32>,
}

impl Config {
    /// Loads and resolves the bootstrap configuration.
    ///
    /// Reads the process environment and, if present, the optional
    /// `config.toml`, then overlays `cli` on top, applying the design
    /// precedence (CLI > env > file > defaults) and deriving the sub-directory
    /// layout under the resolved `data_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if `config.toml` exists but cannot be parsed, or if the
    /// resolved bind address is not a valid IP.
    pub fn load(cli: Cli) -> anyhow::Result<Self> {
        Self::load_from(cli, &EnvProvider)
    }

    /// [`Config::load`] against an injectable environment provider (for tests).
    // One linear layering of CLI > env > file > default per knob; it grows by a
    // line per config field, so the length is inherent (cf. `build_app_state`).
    #[allow(clippy::too_many_lines)]
    fn load_from(cli: Cli, env: &dyn Env) -> anyhow::Result<Self> {
        let file = load_file_config(cli.config_file.as_deref(), env)?;

        // data_dir: CLI > env > file > $XDG_DATA_HOME/hermit > /var/lib/hermit.
        let data_dir = cli
            .data_dir
            .or_else(|| env.var("HERMIT_DATA_DIR").map(PathBuf::from))
            .or(file.data_dir)
            .unwrap_or_else(|| default_data_dir(env));

        let config_dir = env
            .var("HERMIT_CONFIG_DIR")
            .map(PathBuf::from)
            .or(file.config_dir)
            .unwrap_or_else(|| data_dir.join("config"));

        let cache_dir = env
            .var("HERMIT_CACHE_DIR")
            .map(PathBuf::from)
            .or(file.cache_dir)
            .unwrap_or_else(|| data_dir.join("cache"));

        let web_dir = env
            .var("HERMIT_WEB_DIR")
            .map(PathBuf::from)
            .or(file.web_dir)
            .unwrap_or_else(|| data_dir.join("web"));

        let bind_addr_str = cli
            .bind_addr
            .or_else(|| env.var("HERMIT_BIND_ADDR"))
            .or(file.bind_addr)
            .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned());
        let bind_addr: IpAddr = bind_addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid HERMIT_BIND_ADDR `{bind_addr_str}`: {e}"))?;

        let port = cli
            .port
            .or_else(|| parse_var(env, "HERMIT_PORT"))
            .or(file.port)
            .unwrap_or(DEFAULT_HTTP_PORT);

        let https_port = parse_var(env, "HERMIT_HTTPS_PORT")
            .or(file.https_port)
            .unwrap_or(DEFAULT_HTTPS_PORT);

        let published_url = cli
            .published_url
            .or_else(|| env.var("HERMIT_PUBLISHED_URL"))
            .or(file.published_url)
            .filter(|s| !s.is_empty());

        let base_url = env
            .var("HERMIT_BASE_URL")
            .or(file.base_url)
            .unwrap_or_default();

        let omdb_api_key = env
            .var("HERMIT_OMDB_KEY")
            .or(file.omdb_api_key)
            .unwrap_or_default();

        let studios_repo_url = env
            .var("HERMIT_STUDIOS_REPO_URL")
            .or(file.studios_repo_url)
            .unwrap_or_default();

        let tvdb_api_key = env
            .var("HERMIT_TVDB_KEY")
            .or(file.tvdb_api_key)
            .unwrap_or_default();

        let tvdb_subscriber_pin = env
            .var("HERMIT_TVDB_PIN")
            .or(file.tvdb_subscriber_pin)
            .unwrap_or_default();

        let fanart_personal_api_key = env
            .var("HERMIT_FANART_KEY")
            .or(file.fanart_personal_api_key)
            .unwrap_or_default();

        let musicbrainz_base_url = env
            .var("HERMIT_MUSICBRAINZ_URL")
            .or(file.musicbrainz_base_url)
            .unwrap_or_default();

        let ffmpeg_path = cli
            .ffmpeg_path
            .or_else(|| env.var("HERMIT_FFMPEG_PATH").map(PathBuf::from))
            .or(file.ffmpeg_path);

        let ffprobe_path = env
            .var("HERMIT_FFPROBE_PATH")
            .map(PathBuf::from)
            .or(file.ffprobe_path);

        let library_roots = env
            .var("HERMIT_LIBRARY_ROOTS")
            .map(|s| split_list(&s))
            .or(file.library_roots)
            .unwrap_or_default();

        let server_name = env
            .var("HERMIT_SERVER_NAME")
            .or(file.server_name)
            .unwrap_or_else(|| default_server_name(env));

        let log_level = env
            .var("HERMIT_LOG")
            .or(file.log_level)
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned());

        let admin_user = env
            .var("HERMIT_ADMIN_USER")
            .or(file.admin_user)
            .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_owned());

        let admin_password = env
            .var("HERMIT_ADMIN_PASSWORD")
            .or(file.admin_password)
            .unwrap_or_default();

        let db_pool = resolve_db_pool(env, file.db_pool)?;

        Ok(Self {
            data_dir,
            config_dir,
            cache_dir,
            web_dir,
            bind_addr,
            port,
            https_port,
            published_url,
            base_url,
            omdb_api_key,
            studios_repo_url,
            tvdb_api_key,
            tvdb_subscriber_pin,
            fanart_personal_api_key,
            musicbrainz_base_url,
            ffmpeg_path,
            ffprobe_path,
            library_roots,
            server_name,
            log_level,
            admin_user,
            admin_password,
            db_pool,
            enable_metrics: parse_var(env, "HERMIT_ENABLE_METRICS").or(file.enable_metrics),
            metrics_sample_interval: resolve_metrics_interval(env, file.metrics_sample_interval),
            scan_progress_every: parse_var(env, "HERMIT_SCAN_PROGRESS_EVERY")
                .or(file.scan_progress_every),
        })
    }

    /// The path to the SQLite database file, `{data_dir}/hermit.db`.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join(DATABASE_FILE_NAME)
    }

    /// The `sqlite:` connection URL for [`Config::database_path`], suitable for
    /// `hermit_db::Database::connect`.
    #[must_use]
    pub fn database_url(&self) -> String {
        format!("sqlite://{}", self.database_path().display())
    }
}

/// Resolves the metrics sampler interval (seconds): `HERMIT_METRICS_SAMPLE_INTERVAL`
/// env > `metrics_sample_interval` in `config.toml` > `None` (the sampler's 15 s
/// default). A zero or unparsable value falls back to the default — a mistuned
/// scrape knob changes cadence, never correctness.
fn resolve_metrics_interval(env: &dyn Env, file: Option<u32>) -> Option<u32> {
    parse_var(env, "HERMIT_METRICS_SAMPLE_INTERVAL")
        .or(file)
        .filter(|&s| s > 0)
}

/// Resolves the SQLite pool-size override: `HERMIT_DB_POOL` env (integer or
/// `auto`) > `db_pool` in `config.toml` (integer or `"auto"`) > `None` (auto).
///
/// `Some(n)` pins the pool at exactly `n` connections; `None` selects the
/// sizing formula in `hermit_db` (see `suite/perf/pool-sweep.sh` for how that
/// formula is derived). Zero and unrecognized values are errors — a silently
/// ignored typo here would change performance, not correctness, and never be
/// noticed.
fn resolve_db_pool(env: &dyn Env, file: Option<DbPoolFileValue>) -> anyhow::Result<Option<u32>> {
    if let Some(raw) = env.var("HERMIT_DB_POOL").filter(|s| !s.is_empty()) {
        if raw.eq_ignore_ascii_case("auto") {
            return Ok(None);
        }
        let n: u32 = raw.parse().map_err(|_| {
            anyhow::anyhow!("invalid HERMIT_DB_POOL `{raw}`: expected an integer or `auto`")
        })?;
        anyhow::ensure!(
            n >= 1,
            "invalid HERMIT_DB_POOL `0`: the pool needs at least one connection"
        );
        return Ok(Some(n));
    }
    match file {
        None => Ok(None),
        Some(DbPoolFileValue::Count(0)) => Err(anyhow::anyhow!(
            "invalid db_pool `0` in config.toml: the pool needs at least one connection"
        )),
        Some(DbPoolFileValue::Count(n)) => Ok(Some(n)),
        Some(DbPoolFileValue::Mode(s)) if s.eq_ignore_ascii_case("auto") => Ok(None),
        Some(DbPoolFileValue::Mode(s)) => Err(anyhow::anyhow!(
            "invalid db_pool `{s}` in config.toml: expected an integer or \"auto\""
        )),
    }
}

/// Reads and parses the optional `config.toml`.
///
/// The file is looked up at, in order: the `--config` flag, `HERMIT_CONFIG_FILE`
/// env var, else `{data_dir}/config.toml` when `HERMIT_DATA_DIR` is set. A
/// missing file yields an empty (all-`None`) config — never an error.
fn load_file_config(explicit: Option<&Path>, env: &dyn Env) -> anyhow::Result<FileConfig> {
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| env.var("HERMIT_CONFIG_FILE").map(PathBuf::from))
        .or_else(|| {
            env.var("HERMIT_DATA_DIR")
                .map(|d| PathBuf::from(d).join("config.toml"))
        });

    let Some(path) = path else {
        return Ok(FileConfig::default());
    };
    if !path.exists() {
        return Ok(FileConfig::default());
    }

    let settings = config::Config::builder()
        .add_source(config::File::from(path.as_path()))
        .build()
        .map_err(|e| anyhow::anyhow!("failed to read config file `{}`: {e}", path.display()))?;
    settings
        .try_deserialize()
        .map_err(|e| anyhow::anyhow!("failed to parse config file `{}`: {e}", path.display()))
}

/// The default program-data root: `$XDG_DATA_HOME/hermit`, else
/// `$HOME/.local/share/hermit`, else [`SYSTEM_DATA_DIR`].
fn default_data_dir(env: &dyn Env) -> PathBuf {
    if let Some(xdg) = env.var("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("hermit");
    }
    if let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".local/share/hermit");
    }
    PathBuf::from(SYSTEM_DATA_DIR)
}

/// The default advertised server name: the host name, else `"hermit"`.
fn default_server_name(env: &dyn Env) -> String {
    env.var("HOSTNAME")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hermit".to_owned())
}

/// Splits a `HERMIT_LIBRARY_ROOTS` list value on the OS path separator (`:` on
/// Unix), dropping empty segments.
fn split_list(value: &str) -> Vec<PathBuf> {
    std::env::split_paths(value)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// An injectable view of process environment variables, so config resolution is
/// unit-testable without mutating the real environment.
///
/// Kept object-safe (no generic methods) so it can be threaded as `&dyn Env`;
/// typed parsing lives in the free [`parse_var`] helper.
trait Env {
    /// Returns the value of `key`, or `None` if unset or non-UTF-8.
    fn var(&self, key: &str) -> Option<String>;
}

/// Reads `key` from `env` and parses it as `T`, yielding `None` when the var is
/// unset or does not parse.
fn parse_var<T: std::str::FromStr>(env: &dyn Env, key: &str) -> Option<T> {
    env.var(key).and_then(|v| v.parse().ok())
}

/// The real process environment.
struct EnvProvider;

impl Env for EnvProvider {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake environment backed by a map — no real `std::env` mutation.
    struct FakeEnv(HashMap<String, String>);

    impl FakeEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }

        fn with(mut self, key: &str, value: &str) -> Self {
            self.0.insert(key.to_owned(), value.to_owned());
            self
        }
    }

    impl Env for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn defaults_apply_with_empty_env_and_cli() {
        let env = FakeEnv::new();
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, DEFAULT_HTTP_PORT);
        assert_eq!(cfg.https_port, DEFAULT_HTTPS_PORT);
        assert_eq!(cfg.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.admin_user, DEFAULT_ADMIN_USER);
        assert!(cfg.admin_password.is_empty());
        assert!(cfg.published_url.is_none());
        assert!(cfg.base_url.is_empty());
        assert!(cfg.library_roots.is_empty());
        assert!(cfg.ffmpeg_path.is_none());
        // With no XDG/HOME the system location is used.
        assert_eq!(cfg.data_dir, PathBuf::from(SYSTEM_DATA_DIR));
    }

    #[test]
    fn data_dir_derives_sub_directories() {
        let env = FakeEnv::new().with("HERMIT_DATA_DIR", "/srv/hermit");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/hermit"));
        assert_eq!(cfg.config_dir, PathBuf::from("/srv/hermit/config"));
        assert_eq!(cfg.cache_dir, PathBuf::from("/srv/hermit/cache"));
        assert_eq!(cfg.web_dir, PathBuf::from("/srv/hermit/web"));
        assert_eq!(cfg.database_path(), PathBuf::from("/srv/hermit/hermit.db"));
        assert_eq!(cfg.database_url(), "sqlite:///srv/hermit/hermit.db");
    }

    #[test]
    fn explicit_sub_dir_env_overrides_derivation() {
        let env = FakeEnv::new()
            .with("HERMIT_DATA_DIR", "/srv/hermit")
            .with("HERMIT_CACHE_DIR", "/mnt/fast-cache")
            .with("HERMIT_CONFIG_DIR", "/etc/hermit")
            .with("HERMIT_WEB_DIR", "/usr/share/hermit-web");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.cache_dir, PathBuf::from("/mnt/fast-cache"));
        assert_eq!(cfg.config_dir, PathBuf::from("/etc/hermit"));
        assert_eq!(cfg.web_dir, PathBuf::from("/usr/share/hermit-web"));
    }

    #[test]
    fn xdg_data_home_wins_over_system_default() {
        let env = FakeEnv::new().with("XDG_DATA_HOME", "/home/u/.local/share");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/home/u/.local/share/hermit"));
    }

    #[test]
    fn home_fallback_when_no_xdg() {
        let env = FakeEnv::new().with("HOME", "/home/u");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/home/u/.local/share/hermit"));
    }

    #[test]
    fn cli_beats_env_which_beats_default() {
        let env = FakeEnv::new()
            .with("HERMIT_PORT", "9000")
            .with("HERMIT_DATA_DIR", "/env/data")
            .with("HERMIT_BIND_ADDR", "127.0.0.1");
        let cli = Cli {
            port: Some(7000),
            data_dir: Some(PathBuf::from("/cli/data")),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.port, 7000, "CLI port overrides env");
        assert_eq!(
            cfg.data_dir,
            PathBuf::from("/cli/data"),
            "CLI data_dir overrides env"
        );
        // bind_addr had no CLI override, so env wins over default.
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1");
    }

    #[test]
    fn env_port_overrides_default() {
        let env = FakeEnv::new().with("HERMIT_PORT", "9000");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, 9000);
    }

    #[test]
    fn library_roots_split_on_path_separator() {
        let env = FakeEnv::new().with("HERMIT_LIBRARY_ROOTS", "/media/movies:/media/tv");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(
            cfg.library_roots,
            vec![PathBuf::from("/media/movies"), PathBuf::from("/media/tv")]
        );
    }

    #[test]
    fn published_url_empty_string_is_none() {
        let env = FakeEnv::new().with("HERMIT_PUBLISHED_URL", "");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert!(cfg.published_url.is_none());
    }

    #[test]
    fn server_name_defaults_to_hostname_env() {
        let env = FakeEnv::new().with("HOSTNAME", "media-box");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.server_name, "media-box");
    }

    #[test]
    fn invalid_bind_addr_errors() {
        let env = FakeEnv::new().with("HERMIT_BIND_ADDR", "not-an-ip");
        let err = Config::load_from(Cli::default(), &env).unwrap_err();
        assert!(err.to_string().contains("invalid HERMIT_BIND_ADDR"));
    }

    #[test]
    fn config_toml_is_read_and_overridden_by_env() {
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(
            &toml,
            "port = 5000\nadmin_user = \"root\"\nbind_addr = \"10.0.0.1\"\n",
        )
        .unwrap();
        // File supplies all three; env overrides only the port.
        let env = FakeEnv::new().with("HERMIT_PORT", "6000");
        let cli = Cli {
            config_file: Some(toml),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.port, 6000, "env port beats file port");
        assert_eq!(cfg.admin_user, "root", "file admin_user applies");
        assert_eq!(
            cfg.bind_addr.to_string(),
            "10.0.0.1",
            "file bind_addr applies"
        );
    }

    #[test]
    fn db_pool_defaults_to_auto() {
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.db_pool, None, "no knob set ⇒ auto sizing");
    }

    #[test]
    fn enable_metrics_override_env_beats_file_and_defers_when_unset() {
        // Unset ⇒ None (defer to system.json).
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.enable_metrics, None);

        // config.toml sets it.
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "enable_metrics = true\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert_eq!(
            Config::load_from(cli(), &FakeEnv::new())
                .unwrap()
                .enable_metrics,
            Some(true)
        );

        // HERMIT_ENABLE_METRICS env beats the file (can force off, too).
        let env = FakeEnv::new().with("HERMIT_ENABLE_METRICS", "false");
        assert_eq!(
            Config::load_from(cli(), &env).unwrap().enable_metrics,
            Some(false)
        );
    }

    #[test]
    fn db_pool_env_beats_file_and_accepts_auto() {
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "db_pool = 8\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };

        // File alone applies.
        let cfg = Config::load_from(cli(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.db_pool, Some(8));

        // Env integer beats the file.
        let env = FakeEnv::new().with("HERMIT_DB_POOL", "32");
        assert_eq!(Config::load_from(cli(), &env).unwrap().db_pool, Some(32));

        // Env `auto` explicitly restores the formula over a file pin.
        let env = FakeEnv::new().with("HERMIT_DB_POOL", "auto");
        assert_eq!(Config::load_from(cli(), &env).unwrap().db_pool, None);

        // Empty env value is "unset", not an error (compose passes `${VAR:-}`).
        let env = FakeEnv::new().with("HERMIT_DB_POOL", "");
        assert_eq!(Config::load_from(cli(), &env).unwrap().db_pool, Some(8));
    }

    #[test]
    fn db_pool_file_accepts_auto_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");

        std::fs::write(&toml, "db_pool = \"auto\"\n").unwrap();
        let cli = Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert_eq!(
            Config::load_from(cli, &FakeEnv::new()).unwrap().db_pool,
            None
        );

        std::fs::write(&toml, "db_pool = \"lots\"\n").unwrap();
        let cli = Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        let err = Config::load_from(cli, &FakeEnv::new()).unwrap_err();
        assert!(err.to_string().contains("invalid db_pool"));

        std::fs::write(&toml, "db_pool = 0\n").unwrap();
        let cli = Cli {
            config_file: Some(toml),
            ..Cli::default()
        };
        let err = Config::load_from(cli, &FakeEnv::new()).unwrap_err();
        assert!(err.to_string().contains("at least one connection"));
    }

    #[test]
    fn db_pool_env_rejects_garbage() {
        let env = FakeEnv::new().with("HERMIT_DB_POOL", "many");
        let err = Config::load_from(Cli::default(), &env).unwrap_err();
        assert!(err.to_string().contains("invalid HERMIT_DB_POOL"));
    }

    #[test]
    fn missing_config_toml_is_not_an_error() {
        let env = FakeEnv::new();
        let cli = Cli {
            config_file: Some(PathBuf::from("/no/such/config.toml")),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.port, DEFAULT_HTTP_PORT);
    }

    #[test]
    fn scan_progress_every_resolves_from_env_and_preserves_zero() {
        // Env value is used verbatim.
        let env = FakeEnv::new().with("HERMIT_SCAN_PROGRESS_EVERY", "250");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.scan_progress_every, Some(250));

        // Unset → None (the scanner applies its 100-item default).
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.scan_progress_every, None);

        // 0 is preserved (disables progress logs), not treated as unset.
        let env = FakeEnv::new().with("HERMIT_SCAN_PROGRESS_EVERY", "0");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.scan_progress_every, Some(0));
    }
}
