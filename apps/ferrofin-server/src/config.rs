//! Bootstrap configuration for the Ferrofin server — port of the environment /
//! command-line half of `Jellyfin.Server`'s `StartupHelpers` +
//! `ServerApplicationPaths` seeding.
//!
//! This is the *bootstrap-only* config: the small set of knobs the host needs
//! to find its data directory, bind a socket, and locate ffmpeg **before** the
//! runtime [`FerrofinServerConfigurationManager`] takes over the persistent
//! `system.json` / `branding.json`. It is deliberately distinct from that
//! runtime configuration: a missing `config.toml` is not an error.
//!
//! Precedence, highest wins: **CLI flags**, then **`FERROFIN_*` env vars**, then
//! **`config.toml`**, then **built-in defaults**. The optional `config.toml` is
//! loaded via the `config` crate; env and CLI are layered on top explicitly so
//! each flat `FERROFIN_*` name maps to exactly the field the design specifies
//! (the crate's nested prefix/separator scheme does not fit the flat,
//! individually-named vars).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use ferrofin_providers::rate_limit::RequestLimits;
use serde::Deserialize;

/// Default TCP port for the HTTP endpoint.
///
/// `8096` is the Jellyfin parity value (`ServerConfiguration.HttpServerPortNumber`).
/// FLAGGED as a magic number: parity-driven, not physically required — a
/// candidate setting is already exposed as [`Config::port`].
pub const DEFAULT_HTTP_PORT: u16 = 8096;

/// Default TCP port for the HTTPS endpoint, served when the network
/// configuration enables HTTPS and its certificate loads.
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
const SYSTEM_DATA_DIR: &str = "/var/lib/ferrofin";

/// The file stem of the SQLite database inside the data directory.
const DATABASE_FILE_NAME: &str = "ferrofin.db";

/// The bootstrap file's name inside the data directory, where it is read
/// from unless `--config` or `FERROFIN_CONFIG_FILE` names another.
const CONFIG_FILE_NAME: &str = "config.toml";

/// ASP.NET's `HostOptions.ShutdownTimeout` default, which Jellyfin runs with.
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u32 = 30;

/// Minimum period, in milliseconds, between pushes of a dashboard WebSocket
/// stream (Sessions / ScheduledTasksInfo / ActivityLogEntry). The client names
/// the period in its `*Start` subscription. Every stream is event-driven, as
/// upstream's `BasePeriodicWebSocketListener` is, and the period only spaces
/// out low-priority updates (session activity, task progress); the floor
/// dates from when the task and activity streams ran on a timer, and upstream
/// has none.
/// 100 ms allows far faster updates than jellyfin-web's 1500 ms while bounding
/// the worst case for a hostile client.
const DEFAULT_WS_MIN_STREAM_PERIOD_MS: u32 = 100;

/// Command-line arguments — the highest-precedence configuration layer.
///
/// Only the knobs a operator commonly overrides on the command line are
/// exposed as flags; everything else is env / file / default. Each flag, when
/// present, overrides the corresponding env var and `config.toml` value.
#[derive(Debug, Default, Clone, Parser)]
#[command(name = "ferrofin-server", about = "Ferrofin media server", version)]
pub struct Cli {
    /// Program-data root directory (overrides `FERROFIN_DATA_DIR`).
    #[arg(long = "data-dir", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Address to bind the HTTP listener to (overrides `FERROFIN_BIND_ADDR`).
    #[arg(long = "bind", value_name = "ADDR")]
    pub bind_addr: Option<String>,

    /// HTTP port to listen on (overrides `FERROFIN_PORT`).
    #[arg(long = "port", value_name = "PORT")]
    pub port: Option<u16>,

    /// Publicly reachable base URL advertised to clients
    /// (overrides `FERROFIN_PUBLISHED_URL`).
    #[arg(long = "published-url", value_name = "URL")]
    pub published_url: Option<String>,

    /// Explicit path to the `ffmpeg` executable (overrides `FERROFIN_FFMPEG_PATH`).
    #[arg(long = "ffmpeg", value_name = "PATH")]
    pub ffmpeg_path: Option<PathBuf>,

    /// Path to an optional `config.toml` bootstrap file
    /// (overrides `FERROFIN_CONFIG_FILE`).
    ///
    /// When omitted, `config.toml` in the data directory (`--data-dir`, else
    /// `FERROFIN_DATA_DIR`, else the default location) is read if present.
    #[arg(long = "config", value_name = "FILE")]
    pub config_file: Option<PathBuf>,
}

/// The `FERROFIN_*` variables whose `config.toml` key is not their stem
/// (lower-cased, prefix stripped), with that key. Every other variable's key
/// is its stem, and `FERROFIN_CONFIG_FILE`, which names the file, has none.
/// One spelling per setting: the stem is no alias for these, since a file
/// holding both spellings would set one setting twice; it is an unknown key,
/// whose warning names the key to use.
const KEYS_NOT_NAMED_BY_STEM: &[(&str, &str)] = &[
    ("FERROFIN_LOG", "log_level"),
    ("FERROFIN_OMDB_KEY", "omdb_api_key"),
    ("FERROFIN_TVDB_KEY", "tvdb_api_key"),
    ("FERROFIN_TVDB_PIN", "tvdb_subscriber_pin"),
    ("FERROFIN_FANART_KEY", "fanart_personal_api_key"),
    ("FERROFIN_MUSICBRAINZ_URL", "musicbrainz_base_url"),
    (
        "FERROFIN_WS_MIN_STREAM_PERIOD_MS",
        "websocket_min_stream_period_ms",
    ),
];

/// The subset of [`Config`] fields that may appear in `config.toml`.
///
/// All fields are optional: `config.toml` is a sparse override layer, and the
/// whole file is optional. Field names are the keys: the `FERROFIN_*` env var
/// stems (lower-cased, prefix stripped), except those
/// [`KEYS_NOT_NAMED_BY_STEM`] lists, so the two layers describe the same knobs.
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
    /// Read as text whatever its TOML type (`6`, `1.5`, `"6h"`, `true`): the
    /// `config` crate would coerce a number into an integer field (`1.5` →
    /// 2) and fail the whole file on a string, so every value goes through
    /// [`Config::tvdb_cache_duration`]'s check and warning instead.
    tvdb_cache_hours: Option<String>,
    /// Read as text for the same reason as `tvdb_cache_hours`; checked by
    /// [`Config::tvdb_token_max_age`].
    tvdb_token_days: Option<String>,
    fanart_personal_api_key: Option<String>,
    musicbrainz_base_url: Option<String>,
    provider_timeout_seconds: Option<u64>,
    provider_max_wait_seconds: Option<u64>,
    ffmpeg_path: Option<PathBuf>,
    ffprobe_path: Option<PathBuf>,
    ffmpeg_img_extract_perf_tradeoff: Option<bool>,
    library_roots: Option<Vec<PathBuf>>,
    server_name: Option<String>,
    log_level: Option<String>,
    log_format: Option<String>,
    admin_user: Option<String>,
    admin_password: Option<String>,
    db_pool: Option<DbPoolFileValue>,
    enable_metrics: Option<bool>,
    disable_extensions: Option<bool>,
    metrics_sample_interval: Option<u32>,
    metrics_scan_duration_buckets: Option<Vec<f64>>,
    shutdown_timeout_secs: Option<u32>,
    websocket_min_stream_period_ms: Option<u32>,
    scan_progress_every: Option<u32>,
    scan_probe_concurrency: Option<u32>,
    scan_directory_cache_records: Option<u32>,
    scan_directory_cache_idle_secs: Option<u32>,
    auth_cache_ttl_secs: Option<u32>,
    row_cache_ttl_secs: Option<u32>,
    trickplay_concurrency: Option<CountOrAuto>,
    wasm_call_timeout_secs: Option<u32>,
    wasm_memory_limit_mb: Option<u32>,
    wasm_event_queue_capacity: Option<u32>,
    wasm_private_http_allow: Option<String>,
    max_plugin_download_mb: Option<u32>,
    wasm_analysis_concurrency: Option<u32>,
    wasm_state_limit_mb: Option<u32>,
    wasm_image_download_mb: Option<u32>,
    wasm_image_timeout_secs: Option<u32>,
    wasm_write_content_mb: Option<u32>,
    wasm_subtitle_extract_mb: Option<u32>,
    playback_metrics_queue: Option<u32>,
    quick_connect_pending_per_ip: Option<u32>,
}

/// The `db_pool` value in `config.toml`: an explicit SQLite connection count,
/// or the literal string `"auto"` for the built-in sizing formula
/// (`ferrofin_db`'s `default_pool_size`, derived from the mixed-load pool sweep —
/// see `suite/perf/pool-sweep.sh`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DbPoolFileValue {
    /// An explicit connection count (`db_pool = 16`).
    Count(u32),
    /// A sizing mode by name — only `"auto"` is valid (`db_pool = "auto"`).
    Mode(String),
}

/// A `config.toml` value that is an integer or the literal string `"auto"`.
/// Unlike [`DbPoolFileValue`] the integer is signed: zero and negative values
/// are accepted and mean `auto`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CountOrAuto {
    /// An explicit count (`trickplay_concurrency = 2`).
    Count(i64),
    /// A mode by name — only `"auto"` is valid (`trickplay_concurrency = "auto"`).
    Mode(String),
}

/// API roots that replace the public hosts of the remote providers a library
/// scan reaches ([`Config::provider_endpoints`], a test seam). `None` keeps
/// the provider's public host.
///
/// Not here, and why:
/// - MusicBrainz and the studio artwork repository: each has a real setting
///   already ([`Config::musicbrainz_base_url`], [`Config::studios_repo_url`]).
/// - LrcLib: the scheduled lyric-download test uses `lrclib`; scans never
///   download lyrics. ListenBrainz is only called for similar artists on request.
/// - Image CDNs other than TMDb's: their URLs come from the provider answers
///   (fanart.tv, TheTVDB, TheAudioDB), which a mock serves.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderEndpoints {
    /// Replaces `https://api.themoviedb.org/3`.
    pub tmdb: Option<String>,
    /// Replaces TMDb's image CDN root, `https://image.tmdb.org/t/p` (the
    /// artwork URLs TMDb's answers are relative to).
    pub tmdb_images: Option<String>,
    /// Replaces `https://www.omdbapi.com/`.
    pub omdb: Option<String>,
    /// OpenSubtitles REST endpoint override for integration tests.
    pub opensubtitles: Option<String>,
    /// Replaces `https://api4.thetvdb.com/v4`.
    pub tvdb: Option<String>,
    /// Replaces `https://webservice.fanart.tv/v3.2`.
    pub fanart: Option<String>,
    /// Replaces TheAudioDB's API root (`…/api/v1/json/{key}`).
    pub audiodb: Option<String>,
    /// Replaces `https://lrclib.net` for scheduled-lyrics integration tests.
    pub lrclib: Option<String>,
}

/// The base URL [`Config::test_stub`] gives the remote providers a scan or a
/// refresh reaches: a loopback port nothing serves (TCP 9, the discard port),
/// so a test server's provider requests are refused at once instead of
/// reaching the public internet.
#[doc(hidden)]
pub const OFFLINE_PROVIDER_URL: &str = "http://127.0.0.1:9";

impl ProviderEndpoints {
    /// Every provider here pointed at [`OFFLINE_PROVIDER_URL`]: what
    /// [`Config::test_stub`] uses. A test that mocks one provider overrides
    /// that field and keeps the rest offline with
    /// `..ProviderEndpoints::offline()`.
    #[must_use]
    pub fn offline() -> Self {
        let nowhere = Some(OFFLINE_PROVIDER_URL.to_owned());
        Self {
            tmdb: nowhere.clone(),
            tmdb_images: nowhere.clone(),
            omdb: nowhere.clone(),
            opensubtitles: nowhere.clone(),
            tvdb: nowhere.clone(),
            fanart: nowhere.clone(),
            audiodb: nowhere.clone(),
            lrclib: nowhere,
        }
    }
}

/// How log lines are written to stdout. The log file is always plain text:
/// `GET /System/Logs` shows it in the dashboard's log viewer as written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line, carrying the request span's `trace_id` (for a
    /// log pipeline such as Alloy and Loki). The default.
    #[default]
    Json,
    /// Human-readable lines, for running the server by hand.
    Text,
}

/// The resolved bootstrap configuration, after layering CLI > env > file >
/// defaults and deriving the sub-directories under `data_dir`.
///
/// This is the value the composition root threads into
/// `FerrofinServerApplicationPaths::new`, `FerrofinServerConfigurationManager::load`,
/// ffmpeg discovery, and fresh-install seeding.
// Field names deliberately mirror the `FERROFIN_*` env-var stems / `config.toml`
// keys (`config_dir`, `cache_dir`, ...), so the `_dir` suffix repeating the
// struct name is intentional and keeps the three config layers 1:1. Its bools
// are independent settings and facts, not the states of one machine.
#[allow(clippy::struct_field_names, clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct Config {
    /// Program-data root; derives the `data`/`config`/`cache`/`log`/`web`
    /// sub-directories. From `--data-dir` / `FERROFIN_DATA_DIR`, else
    /// `$XDG_DATA_HOME/ferrofin`, else [`SYSTEM_DATA_DIR`].
    pub data_dir: PathBuf,

    /// The bootstrap `config.toml` this configuration was read from, or would
    /// have been: `--config`, else `FERROFIN_CONFIG_FILE`, else
    /// `{data_dir}/config.toml` for the data directory `--data-dir`,
    /// `FERROFIN_DATA_DIR` or the default location names (never a `data_dir`
    /// set inside the file itself). Whether it existed is
    /// [`config_file_found`](Self::config_file_found).
    pub config_file: PathBuf,

    /// Whether [`config_file`](Self::config_file) existed and was read.
    pub config_file_found: bool,

    /// The command-line flags this configuration was loaded under, so the
    /// settings report (`crate::settings`) can tell a value a flag set,
    /// which the settings page then locks.
    pub cli: Cli,

    /// What loading noticed and set aside without failing, e.g. a key the
    /// file may not set. Loading runs before logging starts, so the server
    /// logs these once it has (at boot, and after each in-process restart),
    /// with [`config_file`](Self::config_file) as context.
    pub load_warnings: Vec<String>,

    /// Configuration directory holding `system.json` + `branding.json`.
    /// From `FERROFIN_CONFIG_DIR`, else `{data_dir}/config`.
    pub config_dir: PathBuf,

    /// Cache directory, also the transcode/segment cache root.
    /// From `FERROFIN_CACHE_DIR`, else `{data_dir}/cache`.
    pub cache_dir: PathBuf,

    /// Static web-client directory (`ServeDir` root). Optional for an
    /// API-only First-Light server. From `FERROFIN_WEB_DIR`, else `{data_dir}/web`.
    pub web_dir: PathBuf,

    /// Address the HTTP listener binds to. Parsed from `bind_addr`.
    pub bind_addr: IpAddr,

    /// Whether the operator chose `bind_addr` (flag, `FERROFIN_BIND_ADDR` or
    /// `config.toml`). When they did, that one address is the listener; when
    /// they did not, the dashboard's network settings (`LocalNetworkAddresses`,
    /// `EnableIPv4`/`EnableIPv6`) decide, as Jellyfin's `GetAllBindInterfaces`.
    pub bind_addr_explicit: bool,

    /// Internal discovery transport seam: production uses UDP 7359; test fixtures
    /// disable it or select an isolated loopback endpoint. Not a file/env setting.
    #[doc(hidden)]
    pub discovery_bind_addr: Option<std::net::SocketAddr>,

    /// Internal provider transport seam: production calls each built-in
    /// remote provider's public API; the end-to-end scan test points them at
    /// an in-process mock server. Not a file/env setting.
    #[doc(hidden)]
    pub provider_endpoints: ProviderEndpoints,

    /// HTTP port to listen on. Default [`DEFAULT_HTTP_PORT`].
    pub port: u16,

    /// HTTPS port, listened on beside the HTTP port when the network
    /// configuration's `EnableHttps` is on and its certificate loads. Default
    /// [`DEFAULT_HTTPS_PORT`].
    pub https_port: u16,

    /// Publicly reachable server URL advertised to clients
    /// (`HostNetworkInfo.published_server_url`). `None` = auto-detect.
    pub published_url: Option<String>,

    /// URL path prefix the server is mounted under
    /// (`HostNetworkInfo.base_url`). Empty string = root.
    pub base_url: String,

    /// OMDb (omdbapi.com) API key override. Empty uses Jellyfin's built-in key.
    /// From `FERROFIN_OMDB_KEY` or
    /// `config.toml`.
    pub omdb_api_key: String,

    /// Studio Images artwork repository base URL. Empty = the built-in Jellyfin
    /// `emby-artwork` studios tree. From `FERROFIN_STUDIOS_REPO_URL` or
    /// `config.toml`.
    pub studios_repo_url: String,

    /// TheTVDB API key. Empty = the built-in Jellyfin project key (TV metadata
    /// works with no configuration). From `FERROFIN_TVDB_KEY` or `config.toml`.
    pub tvdb_api_key: String,

    /// TheTVDB subscriber PIN (for a user's paid subscription tier). Empty =
    /// non-subscriber. From `FERROFIN_TVDB_PIN` or `config.toml`.
    pub tvdb_subscriber_pin: String,

    /// How long TheTVDB lookups are reused, in whole hours, as written: the
    /// TVDB plugin's `CacheDurationInHours` (default 1). It covers the
    /// episode ids the TVDB episode provider resolves and the season list of
    /// each series the season provider reads a season's id from. Resolved
    /// `FERROFIN_TVDB_CACHE_HOURS` env > `tvdb_cache_hours` in `config.toml` >
    /// unset; validated by [`tvdb_cache_duration`](Self::tvdb_cache_duration)
    /// where the client is built, so a bad value warns (tracing is up by
    /// then) and the default applies — it never stops the server.
    pub tvdb_cache_hours: Option<String>,

    /// How old TheTVDB's bearer token may get before Ferrofin logs in again,
    /// in whole days, as written: the TVDB plugin's 25 (`LoginAsync`), inside
    /// the month a TVDB v4 token is valid for. Resolved
    /// `FERROFIN_TVDB_TOKEN_DAYS` env > `tvdb_token_days` in `config.toml` >
    /// unset; validated by [`tvdb_token_max_age`](Self::tvdb_token_max_age)
    /// where the client is built, so a bad value warns and the default
    /// applies.
    pub tvdb_token_days: Option<String>,

    /// fanart.tv personal API key (`client_key`), raising rate limits and
    /// unlocking fresher artwork. Empty = the built-in key only. From
    /// `FERROFIN_FANART_KEY` or `config.toml`.
    pub fanart_personal_api_key: String,

    /// MusicBrainz web-service base URL. Empty = `https://musicbrainz.org`. From
    /// `FERROFIN_MUSICBRAINZ_URL` or `config.toml` (point at a mirror to lift the
    /// 1 req/sec limit).
    pub musicbrainz_base_url: String,

    /// Default per-attempt timeout of a metadata-provider request, in seconds
    /// (a request that sets its own keeps it). Resolved
    /// `FERROFIN_PROVIDER_TIMEOUT_SECONDS` env > `provider_timeout_seconds` in
    /// `config.toml` > 20, clamped to
    /// [`RequestLimits::TIMEOUT_SECONDS`](ferrofin_providers::rate_limit::RequestLimits::TIMEOUT_SECONDS)
    /// with a warning. The server hands it to every provider limiter for each
    /// lifetime (`ferrofin_providers::rate_limit::set_request_limits`).
    pub provider_timeout_seconds: u64,

    /// Longest pending pacing or quota wait before a metadata lookup is
    /// skipped, in seconds. Resolved `FERROFIN_PROVIDER_MAX_WAIT_SECONDS` env >
    /// `provider_max_wait_seconds` in `config.toml` > 60, clamped to
    /// [`RequestLimits::MAX_WAIT_SECONDS`](ferrofin_providers::rate_limit::RequestLimits::MAX_WAIT_SECONDS)
    /// with a warning; applied as
    /// [`provider_timeout_seconds`](Self::provider_timeout_seconds) is.
    pub provider_max_wait_seconds: u64,

    /// Explicit `ffmpeg` executable path. `None` falls back to `system.json`
    /// then `$PATH` during discovery.
    pub ffmpeg_path: Option<PathBuf>,

    /// Explicit `ffprobe` executable path. `None` derives it from the resolved
    /// ffmpeg path during discovery.
    pub ffprobe_path: Option<PathBuf>,

    /// Jellyfin's `FFmpeg:imgExtractPerfTradeoff` startup option
    /// (`GetFFmpegImgExtractPerfTradeoff`): chapter-image and thumbnail
    /// extraction trades frame quality for speed, decoding keyframes only and
    /// skipping the 24-frame thumbnail sampling. Resolved
    /// `FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF` env >
    /// `ffmpeg_img_extract_perf_tradeoff` in `config.toml` > `false`,
    /// Jellyfin's default.
    pub ffmpeg_img_extract_perf_tradeoff: bool,

    /// Initial media library roots used to seed a fresh install.
    pub library_roots: Vec<PathBuf>,

    /// This server's advertised machine / friendly name. Defaults to the host
    /// name.
    pub server_name: String,

    /// Log filter in `EnvFilter` (RUST_LOG) syntax.
    pub log_level: String,

    /// How stdout log lines are written. Resolved `FERROFIN_LOG_FORMAT` env >
    /// `log_format` in `config.toml` (`json` or `text`, any case) > JSON; any
    /// other value warns and is JSON. Logging is set up once per process.
    pub log_format: LogFormat,

    /// Administrator username seeded on a fresh install.
    pub admin_user: String,

    /// Administrator password seeded on a fresh install. Empty forces a
    /// password change on first login.
    pub admin_password: String,

    /// SQLite connection-pool size override. `None` = `auto` (the sizing
    /// formula in `ferrofin_db`). Resolved `FERROFIN_DB_POOL` env > `db_pool` in
    /// `config.toml` (integer or `"auto"`) > auto, matching the layering of
    /// every other knob.
    pub db_pool: Option<u32>,

    /// Bootstrap override for the metrics endpoint toggle. `Some(true)`/`Some(false)`
    /// force it on/off regardless of the persisted `ServerConfiguration.EnableMetrics`;
    /// `None` defers to `system.json`. Resolved `FERROFIN_ENABLE_METRICS` env >
    /// `enable_metrics` in `config.toml` > `None`. This exists for declarative
    /// (GitOps/container) deploys where flipping a `system.json` field or calling the
    /// API is impractical; the persisted toggle and dashboard still work when it is
    /// unset. It is a bootstrap knob only — NOT added to the API `ServerConfiguration`,
    /// so `/System/Configuration` stays byte-identical to Jellyfin.
    pub enable_metrics: Option<bool>,

    /// Suppress the compiled-in extensions (intro skipper, file
    /// transformation, merge versions) — `true` registers none of them but
    /// the Ferrofin settings extension, the server's own settings page
    /// (`ferrofin_extensions::registered_extensions`).
    ///
    /// This exists for measurement. A benchmark leg has to compare like with
    /// like, and the Jellyfin leg runs with no plugins installed; leaving
    /// Ferrofin's extensions on means its scheduled tasks and event hooks fire
    /// inside the measurement window while Jellyfin's do not. `suite/micro`
    /// and the perf compose both set `FERROFIN_DISABLE_EXTENSIONS=1`.
    ///
    /// Resolved `FERROFIN_DISABLE_EXTENSIONS` env > `disable_extensions` in
    /// `config.toml` > `false`. A bootstrap knob only — not part of the API
    /// `ServerConfiguration`, so `/System/Configuration` stays byte-identical
    /// to Jellyfin.
    pub disable_extensions: bool,

    /// Metrics gauge-sampler interval, in seconds. `None` = the 15 s default
    /// (aligned with the Prometheus scrape interval). Resolved
    /// `FERROFIN_METRICS_SAMPLE_INTERVAL` env > `metrics_sample_interval` in
    /// `config.toml` > default. Only consulted when `EnableMetrics` is set; kept
    /// out of the API `ServerConfiguration` so `/System/Configuration` stays
    /// byte-identical to Jellyfin.
    pub metrics_sample_interval: Option<u32>,

    /// Bucket boundaries (seconds, ascending) of the library-scan duration
    /// histograms (`ferrofin_library_scan_duration_seconds`,
    /// `ferrofin_library_scan_pass_duration_seconds`). `None` = the default,
    /// 1–2.5–5 per decade from 0.001 s to 5000 s
    /// ([`ferrofin_core::scan_metrics::DEFAULT_DURATION_BUCKETS`]). Resolved
    /// `FERROFIN_METRICS_SCAN_DURATION_BUCKETS` env (comma-separated, e.g.
    /// `0.01,0.1,1,10,100,1000`) > `metrics_scan_duration_buckets` in
    /// `config.toml` (an array of numbers) > default. Only consulted when
    /// `EnableMetrics` is set. Like the sampler interval, a bad value never
    /// stops the server: the metrics install refuses a list that is empty,
    /// non-finite, not above zero or not strictly increasing, warns, and uses
    /// the default. A bootstrap knob only, never a `ServerConfiguration` field.
    pub metrics_scan_duration_buckets: Option<Vec<f64>>,

    /// How long a graceful shutdown/restart waits for in-flight requests before
    /// the remaining connections are aborted, in seconds. Default 30 — ASP.NET's
    /// `HostOptions.ShutdownTimeout`, which Jellyfin runs with. Without a bound a
    /// client mid-stream would keep a draining host alive indefinitely, so an
    /// in-process restart during playback would never come back. Resolved
    /// `FERROFIN_SHUTDOWN_TIMEOUT_SECS` env > `shutdown_timeout_secs` in
    /// `config.toml` > 30.
    pub shutdown_timeout_secs: u32,

    /// Minimum period (milliseconds) between pushes of a dashboard WebSocket
    /// stream; the client-chosen Sessions / ScheduledTasksInfo /
    /// ActivityLogEntry period is clamped up to it. Resolved
    /// `FERROFIN_WS_MIN_STREAM_PERIOD_MS` env > `websocket_min_stream_period_ms`
    /// in `config.toml` > 100. See [`DEFAULT_WS_MIN_STREAM_PERIOD_MS`].
    pub websocket_min_stream_period_ms: u32,

    /// Library-scan progress cadence: emit a progress `info!` every N items.
    /// `None` = the 100-item default; `0` disables progress logs. Resolved in
    /// order: `FERROFIN_SCAN_PROGRESS_EVERY` env, then `scan_progress_every` in
    /// `config.toml`, else the default. A logging knob only — mistuning it changes
    /// log density, never scan correctness.
    pub scan_progress_every: Option<u32>,

    /// How many ffprobe processes a library scan keeps in flight. The probe is
    /// ~95% of scan wall time and is a pure per-file read, so probing one file
    /// at a time leaves every core but one idle. `None`/zero =
    /// [`ferrofin_core::library_scan::DEFAULT_SCAN_PROBE_CONCURRENCY`]. Raise
    /// it on a local SSD (it scales close to linearly with cores); leave it low
    /// on a spinning disk or a network mount, where a wide window turns
    /// sequential reads into seek thrash. Resolved
    /// `FERROFIN_SCAN_PROBE_CONCURRENCY` env > `scan_probe_concurrency` in
    /// `config.toml` > default.
    pub scan_probe_concurrency: Option<u32>,

    /// The directory listings one library scan keeps: dropped whole once
    /// they hold this many records (a folder counts each entry plus one).
    /// A scan lists each folder once and its items read that listing, so
    /// this bounds the memory a huge library's scan spends on them; past it,
    /// a folder is listed again. `None` = upstream's
    /// `DirectoryService.MaxCachedRecords` (100 000). Resolved
    /// `FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS` env >
    /// `scan_directory_cache_records` in `config.toml` > default; `0` is an
    /// error.
    pub scan_directory_cache_records: Option<u32>,

    /// How long, in seconds, a library scan's directory listings survive
    /// unread before they are dropped whole. `None` = upstream's
    /// `DirectoryService.IdleTimeoutMs` (300 s). Resolved
    /// `FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS` env >
    /// `scan_directory_cache_idle_secs` in `config.toml` > default; `0` is an
    /// error.
    pub scan_directory_cache_idle_secs: Option<u32>,

    /// How long, in seconds, a resolved access token (its device, user and
    /// policy) may be served from the auth cache. `None` = 30 s
    /// (`AUTH_CACHE_TTL`). Every change made through the server takes effect
    /// at once; this only bounds how long one made outside it, in the database
    /// itself, goes unseen. Resolved `FERROFIN_AUTH_CACHE_TTL_SECS` env >
    /// `auth_cache_ttl_secs` in `config.toml` > default; must be within
    /// [`CACHE_TTL_RANGE_SECS`].
    pub auth_cache_ttl_secs: Option<u32>,

    /// How long, in seconds, the per-user access check may serve a user's
    /// policy rows and the libraries' rows and options from its cache
    /// (`ferrofin_db::row_cache`). `None` = 30 s (`ROW_CACHE_TTL`). As for
    /// the auth cache, it only bounds edits made outside the server. Resolved
    /// `FERROFIN_ROW_CACHE_TTL_SECS` env > `row_cache_ttl_secs` in
    /// `config.toml` > default; must be within [`CACHE_TTL_RANGE_SECS`].
    pub row_cache_ttl_secs: Option<u32>,

    /// How many trickplay generations (one item width each, an ffmpeg frame
    /// extraction plus tile packing) run at once. A library scan runs several;
    /// the scheduled task runs one but shares the cap. `None` = `auto`: as many as the shared image-encoding
    /// pool allows (`ParallelImageEncodingLimit`, else the usable cores).
    /// Jellyfin runs one at a time; `1` restores that. Zero, negative, and
    /// values above the auto value also mean auto (applied in `state.rs`).
    /// Resolved `FERROFIN_TRICKPLAY_CONCURRENCY` env (integer or `auto`) >
    /// `trickplay_concurrency` in `config.toml` (integer or `"auto"`) > auto.
    pub trickplay_concurrency: Option<u32>,

    /// Per-guest-call deadline for WASM plugins, in seconds. `None` = the
    /// 30 s default (`ferrofin_wasm::WasmSettings`). Resolved
    /// `FERROFIN_WASM_CALL_TIMEOUT_SECS` env > `wasm_call_timeout_secs` in
    /// `config.toml` > default; zero is treated as unset.
    pub wasm_call_timeout_secs: Option<u32>,

    /// Per-plugin linear-memory cap for WASM plugins, in MiB. `None` = the
    /// 128 MiB default (a `memory.grow` ceiling, never a reservation).
    /// Resolved `FERROFIN_WASM_MEMORY_LIMIT_MB` env > `wasm_memory_limit_mb`
    /// in `config.toml` > default; zero is treated as unset.
    pub wasm_memory_limit_mb: Option<u32>,

    /// Per-plugin event queue depth for WASM plugins. `None` = the 256
    /// default. A full queue drops events for that plugin only (events are
    /// hints; the DB is the truth), and web requests may hold at most half
    /// of it. Resolved
    /// `FERROFIN_WASM_EVENT_QUEUE_CAPACITY` env >
    /// `wasm_event_queue_capacity` in `config.toml` > default; zero is
    /// treated as unset.
    pub wasm_event_queue_capacity: Option<u32>,

    /// WASM plugins allowed to `http-fetch` private/loopback destinations:
    /// a comma-separated list of plugin UUIDs, or `*` for all. `None`/empty
    /// = every plugin is denied private ranges (the safe default). Resolved
    /// `FERROFIN_WASM_PRIVATE_HTTP_ALLOW` env > `wasm_private_http_allow`
    /// in `config.toml` > deny.
    pub wasm_private_http_allow: Option<String>,

    /// Per-plugin total KV-state cap in MiB. `None`/zero = 8 MiB —
    /// settings and cursors fit easily; stats-heavy plugins (e.g. a
    /// playback-reporting port) may need more. Resolved
    /// `FERROFIN_WASM_STATE_LIMIT_MB` env > `wasm_state_limit_mb` in
    /// `config.toml` > default.
    pub wasm_state_limit_mb: Option<u32>,

    /// Cap on one plugin-artwork download, in MiB. `None`/zero = 20 MiB
    /// (posters/backdrops are single-digit MiB; anything larger is not
    /// artwork). Resolved `FERROFIN_WASM_IMAGE_DOWNLOAD_MB` env >
    /// `wasm_image_download_mb` in `config.toml` > default.
    pub wasm_image_download_mb: Option<u32>,

    /// Wall-clock bound on one plugin-artwork download, in seconds.
    /// `None`/zero = 30 s (a CDN GET, not a transfer job). Resolved
    /// `FERROFIN_WASM_IMAGE_TIMEOUT_SECS` env > `wasm_image_timeout_secs`
    /// in `config.toml` > default.
    pub wasm_image_timeout_secs: Option<u32>,

    /// Cap on one plugin `write-lyrics`/`write-subtitles` payload, in MiB.
    /// `None`/zero = 2 MiB (settings-class writes, not media). Resolved
    /// `FERROFIN_WASM_WRITE_CONTENT_MB` env > `wasm_write_content_mb` in
    /// `config.toml` > default.
    pub wasm_write_content_mb: Option<u32>,

    /// Cap on one plugin-extracted subtitle track, in MiB. `None`/zero =
    /// 10 MiB (generous for SRT text). Resolved
    /// `FERROFIN_WASM_SUBTITLE_EXTRACT_MB` env >
    /// `wasm_subtitle_extract_mb` in `config.toml` > default.
    pub wasm_subtitle_extract_mb: Option<u32>,

    /// Depth of the playback-metrics write queue, in events. `None`/zero =
    /// `ferrofin_core::playback_metrics::DEFAULT_QUEUE_DEPTH`. The recorder
    /// hands each PlaybackInfo decision and playstate update to this queue and
    /// returns, so a WAL checkpoint never stalls a playback request; a full
    /// queue drops the event with a warning rather than blocking. Raise it if
    /// `playback-metrics queue full` shows up in the log under a load spike.
    /// Resolved `FERROFIN_PLAYBACK_METRICS_QUEUE` env >
    /// `playback_metrics_queue` in `config.toml` > default.
    pub playback_metrics_queue: Option<u32>,

    /// Unauthorized Quick Connect requests one remote IP may hold at once;
    /// the next `POST /QuickConnect/Initiate` from that IP drops its oldest.
    /// `None`/zero = `ferrofin_core::quick_connect_manager::DEFAULT_PENDING_PER_IP` (10). Jellyfin has no
    /// cap. Resolved `FERROFIN_QUICK_CONNECT_PENDING_PER_IP` env >
    /// `quick_connect_pending_per_ip` in `config.toml` > default.
    pub quick_connect_pending_per_ip: Option<u32>,

    /// Concurrent media-decode budget for WASM plugin analysis
    /// (`extract-audio`/`extract-frames`). `None`/zero = a quarter of the
    /// visible cores, at least one — plugin analysis must never starve
    /// transcodes, but a NAS and a 32-core host disagree on what that
    /// means. Resolved `FERROFIN_WASM_ANALYSIS_CONCURRENCY` env >
    /// `wasm_analysis_concurrency` in `config.toml` > default.
    pub wasm_analysis_concurrency: Option<u32>,

    /// Size cap on a plugin artifact downloaded during a repository install,
    /// in MiB. `None` = the 128 MiB default (sized for interpreter-language
    /// guests bundling their runtime; an abuse guard, not a tuning knob).
    /// Resolved `FERROFIN_MAX_PLUGIN_DOWNLOAD_MB` env >
    /// `max_plugin_download_mb` in `config.toml` > default; zero is treated
    /// as unset.
    pub max_plugin_download_mb: Option<u32>,
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

    /// [`Config::load`] with `vars` in place of the process environment, so a
    /// test that loads a `config.toml` as the binary does sees none of the
    /// environment it runs in.
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    #[doc(hidden)]
    pub fn load_with_vars(cli: Cli, vars: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        struct Vars<'a>(&'a dyn Fn(&str) -> Option<String>);
        impl Env for Vars<'_> {
            fn var(&self, key: &str) -> Option<String> {
                (self.0)(key)
            }
        }
        Self::load_from(cli, &Vars(vars))
    }

    /// [`Config::load`] against an injectable environment provider (for tests).
    // One linear layering of CLI > env > file > default per knob; it grows by a
    // line per config field, so the length is inherent (cf. `build_app_state`).
    #[allow(clippy::too_many_lines)]
    pub(crate) fn load_from(cli: Cli, env: &dyn Env) -> anyhow::Result<Self> {
        let flags = cli.clone();
        let env = &WarningEnv {
            env,
            warnings: std::cell::RefCell::default(),
        };
        let mut load_warnings = Vec::new();
        let chosen_data_dir = cli
            .data_dir
            .or_else(|| env.var("FERROFIN_DATA_DIR").map(PathBuf::from));
        let named_file = cli.config_file.or_else(|| {
            env.var("FERROFIN_CONFIG_FILE")
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        });

        // data_dir: CLI > env > file > $XDG_DATA_HOME/ferrofin > /var/lib/ferrofin,
        // where the file may set it only when `--config`/`FERROFIN_CONFIG_FILE`
        // named the file: one found in the data directory cannot move it.
        let (data_dir, config_file, file, config_file_found) = if let Some(config_file) = named_file
        {
            let (file, found, unknown) = load_file_config(&config_file)?;
            load_warnings.extend(unknown);
            let data_dir = chosen_data_dir
                .or_else(|| file.data_dir.clone())
                .unwrap_or_else(|| default_data_dir(env));
            (data_dir, config_file, file, found)
        } else {
            let data_dir = chosen_data_dir.unwrap_or_else(|| default_data_dir(env));
            let config_file = data_dir.join(CONFIG_FILE_NAME);
            let (file, found, unknown) = load_file_config(&config_file)?;
            load_warnings.extend(unknown);
            if let Some(ignored) = file.data_dir.as_ref().filter(|dir| **dir != data_dir) {
                load_warnings.push(format!(
                    "data_dir `{}` in config.toml is ignored: a config.toml found in the data \
                     directory cannot move it; set --data-dir or FERROFIN_DATA_DIR, or name \
                     the file with --config or FERROFIN_CONFIG_FILE",
                    ignored.display()
                ));
            }
            (data_dir, config_file, file, found)
        };

        let config_dir = env
            .var("FERROFIN_CONFIG_DIR")
            .map(PathBuf::from)
            .or(file.config_dir)
            .unwrap_or_else(|| data_dir.join("config"));

        let cache_dir = env
            .var("FERROFIN_CACHE_DIR")
            .map(PathBuf::from)
            .or(file.cache_dir)
            .unwrap_or_else(|| data_dir.join("cache"));

        let web_dir = env
            .var("FERROFIN_WEB_DIR")
            .map(PathBuf::from)
            .or(file.web_dir)
            .unwrap_or_else(|| data_dir.join("web"));

        let chosen_bind = cli
            .bind_addr
            .or_else(|| env.var("FERROFIN_BIND_ADDR"))
            .or(file.bind_addr);
        let bind_addr_explicit = chosen_bind.is_some();
        let bind_addr_str = chosen_bind.unwrap_or_else(|| DEFAULT_BIND_ADDR.to_owned());
        let bind_addr: IpAddr = bind_addr_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid FERROFIN_BIND_ADDR `{bind_addr_str}`: {e}"))?;

        let port = cli
            .port
            .or_else(|| parse_var(env, "FERROFIN_PORT"))
            .or(file.port)
            .unwrap_or(DEFAULT_HTTP_PORT);

        let https_port = parse_var(env, "FERROFIN_HTTPS_PORT")
            .or(file.https_port)
            .unwrap_or(DEFAULT_HTTPS_PORT);

        let published_url = cli
            .published_url
            .or_else(|| env.var("FERROFIN_PUBLISHED_URL"))
            .or(file.published_url)
            .filter(|s| !s.is_empty());

        let base_url = env
            .var("FERROFIN_BASE_URL")
            .or(file.base_url)
            .unwrap_or_default();
        let base_url = crate::base_url::normalize(&base_url);

        let omdb_api_key = env
            .var("FERROFIN_OMDB_KEY")
            .or(file.omdb_api_key)
            .unwrap_or_default();

        let studios_repo_url = env
            .var("FERROFIN_STUDIOS_REPO_URL")
            .or(file.studios_repo_url)
            .unwrap_or_default();

        let tvdb_api_key = env
            .var("FERROFIN_TVDB_KEY")
            .or(file.tvdb_api_key)
            .unwrap_or_default();

        let tvdb_subscriber_pin = env
            .var("FERROFIN_TVDB_PIN")
            .or(file.tvdb_subscriber_pin)
            .unwrap_or_default();

        let tvdb_cache_hours = env
            .var("FERROFIN_TVDB_CACHE_HOURS")
            .filter(|raw| !raw.trim().is_empty())
            .or(file.tvdb_cache_hours);

        let tvdb_token_days = env
            .var("FERROFIN_TVDB_TOKEN_DAYS")
            .filter(|raw| !raw.trim().is_empty())
            .or(file.tvdb_token_days);

        let fanart_personal_api_key = env
            .var("FERROFIN_FANART_KEY")
            .or(file.fanart_personal_api_key)
            .unwrap_or_default();

        let musicbrainz_base_url = env
            .var("FERROFIN_MUSICBRAINZ_URL")
            .or(file.musicbrainz_base_url)
            .unwrap_or_default();

        let provider_timeout_seconds = resolve_provider_seconds(
            env,
            "FERROFIN_PROVIDER_TIMEOUT_SECONDS",
            "provider_timeout_seconds",
            file.provider_timeout_seconds,
            RequestLimits::DEFAULT_TIMEOUT_SECONDS,
            RequestLimits::TIMEOUT_SECONDS,
        );
        let provider_max_wait_seconds = resolve_provider_seconds(
            env,
            "FERROFIN_PROVIDER_MAX_WAIT_SECONDS",
            "provider_max_wait_seconds",
            file.provider_max_wait_seconds,
            RequestLimits::DEFAULT_MAX_WAIT_SECONDS,
            RequestLimits::MAX_WAIT_SECONDS,
        );

        let ffmpeg_path = cli
            .ffmpeg_path
            .or_else(|| env.var("FERROFIN_FFMPEG_PATH").map(PathBuf::from))
            .or(file.ffmpeg_path);

        let ffprobe_path = env
            .var("FERROFIN_FFPROBE_PATH")
            .map(PathBuf::from)
            .or(file.ffprobe_path);

        let library_roots = env
            .var("FERROFIN_LIBRARY_ROOTS")
            .map(|s| split_list(&s))
            .or(file.library_roots)
            .unwrap_or_default();

        let server_name = env
            .var("FERROFIN_SERVER_NAME")
            .or(file.server_name)
            .unwrap_or_else(|| default_server_name(env));

        let log_level = env
            .var("FERROFIN_LOG")
            .or(file.log_level)
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_owned());

        let log_format = resolve_log_format(env, file.log_format);

        let admin_user = env
            .var("FERROFIN_ADMIN_USER")
            .or(file.admin_user)
            .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_owned());

        let admin_password = env
            .var("FERROFIN_ADMIN_PASSWORD")
            .or(file.admin_password)
            .unwrap_or_default();

        let db_pool = resolve_db_pool(env, file.db_pool)?;

        let mut config = Self {
            data_dir,
            config_file,
            config_file_found,
            cli: flags,
            load_warnings: Vec::new(),
            config_dir,
            cache_dir,
            web_dir,
            bind_addr,
            bind_addr_explicit,
            discovery_bind_addr: Some(std::net::SocketAddr::from(([0, 0, 0, 0], 7359))),
            provider_endpoints: ProviderEndpoints::default(),
            port,
            https_port,
            published_url,
            base_url,
            omdb_api_key,
            studios_repo_url,
            tvdb_api_key,
            tvdb_subscriber_pin,
            tvdb_cache_hours,
            tvdb_token_days,
            fanart_personal_api_key,
            musicbrainz_base_url,
            provider_timeout_seconds,
            provider_max_wait_seconds,
            ffmpeg_path,
            ffprobe_path,
            ffmpeg_img_extract_perf_tradeoff: parse_bool_var(
                env,
                "FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF",
            )
            .or(file.ffmpeg_img_extract_perf_tradeoff)
            .unwrap_or(false),
            library_roots,
            server_name,
            log_level,
            log_format,
            admin_user,
            admin_password,
            db_pool,
            enable_metrics: parse_bool_var(env, "FERROFIN_ENABLE_METRICS").or(file.enable_metrics),
            disable_extensions: parse_bool_var(env, "FERROFIN_DISABLE_EXTENSIONS")
                .or(file.disable_extensions)
                .unwrap_or(false),
            metrics_sample_interval: resolve_metrics_interval(env, file.metrics_sample_interval),
            metrics_scan_duration_buckets: env
                .var("FERROFIN_METRICS_SCAN_DURATION_BUCKETS")
                .filter(|raw| !raw.trim().is_empty())
                .map(|raw| parse_bucket_list(&raw))
                .or(file.metrics_scan_duration_buckets),
            shutdown_timeout_secs: parse_var(env, "FERROFIN_SHUTDOWN_TIMEOUT_SECS")
                .or(file.shutdown_timeout_secs)
                .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_SECS),
            websocket_min_stream_period_ms: parse_var(env, "FERROFIN_WS_MIN_STREAM_PERIOD_MS")
                .or(file.websocket_min_stream_period_ms)
                .unwrap_or(DEFAULT_WS_MIN_STREAM_PERIOD_MS),
            scan_progress_every: parse_var(env, "FERROFIN_SCAN_PROGRESS_EVERY")
                .or(file.scan_progress_every),
            scan_probe_concurrency: parse_var(env, "FERROFIN_SCAN_PROBE_CONCURRENCY")
                .or(file.scan_probe_concurrency),
            scan_directory_cache_records: resolve_positive(
                env,
                "FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS",
                "scan_directory_cache_records",
                file.scan_directory_cache_records,
            )?,
            scan_directory_cache_idle_secs: resolve_positive(
                env,
                "FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS",
                "scan_directory_cache_idle_secs",
                file.scan_directory_cache_idle_secs,
            )?,
            auth_cache_ttl_secs: resolve_cache_ttl(
                env,
                "FERROFIN_AUTH_CACHE_TTL_SECS",
                "auth_cache_ttl_secs",
                file.auth_cache_ttl_secs,
            )?,
            row_cache_ttl_secs: resolve_cache_ttl(
                env,
                "FERROFIN_ROW_CACHE_TTL_SECS",
                "row_cache_ttl_secs",
                file.row_cache_ttl_secs,
            )?,
            trickplay_concurrency: resolve_trickplay_concurrency(env, file.trickplay_concurrency)?,
            wasm_call_timeout_secs: parse_var(env, "FERROFIN_WASM_CALL_TIMEOUT_SECS")
                .or(file.wasm_call_timeout_secs),
            wasm_memory_limit_mb: parse_var(env, "FERROFIN_WASM_MEMORY_LIMIT_MB")
                .or(file.wasm_memory_limit_mb),
            wasm_event_queue_capacity: parse_var(env, "FERROFIN_WASM_EVENT_QUEUE_CAPACITY")
                .or(file.wasm_event_queue_capacity),
            wasm_private_http_allow: env
                .var("FERROFIN_WASM_PRIVATE_HTTP_ALLOW")
                .or(file.wasm_private_http_allow)
                .filter(|s| !s.trim().is_empty()),
            max_plugin_download_mb: parse_var(env, "FERROFIN_MAX_PLUGIN_DOWNLOAD_MB")
                .or(file.max_plugin_download_mb),
            wasm_analysis_concurrency: parse_var(env, "FERROFIN_WASM_ANALYSIS_CONCURRENCY")
                .or(file.wasm_analysis_concurrency),
            wasm_state_limit_mb: parse_var(env, "FERROFIN_WASM_STATE_LIMIT_MB")
                .or(file.wasm_state_limit_mb),
            wasm_image_download_mb: parse_var(env, "FERROFIN_WASM_IMAGE_DOWNLOAD_MB")
                .or(file.wasm_image_download_mb),
            wasm_image_timeout_secs: parse_var(env, "FERROFIN_WASM_IMAGE_TIMEOUT_SECS")
                .or(file.wasm_image_timeout_secs),
            wasm_write_content_mb: parse_var(env, "FERROFIN_WASM_WRITE_CONTENT_MB")
                .or(file.wasm_write_content_mb),
            wasm_subtitle_extract_mb: parse_var(env, "FERROFIN_WASM_SUBTITLE_EXTRACT_MB")
                .or(file.wasm_subtitle_extract_mb),
            playback_metrics_queue: parse_var(env, "FERROFIN_PLAYBACK_METRICS_QUEUE")
                .or(file.playback_metrics_queue),
            quick_connect_pending_per_ip: parse_var(env, "FERROFIN_QUICK_CONNECT_PENDING_PER_IP")
                .or(file.quick_connect_pending_per_ip),
        };
        // The file's warnings first, then the variables', in the order read.
        load_warnings.extend(env.warnings.take());
        config.load_warnings = load_warnings;
        Ok(config)
    }

    /// The path to the SQLite database file.
    ///
    /// Normally `{data_dir}/ferrofin.db`. Drop-in adoption: when no `ferrofin.db`
    /// exists but the data dir holds a Jellyfin database — `jellyfin.db` at
    /// the root, or Jellyfin's own layout `data/jellyfin.db` — that file is
    /// opened instead and adopted in place
    /// (`ferrofin_db::Database` validates the version and takes a backup).
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        let ferrofin = self.data_dir.join(DATABASE_FILE_NAME);
        if ferrofin.exists() {
            return ferrofin;
        }
        for candidate in self.jellyfin_database_paths() {
            if candidate.exists() {
                return candidate;
            }
        }
        ferrofin
    }

    /// Where [`Config::database_path`] looks for a Jellyfin database to adopt
    /// when no `ferrofin.db` exists, in order: `jellyfin.db` at the data dir's
    /// root, then Jellyfin's own layout `data/jellyfin.db`.
    #[must_use]
    pub fn jellyfin_database_paths(&self) -> [PathBuf; 2] {
        [
            self.data_dir.join("jellyfin.db"),
            self.data_dir.join("data").join("jellyfin.db"),
        ]
    }

    /// TheTVDB's cache duration ([`tvdb_cache_hours`](Self::tvdb_cache_hours)):
    /// the configured whole hours, or the plugin's default of one hour
    /// ([`ferrofin_providers::tvdb::DEFAULT_CACHE_DURATION`]) when unset. A
    /// value that is not a whole number of hours above zero is refused with
    /// a warning and the default is used, as the sibling settings do; call
    /// it once tracing is installed.
    #[must_use]
    pub fn tvdb_cache_duration(&self) -> std::time::Duration {
        let default = ferrofin_providers::tvdb::DEFAULT_CACHE_DURATION;
        let Some(raw) = self.tvdb_cache_hours.as_deref() else {
            return default;
        };
        match raw.trim().parse::<u32>() {
            Ok(hours) if hours > 0 => std::time::Duration::from_hours(u64::from(hours)),
            _ => {
                tracing::warn!(
                    value = raw,
                    default_hours = default.as_secs() / 3600,
                    "FERROFIN_TVDB_CACHE_HOURS / tvdb_cache_hours is not a whole number of hours above zero; using the default"
                );
                default
            }
        }
    }

    /// TheTVDB's token max age ([`tvdb_token_days`](Self::tvdb_token_days)):
    /// the configured whole days, or the plugin's 25
    /// ([`ferrofin_providers::tvdb::DEFAULT_TOKEN_MAX_AGE`]) when unset. A
    /// value outside 1–29 days is refused with a warning and the default is
    /// used: TheTVDB expires a token after a month, so 30 or more would let
    /// every lookup fail before the renewal. Call it once tracing is
    /// installed.
    #[must_use]
    pub fn tvdb_token_max_age(&self) -> std::time::Duration {
        let default = ferrofin_providers::tvdb::DEFAULT_TOKEN_MAX_AGE;
        let Some(raw) = self.tvdb_token_days.as_deref() else {
            return default;
        };
        if let Ok(days @ 1..=29) = raw.trim().parse::<u32>() {
            return std::time::Duration::from_hours(u64::from(days) * 24);
        }
        tracing::warn!(
            value = raw,
            default_days = default.as_secs() / 86_400,
            "FERROFIN_TVDB_TOKEN_DAYS / tvdb_token_days is not a whole number of days from 1 to 29; using the default"
        );
        default
    }

    /// `next`, loaded again for an in-process restart, with what this process
    /// holds for its whole life put back to its running value; and the
    /// `config.toml` keys of the settings whose new value that leaves
    /// unapplied until the process restarts.
    ///
    /// Held for the process's life: the data, configuration and cache
    /// directories (the database, configuration and caches never move under a
    /// running process), the log filter and format (`init_tracing` installs
    /// them once), and
    /// the scan-duration histogram buckets (the metrics pipeline installs them
    /// once; kept even before metrics were first enabled, so the setting
    /// always applies on a process restart). The test seams that are no
    /// setting (`discovery_bind_addr`, `provider_endpoints`) carry over
    /// without a mention. `web_dir` is served anew each lifetime, so it
    /// follows the reload, even when it derives from a `data_dir` kept here.
    #[must_use]
    pub fn for_restart(&self, mut next: Config) -> (Config, Vec<&'static str>) {
        let mut kept = Vec::new();
        keep_running(&mut next.data_dir, &self.data_dir, "data_dir", &mut kept);
        keep_running(
            &mut next.config_dir,
            &self.config_dir,
            "config_dir",
            &mut kept,
        );
        keep_running(&mut next.cache_dir, &self.cache_dir, "cache_dir", &mut kept);
        keep_running(&mut next.log_level, &self.log_level, "log_level", &mut kept);
        keep_running(
            &mut next.log_format,
            &self.log_format,
            "log_format",
            &mut kept,
        );
        // Compared bit for bit: an unparsable entry is NaN, which never equals
        // itself, and an unchanged list must not read as a change.
        let bits = |buckets: &Option<Vec<f64>>| {
            buckets
                .as_ref()
                .map(|list| list.iter().map(|b| b.to_bits()).collect::<Vec<_>>())
        };
        if bits(&next.metrics_scan_duration_buckets) != bits(&self.metrics_scan_duration_buckets) {
            next.metrics_scan_duration_buckets
                .clone_from(&self.metrics_scan_duration_buckets);
            kept.push("metrics_scan_duration_buckets");
        }
        next.discovery_bind_addr = self.discovery_bind_addr;
        next.provider_endpoints.clone_from(&self.provider_endpoints);
        (next, kept)
    }

    /// The `sqlite:` connection URL for [`Config::database_path`], suitable for
    /// `ferrofin_db::Database::connect`.
    #[must_use]
    pub fn database_url(&self) -> String {
        format!("sqlite://{}", self.database_path().display())
    }

    /// A minimal [`Config`] for tests: all directories under `root`, empty
    /// credentials/keys, ephemeral ports, and the remote providers a scan or a
    /// refresh reaches (the [`ProviderEndpoints`], MusicBrainz and the studio
    /// artwork repository) offline at [`OFFLINE_PROVIDER_URL`], so a scan in a
    /// test never waits on the public internet. ListenBrainz, asked only for
    /// similar artists on request, keeps its public host. As with any
    /// MusicBrainz mirror, the music items' MusicBrainz links point at the
    /// configured base URL, here the offline one. The two URLs are operator
    /// settings, which win over the dashboard's MusicBrainz `Server` and Studio
    /// Images `RepositoryUrl`: a test of those dashboard settings sets them
    /// back to `String::new()`. Callers override the one or two fields they
    /// exercise via struct-update syntax (`..Config::test_stub(root)`), so
    /// adding a new field only needs a default here — not an edit at every
    /// site.
    ///
    /// `#[doc(hidden)] pub` (not `#[cfg(test)]`) so the integration tests in
    /// `tests/`, which link the crate compiled normally, can reach it too.
    // ponytail: unconditionally compiled (unused in release, dead-code-eliminated);
    // a feature flag would be the only way to gate it and isn't worth the plumbing.
    #[doc(hidden)]
    #[must_use]
    pub fn test_stub(root: &Path) -> Self {
        Config {
            data_dir: root.join("data"),
            config_file: root.join("data").join(CONFIG_FILE_NAME),
            config_file_found: false,
            cli: Cli::default(),
            load_warnings: Vec::new(),
            config_dir: root.join("config"),
            cache_dir: root.join("cache"),
            web_dir: root.join("web"),
            bind_addr: "127.0.0.1".parse().expect("literal IP parses"),
            bind_addr_explicit: true,
            discovery_bind_addr: None,
            provider_endpoints: ProviderEndpoints::offline(),
            port: 0,
            https_port: 0,
            published_url: None,
            base_url: String::new(),
            omdb_api_key: String::new(),
            studios_repo_url: OFFLINE_PROVIDER_URL.to_owned(),
            tvdb_api_key: String::new(),
            tvdb_subscriber_pin: String::new(),
            tvdb_cache_hours: None,
            tvdb_token_days: None,
            fanart_personal_api_key: String::new(),
            musicbrainz_base_url: OFFLINE_PROVIDER_URL.to_owned(),
            provider_timeout_seconds: RequestLimits::DEFAULT_TIMEOUT_SECONDS,
            provider_max_wait_seconds: RequestLimits::DEFAULT_MAX_WAIT_SECONDS,
            ffmpeg_path: None,
            ffprobe_path: None,
            ffmpeg_img_extract_perf_tradeoff: false,
            library_roots: Vec::new(),
            server_name: "ferrofin-test".to_owned(),
            log_level: "info".to_owned(),
            log_format: LogFormat::Json,
            admin_user: "admin".to_owned(),
            admin_password: String::new(),
            db_pool: None,
            enable_metrics: None,
            disable_extensions: false,
            metrics_sample_interval: None,
            metrics_scan_duration_buckets: None,
            shutdown_timeout_secs: DEFAULT_SHUTDOWN_TIMEOUT_SECS,
            websocket_min_stream_period_ms: DEFAULT_WS_MIN_STREAM_PERIOD_MS,
            scan_progress_every: None,
            scan_probe_concurrency: None,
            scan_directory_cache_records: None,
            scan_directory_cache_idle_secs: None,
            auth_cache_ttl_secs: None,
            row_cache_ttl_secs: None,
            trickplay_concurrency: None,
            wasm_call_timeout_secs: None,
            wasm_memory_limit_mb: None,
            wasm_event_queue_capacity: None,
            wasm_private_http_allow: None,
            max_plugin_download_mb: None,
            wasm_analysis_concurrency: None,
            wasm_state_limit_mb: None,
            wasm_image_download_mb: None,
            wasm_image_timeout_secs: None,
            wasm_write_content_mb: None,
            wasm_subtitle_extract_mb: None,
            playback_metrics_queue: None,
            quick_connect_pending_per_ip: None,
        }
    }
}

/// [`Config::for_restart`]'s step for one setting: sets `next` back to
/// `running` and records `key` when the reload changed it.
fn keep_running<T: Clone + PartialEq>(
    next: &mut T,
    running: &T,
    key: &'static str,
    kept: &mut Vec<&'static str>,
) {
    if next != running {
        next.clone_from(running);
        kept.push(key);
    }
}

/// Resolves a provider request limit in seconds: the `env_key` env var >
/// `file_key` in `config.toml` > `default`, clamped to `range`. A value
/// outside the range warns ([`Env::warn`]) and takes the nearest bound, as
/// the limit always did; one that does not parse warns and is unset.
fn resolve_provider_seconds(
    env: &dyn Env,
    env_key: &str,
    file_key: &str,
    file: Option<u64>,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
) -> u64 {
    let seconds = parse_var(env, env_key).or(file).unwrap_or(default);
    let clamped = seconds.clamp(*range.start(), *range.end());
    if clamped != seconds {
        env.warn(format!(
            "{env_key} / {file_key} `{seconds}` is outside {} to {} seconds; using {clamped}",
            range.start(),
            range.end()
        ));
    }
    clamped
}

/// Resolves the stdout log format: the `FERROFIN_LOG_FORMAT` env var when it
/// is not blank, else `log_format` in `config.toml`, else JSON. `json` and
/// `text` match in any case; anything else warns ([`Env::warn`]) and is JSON,
/// as it always was.
fn resolve_log_format(env: &dyn Env, file: Option<String>) -> LogFormat {
    let Some(raw) = env
        .var("FERROFIN_LOG_FORMAT")
        .filter(|raw| !raw.trim().is_empty())
        .or(file)
    else {
        return LogFormat::Json;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "json" => LogFormat::Json,
        "text" => LogFormat::Text,
        _ => {
            env.warn(format!(
                "FERROFIN_LOG_FORMAT / log_format `{raw}` is neither `json` nor `text`; using json"
            ));
            LogFormat::Json
        }
    }
}

/// Resolves the metrics sampler interval (seconds): `FERROFIN_METRICS_SAMPLE_INTERVAL`
/// env > `metrics_sample_interval` in `config.toml` > `None` (the sampler's 15 s
/// default). A zero or unparsable value falls back to the default — a mistuned
/// scrape knob changes cadence, never correctness.
fn resolve_metrics_interval(env: &dyn Env, file: Option<u32>) -> Option<u32> {
    parse_var(env, "FERROFIN_METRICS_SAMPLE_INTERVAL")
        .or(file)
        .filter(|&s| s > 0)
}

/// Parses a comma-separated list of bucket boundaries (seconds). An entry
/// that is not a number becomes `NaN`, so the metrics install refuses the
/// whole list with a warning instead of silently installing a shorter one.
fn parse_bucket_list(raw: &str) -> Vec<f64> {
    raw.split(',')
        .map(|entry| entry.trim().parse().unwrap_or(f64::NAN))
        .collect()
}

/// Resolves the SQLite pool-size override: `FERROFIN_DB_POOL` env (integer or
/// `auto`) > `db_pool` in `config.toml` (integer or `"auto"`) > `None` (auto).
///
/// `Some(n)` pins the pool at exactly `n` connections; `None` selects the
/// sizing formula in `ferrofin_db` (see `suite/perf/pool-sweep.sh` for how that
/// formula is derived). Zero and unrecognized values are errors — a silently
/// ignored typo here would change performance, not correctness, and never be
/// noticed.
fn resolve_db_pool(env: &dyn Env, file: Option<DbPoolFileValue>) -> anyhow::Result<Option<u32>> {
    if let Some(raw) = env.var("FERROFIN_DB_POOL").filter(|s| !s.is_empty()) {
        if raw.eq_ignore_ascii_case("auto") {
            return Ok(None);
        }
        let n: u32 = raw.parse().map_err(|_| {
            anyhow::anyhow!("invalid FERROFIN_DB_POOL `{raw}`: expected an integer or `auto`")
        })?;
        anyhow::ensure!(
            n >= 1,
            "invalid FERROFIN_DB_POOL `0`: the pool needs at least one connection"
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

/// Resolves a count that must be positive: the `env_key` env var > `file_key`
/// in `config.toml` > `None` (the default). Zero and anything that is not a
/// non-negative integer are errors, so a typo or a `0` meant as "off" is
/// noticed instead of silently disabling a cache. An empty env value is
/// unset.
fn resolve_positive(
    env: &dyn Env,
    env_key: &str,
    file_key: &str,
    file: Option<u32>,
) -> anyhow::Result<Option<u32>> {
    if let Some(raw) = env.var(env_key).filter(|s| !s.trim().is_empty()) {
        let n: u32 = raw.trim().parse().map_err(|_| {
            anyhow::anyhow!("invalid {env_key} `{raw}`: expected a positive integer")
        })?;
        anyhow::ensure!(n > 0, "invalid {env_key} `0`: expected a positive integer");
        return Ok(Some(n));
    }
    anyhow::ensure!(
        file != Some(0),
        "invalid {file_key} `0` in config.toml: expected a positive integer"
    );
    Ok(file)
}

/// The seconds a cache lifetime setting may take (`auth_cache_ttl_secs`,
/// `row_cache_ttl_secs`): from 5, below which a busy client's request burst
/// stops hitting the cache, to 300, past which an edit made outside the server
/// goes unseen for longer than an operator would wait for it.
pub const CACHE_TTL_RANGE_SECS: std::ops::RangeInclusive<u32> = 5..=300;

/// [`resolve_positive`] for a cache lifetime, which must also lie within
/// [`CACHE_TTL_RANGE_SECS`].
fn resolve_cache_ttl(
    env: &dyn Env,
    env_key: &str,
    file_key: &str,
    file: Option<u32>,
) -> anyhow::Result<Option<u32>> {
    let secs = resolve_positive(env, env_key, file_key, file)?;
    if let Some(n) = secs {
        anyhow::ensure!(
            CACHE_TTL_RANGE_SECS.contains(&n),
            "invalid {env_key} / {file_key} `{n}`: expected {} to {} seconds",
            CACHE_TTL_RANGE_SECS.start(),
            CACHE_TTL_RANGE_SECS.end()
        );
    }
    Ok(secs)
}

/// Resolves the trickplay concurrency: `FERROFIN_TRICKPLAY_CONCURRENCY` env
/// (integer or `auto`) > `trickplay_concurrency` in `config.toml` (integer or
/// `"auto"`) > `None` (auto). Zero and negative integers are `auto`; anything
/// that is neither an integer nor `auto` is an error, so a typo is noticed.
fn resolve_trickplay_concurrency(
    env: &dyn Env,
    file: Option<CountOrAuto>,
) -> anyhow::Result<Option<u32>> {
    // Huge values saturate; state.rs caps every value at the auto value anyway.
    let positive = |n: i64| (n > 0).then(|| u32::try_from(n).unwrap_or(u32::MAX));
    if let Some(raw) = env
        .var("FERROFIN_TRICKPLAY_CONCURRENCY")
        .filter(|s| !s.is_empty())
    {
        if raw.eq_ignore_ascii_case("auto") {
            return Ok(None);
        }
        let n: i64 = raw.trim().parse().map_err(|_| {
            anyhow::anyhow!(
                "invalid FERROFIN_TRICKPLAY_CONCURRENCY `{raw}`: expected an integer or `auto`"
            )
        })?;
        return Ok(positive(n));
    }
    match file {
        None => Ok(None),
        Some(CountOrAuto::Count(n)) => Ok(positive(n)),
        Some(CountOrAuto::Mode(s)) if s.eq_ignore_ascii_case("auto") => Ok(None),
        Some(CountOrAuto::Mode(s)) => Err(anyhow::anyhow!(
            "invalid trickplay_concurrency `{s}` in config.toml: expected an integer or \"auto\""
        )),
    }
}

/// The keys `config.toml` may hold: [`FileConfig`]'s field names, as its
/// derived `Deserialize` hands them to the deserializer, so no list is kept
/// by hand.
pub(crate) fn file_config_keys() -> &'static [&'static str] {
    use serde::de::{self, Visitor};

    /// Carries the field names out of `deserialize_struct`.
    #[derive(Debug)]
    struct Fields(&'static [&'static str]);
    impl std::fmt::Display for Fields {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("field names")
        }
    }
    impl std::error::Error for Fields {}
    impl de::Error for Fields {
        fn custom<T: std::fmt::Display>(_: T) -> Self {
            Fields(&[])
        }
    }

    /// A deserializer that only records the struct's field names.
    struct FieldNames;
    impl<'de> de::Deserializer<'de> for FieldNames {
        type Error = Fields;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Fields> {
            Err(Fields(&[]))
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Fields> {
            Err(Fields(fields))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
            identifier ignored_any
        }
    }

    match FileConfig::deserialize(FieldNames) {
        Err(Fields(fields)) => fields,
        Ok(_) => &[],
    }
}

/// Warnings for the keys of `table` that `config.toml` cannot hold, in key
/// order: each names the key, and the key to use where it is a key in the
/// wrong case (`Port`) or the stem of a variable whose key is spelled
/// otherwise ([`KEYS_NOT_NAMED_BY_STEM`]); the file's keys are case-sensitive.
fn unknown_key_warnings<'a>(table: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let known = file_config_keys();
    let mut unknown: Vec<&String> = table
        .into_iter()
        .filter(|key| !known.contains(&key.as_str()))
        .collect();
    unknown.sort();
    unknown
        .into_iter()
        .map(|key| {
            let canonical = known
                .iter()
                .find(|known| known.eq_ignore_ascii_case(key))
                .copied()
                .or_else(|| {
                    KEYS_NOT_NAMED_BY_STEM.iter().find_map(|(var, file_key)| {
                        var.strip_prefix("FERROFIN_")
                            .is_some_and(|stem| stem.eq_ignore_ascii_case(key))
                            .then_some(*file_key)
                    })
                });
            match canonical {
                Some(file_key) => format!(
                    "unknown key `{key}` in config.toml is ignored; did you mean `{file_key}`?"
                ),
                None => format!("unknown key `{key}` in config.toml is ignored"),
            }
        })
        .collect()
}

/// Reads and parses the optional `config.toml` at `path` (chosen by
/// [`Config::load_from`]), says whether it existed, and warns about each key
/// it holds that is not a setting ([`unknown_key_warnings`]). A missing file
/// yields an empty (all-`None`) config — never an error.
fn load_file_config(path: &Path) -> anyhow::Result<(FileConfig, bool, Vec<String>)> {
    let Some(settings) = read_config_file(path)? else {
        return Ok((FileConfig::default(), false, Vec::new()));
    };
    let warnings = unknown_key_warnings(top_level_keys(&settings, path)?.keys());
    let file = settings
        .try_deserialize()
        .map_err(|e| anyhow::anyhow!("failed to parse config file `{}`: {e}", path.display()))?;
    Ok((file, true, warnings))
}

/// The keys `config.toml` at `path` holds at its top level, read as
/// [`Config::load`] reads the file; none when it does not exist.
///
/// # Errors
///
/// As [`Config::load`], when the file exists but does not parse.
pub(crate) fn config_file_keys(path: &Path) -> anyhow::Result<Vec<String>> {
    let Some(settings) = read_config_file(path)? else {
        return Ok(Vec::new());
    };
    Ok(top_level_keys(&settings, path)?.into_keys().collect())
}

/// Reads `config.toml` at `path` through the `config` crate, or `None` when
/// it does not exist. A parse error never quotes the file
/// ([`without_source_lines`]).
fn read_config_file(path: &Path) -> anyhow::Result<Option<config::Config>> {
    if !path.exists() {
        return Ok(None);
    }
    config::Config::builder()
        .add_source(config::File::from(path))
        .build()
        .map(Some)
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to read config file `{}`: {}",
                path.display(),
                without_source_lines(&e.to_string())
            )
        })
}

/// `settings`' top-level table, keyed by the keys as written.
fn top_level_keys(
    settings: &config::Config,
    path: &Path,
) -> anyhow::Result<std::collections::HashMap<String, config::Value>> {
    settings
        .clone()
        .try_deserialize::<std::collections::HashMap<String, config::Value>>()
        .map_err(|e| anyhow::anyhow!("failed to parse config file `{}`: {e}", path.display()))
}

/// A TOML parse error without the lines of the file it quotes: the line at
/// fault may hold a password or an API key, and the error reaches the log (an
/// in-process restart logs it) and the terminal. What is left names the line
/// and column and the problem, e.g. `TOML parse error at line 3, column 18:
/// invalid basic string`.
fn without_source_lines(message: &str) -> String {
    let quotes_the_file = |line: &str| {
        line.trim_start()
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start()
            .starts_with('|')
    };
    let mut kept = String::new();
    for line in message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !quotes_the_file(line))
    {
        // The `config` crate appends ` in <file>` on a line of its own.
        let separator = if line.starts_with("in ") { " " } else { ": " };
        if !kept.is_empty() {
            kept.push_str(separator);
        }
        kept.push_str(line);
    }
    kept
}

/// The default program-data root: `$XDG_DATA_HOME/ferrofin`, else
/// `$HOME/.local/share/ferrofin`, else [`SYSTEM_DATA_DIR`].
fn default_data_dir(env: &dyn Env) -> PathBuf {
    if let Some(xdg) = env.var("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("ferrofin");
    }
    if let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".local/share/ferrofin");
    }
    PathBuf::from(SYSTEM_DATA_DIR)
}

/// The default advertised server name: the host name, else `"ferrofin"`.
fn default_server_name(env: &dyn Env) -> String {
    env.var("HOSTNAME")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ferrofin".to_owned())
}

/// Splits a `FERROFIN_LIBRARY_ROOTS` list value on the OS path separator (`:` on
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
pub(crate) trait Env {
    /// Returns the value of `key`, or `None` if unset or non-UTF-8.
    fn var(&self, key: &str) -> Option<String>;

    /// Records a value that is set but cannot be used. The process
    /// environment and the test fakes drop it; [`Config::load_from`] reads
    /// through a [`WarningEnv`], which keeps it for [`Config::load_warnings`].
    fn warn(&self, _warning: String) {}
}

/// An [`Env`] that keeps the warnings its values raise ([`Env::warn`]).
struct WarningEnv<'a> {
    env: &'a dyn Env,
    warnings: std::cell::RefCell<Vec<String>>,
}

impl Env for WarningEnv<'_> {
    fn var(&self, key: &str) -> Option<String> {
        self.env.var(key)
    }

    fn warn(&self, warning: String) {
        self.warnings.borrow_mut().push(warning);
    }
}

/// Reads `key` from `env` and parses it as `T` (blanks around the value
/// ignored), yielding `None` when the var is unset or blank, and when it does
/// not parse, which also warns ([`Env::warn`]) so the operator learns the
/// value did nothing.
pub(crate) fn parse_var<T: std::str::FromStr>(env: &dyn Env, key: &str) -> Option<T> {
    let raw = env.var(key)?;
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = value.parse().ok();
    if parsed.is_none() {
        env.warn(format!(
            "{key} `{raw}` is not a valid value and is ignored; config.toml or the default applies"
        ));
    }
    parsed
}

/// A boolean env var, accepting the forms people actually write in a container
/// or CI file — not just what `bool::from_str` takes.
///
/// `bool::from_str` accepts ONLY `"true"`/`"false"`, so plain [`parse_var`]
/// silently discards `FERROFIN_ENABLE_METRICS=1`: it parses as `None`, falls
/// through to the file/default, and the operator sees a knob that did nothing
/// and said nothing. Accepted here, case-insensitively: `1`/`0`, `true`/`false`,
/// `yes`/`no`, `on`/`off`. Blank is unset; anything else is `None` too, with a
/// warning ([`Env::warn`]).
pub(crate) fn parse_bool_var(env: &dyn Env, key: &str) -> Option<bool> {
    let raw = env.var(key)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        "" => None,
        _ => {
            env.warn(format!(
                "{key} `{raw}` is not on or off (1/0, true/false, yes/no, on/off) and is \
                 ignored; config.toml or the default applies"
            ));
            None
        }
    }
}

/// The real process environment.
pub(crate) struct EnvProvider;

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

    /// `XDG_DATA_HOME` in [`FakeEnv::new`]: a directory no host has, so a test
    /// that names no data directory reads no `config.toml` of the host's.
    const NOWHERE: &str = "/nonexistent/ferrofin-config-test";

    impl FakeEnv {
        /// An environment that sets only `XDG_DATA_HOME`, to [`NOWHERE`].
        fn new() -> Self {
            Self::empty().with("XDG_DATA_HOME", NOWHERE)
        }

        /// An environment that sets nothing at all, for the tests of the
        /// default data directory's fallbacks.
        fn empty() -> Self {
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
        assert_eq!(cfg.data_dir, PathBuf::from(NOWHERE).join("ferrofin"));
        assert_eq!(cfg.port, DEFAULT_HTTP_PORT);
        assert_eq!(cfg.https_port, DEFAULT_HTTPS_PORT);
        assert_eq!(cfg.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(cfg.admin_user, DEFAULT_ADMIN_USER);
        assert_eq!(cfg.admin_password, "");
        assert!(cfg.published_url.is_none());
        assert_eq!(cfg.base_url, "");
        assert!(cfg.library_roots.is_empty(), "{:?}", cfg.library_roots);
        assert!(cfg.ffmpeg_path.is_none());
        assert_eq!(
            cfg.websocket_min_stream_period_ms,
            DEFAULT_WS_MIN_STREAM_PERIOD_MS
        );
    }

    #[test]
    fn websocket_min_stream_period_resolves_env_over_default() {
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(
            cfg.websocket_min_stream_period_ms,
            DEFAULT_WS_MIN_STREAM_PERIOD_MS
        );

        let env = FakeEnv::new().with("FERROFIN_WS_MIN_STREAM_PERIOD_MS", "250");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.websocket_min_stream_period_ms, 250);
    }

    #[test]
    fn data_dir_derives_sub_directories() {
        let env = FakeEnv::new().with("FERROFIN_DATA_DIR", "/srv/ferrofin");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/ferrofin"));
        assert_eq!(cfg.config_dir, PathBuf::from("/srv/ferrofin/config"));
        assert_eq!(cfg.cache_dir, PathBuf::from("/srv/ferrofin/cache"));
        assert_eq!(cfg.web_dir, PathBuf::from("/srv/ferrofin/web"));
        assert_eq!(
            cfg.database_path(),
            PathBuf::from("/srv/ferrofin/ferrofin.db")
        );
        assert_eq!(cfg.database_url(), "sqlite:///srv/ferrofin/ferrofin.db");
    }

    #[test]
    fn explicit_sub_dir_env_overrides_derivation() {
        let env = FakeEnv::new()
            .with("FERROFIN_DATA_DIR", "/srv/ferrofin")
            .with("FERROFIN_CACHE_DIR", "/mnt/fast-cache")
            .with("FERROFIN_CONFIG_DIR", "/etc/ferrofin")
            .with("FERROFIN_WEB_DIR", "/usr/share/ferrofin-web");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.cache_dir, PathBuf::from("/mnt/fast-cache"));
        assert_eq!(cfg.config_dir, PathBuf::from("/etc/ferrofin"));
        assert_eq!(cfg.web_dir, PathBuf::from("/usr/share/ferrofin-web"));
    }

    #[test]
    fn xdg_data_home_wins_over_system_default() {
        let env = FakeEnv::new().with("XDG_DATA_HOME", "/home/u/.local/share");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/home/u/.local/share/ferrofin"));
    }

    #[test]
    fn home_fallback_when_no_xdg() {
        let env = FakeEnv::empty().with("HOME", "/home/u");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/home/u/.local/share/ferrofin"));
    }

    /// With no `XDG_DATA_HOME` and no `HOME`, the system location. It reads
    /// the host's `/var/lib/ferrofin/config.toml` if there is one, which
    /// cannot move the data directory it was found in.
    #[test]
    fn system_data_dir_when_no_xdg_and_no_home() {
        let cfg = Config::load_from(Cli::default(), &FakeEnv::empty()).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from(SYSTEM_DATA_DIR));
        assert_eq!(
            cfg.config_file,
            PathBuf::from(SYSTEM_DATA_DIR).join("config.toml")
        );
    }

    #[test]
    fn cli_beats_env_which_beats_default() {
        let env = FakeEnv::new()
            .with("FERROFIN_PORT", "9000")
            .with("FERROFIN_DATA_DIR", "/env/data")
            .with("FERROFIN_BIND_ADDR", "127.0.0.1");
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
        let env = FakeEnv::new().with("FERROFIN_PORT", "9000");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, 9000);
    }

    #[test]
    fn library_roots_split_on_path_separator() {
        let env = FakeEnv::new().with("FERROFIN_LIBRARY_ROOTS", "/media/movies:/media/tv");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(
            cfg.library_roots,
            vec![PathBuf::from("/media/movies"), PathBuf::from("/media/tv")]
        );
    }

    #[test]
    fn published_url_empty_string_is_none() {
        let env = FakeEnv::new().with("FERROFIN_PUBLISHED_URL", "");
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
        let env = FakeEnv::new().with("FERROFIN_BIND_ADDR", "not-an-ip");
        let err = Config::load_from(Cli::default(), &env).unwrap_err();
        assert!(err.to_string().contains("invalid FERROFIN_BIND_ADDR"));
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
        let env = FakeEnv::new().with("FERROFIN_PORT", "6000");
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

    /// `{data_dir}/config.toml` is read whatever names the data directory:
    /// the `--data-dir` flag, `FERROFIN_DATA_DIR`, or the default location.
    #[test]
    fn config_toml_in_the_data_dir_is_read_however_the_data_dir_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let flag_dir = dir.path().join("flag");
        let env_dir = dir.path().join("env");
        let xdg = dir.path().join("xdg");
        let default_dir = xdg.join("ferrofin");
        for (data_dir, port) in [(&flag_dir, 5001), (&env_dir, 5002), (&default_dir, 5003)] {
            std::fs::create_dir_all(data_dir).unwrap();
            std::fs::write(data_dir.join("config.toml"), format!("port = {port}\n")).unwrap();
        }

        let by_flag = Cli {
            data_dir: Some(flag_dir.clone()),
            ..Cli::default()
        };
        let cfg = Config::load_from(by_flag, &FakeEnv::new()).unwrap();
        assert_eq!(cfg.port, 5001, "--data-dir names the file");
        assert_eq!(cfg.config_file, flag_dir.join("config.toml"));
        assert!(cfg.config_file_found);

        let env = FakeEnv::new().with("FERROFIN_DATA_DIR", env_dir.to_str().unwrap());
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, 5002, "FERROFIN_DATA_DIR names the file");
        assert_eq!(cfg.config_file, env_dir.join("config.toml"));

        let env = FakeEnv::new().with("XDG_DATA_HOME", xdg.to_str().unwrap());
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, 5003, "the default data dir names the file");
        assert_eq!(cfg.data_dir, default_dir);
        assert_eq!(cfg.config_file, default_dir.join("config.toml"));
    }

    /// The flag beats the variable for the file as it does for the data
    /// directory, so `FERROFIN_DATA_DIR=/a --data-dir /b` reads
    /// `/b/config.toml`, never `/a/config.toml`.
    #[test]
    fn the_data_dir_flag_names_the_config_file_over_the_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        for (data_dir, port) in [(&a, 5001), (&b, 5002)] {
            std::fs::create_dir_all(data_dir).unwrap();
            std::fs::write(data_dir.join("config.toml"), format!("port = {port}\n")).unwrap();
        }
        let env = FakeEnv::new().with("FERROFIN_DATA_DIR", a.to_str().unwrap());
        let cli = Cli {
            data_dir: Some(b.clone()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.data_dir, b);
        assert_eq!(cfg.port, 5002);
        assert_eq!(cfg.config_file, b.join("config.toml"));
    }

    /// No file in the data directory: the path it would be read from is
    /// still resolved (and logged), and nothing is read.
    #[test]
    fn a_missing_config_toml_still_names_where_it_was_looked_for() {
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &FakeEnv::new()).unwrap();
        assert_eq!(cfg.config_file, dir.path().join("config.toml"));
        assert!(!cfg.config_file_found);
        assert_eq!(cfg.port, DEFAULT_HTTP_PORT);
        assert!(cfg.load_warnings.is_empty(), "{:?}", cfg.load_warnings);
    }

    /// `data_dir` inside the file applies only to a file named by `--config`
    /// or `FERROFIN_CONFIG_FILE`. Found in the data directory, the file cannot
    /// move that directory: the key is ignored with a warning naming it.
    #[test]
    fn data_dir_in_the_file_applies_only_to_a_file_named_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&data).unwrap();
        let body = format!(
            "data_dir = {:?}\nport = 5004\n",
            elsewhere.to_str().unwrap()
        );
        std::fs::write(data.join("config.toml"), &body).unwrap();

        let cli = Cli {
            data_dir: Some(data.clone()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &FakeEnv::new()).unwrap();
        assert_eq!(
            cfg.data_dir, data,
            "the file found in the data dir cannot move it"
        );
        assert_eq!(cfg.port, 5004, "the file's other keys still apply");
        assert_eq!(cfg.load_warnings.len(), 1, "{:?}", cfg.load_warnings);
        assert!(
            cfg.load_warnings[0].contains("data_dir"),
            "{:?}",
            cfg.load_warnings
        );

        let env = FakeEnv::new().with(
            "FERROFIN_CONFIG_FILE",
            data.join("config.toml").to_str().unwrap(),
        );
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.data_dir, elsewhere, "a file named explicitly sets it");
        assert!(cfg.load_warnings.is_empty(), "{:?}", cfg.load_warnings);
    }

    /// `--config` beats `FERROFIN_CONFIG_FILE`; a blank variable is unset, so
    /// the data directory's file is read; a named file that does not exist
    /// is no error and reads nothing, even with a data-dir file present.
    #[test]
    fn a_named_config_file_wins_and_a_blank_variable_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let (flag_file, env_file) = (dir.path().join("flag.toml"), dir.path().join("env.toml"));
        std::fs::write(&flag_file, "port = 5001\n").unwrap();
        std::fs::write(&env_file, "port = 5002\n").unwrap();
        std::fs::write(dir.path().join("config.toml"), "port = 5003\n").unwrap();
        let data_dir = dir.path().to_str().unwrap();

        let env = FakeEnv::new()
            .with("FERROFIN_DATA_DIR", data_dir)
            .with("FERROFIN_CONFIG_FILE", env_file.to_str().unwrap());
        let cli = Cli {
            config_file: Some(flag_file.clone()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!((cfg.port, cfg.config_file), (5001, flag_file));

        let env = FakeEnv::new()
            .with("FERROFIN_DATA_DIR", data_dir)
            .with("FERROFIN_CONFIG_FILE", "  ");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, 5003, "a blank FERROFIN_CONFIG_FILE is unset");
        assert_eq!(cfg.config_file, dir.path().join("config.toml"));

        let missing = dir.path().join("missing.toml");
        let env = FakeEnv::new()
            .with("FERROFIN_DATA_DIR", data_dir)
            .with("FERROFIN_CONFIG_FILE", missing.to_str().unwrap());
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(
            cfg.port, DEFAULT_HTTP_PORT,
            "the named file, not the data dir's"
        );
        assert_eq!(cfg.config_file, missing);
        assert!(!cfg.config_file_found);
    }

    /// A `data_dir` key naming the directory the file was found in changes
    /// nothing, so it draws no warning.
    #[test]
    fn data_dir_in_the_file_naming_its_own_directory_is_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("data_dir = {:?}\n", dir.path().to_str().unwrap());
        std::fs::write(dir.path().join("config.toml"), body).unwrap();
        let cli = Cli {
            data_dir: Some(dir.path().to_path_buf()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &FakeEnv::new()).unwrap();
        assert!(cfg.config_file_found);
        assert!(cfg.load_warnings.is_empty(), "{:?}", cfg.load_warnings);
    }

    /// An in-process restart applies what the reload changed, except what
    /// the process holds for its life, which keeps its running value and is
    /// named; the test seams carry over unnamed.
    #[test]
    fn a_restart_applies_the_reload_but_keeps_what_the_process_holds() {
        let running = Config {
            metrics_scan_duration_buckets: Some(vec![f64::NAN, 1.0]),
            ..Config::test_stub(Path::new("/run"))
        };
        let reloaded = Config {
            server_name: "renamed".to_owned(),
            scan_probe_concurrency: Some(3),
            // A bad bucket entry (NaN) on both sides is no change.
            metrics_scan_duration_buckets: Some(vec![f64::NAN, 1.0]),
            discovery_bind_addr: Some(std::net::SocketAddr::from(([0, 0, 0, 0], 7359))),
            provider_endpoints: ProviderEndpoints::default(),
            ..Config::test_stub(Path::new("/run"))
        };
        let (next, kept) = running.for_restart(reloaded);
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(next.server_name, "renamed");
        assert_eq!(next.scan_probe_concurrency, Some(3));
        assert_eq!(next.discovery_bind_addr, None);
        assert_eq!(next.provider_endpoints, ProviderEndpoints::offline());

        let moved = Config {
            log_level: "debug".to_owned(),
            metrics_scan_duration_buckets: Some(vec![0.5, 5.0]),
            ..Config::test_stub(Path::new("/elsewhere"))
        };
        let (next, kept) = running.for_restart(moved);
        assert_eq!(
            kept,
            [
                "data_dir",
                "config_dir",
                "cache_dir",
                "log_level",
                "metrics_scan_duration_buckets"
            ]
        );
        assert_eq!(next.data_dir, running.data_dir);
        assert_eq!(next.config_dir, running.config_dir);
        assert_eq!(next.cache_dir, running.cache_dir);
        assert_eq!(
            next.web_dir,
            PathBuf::from("/elsewhere/web"),
            "served anew each lifetime"
        );
        assert_eq!(next.log_level, running.log_level);
        assert_eq!(
            bits(next.metrics_scan_duration_buckets.as_deref()),
            bits(running.metrics_scan_duration_buckets.as_deref())
        );
    }

    fn bits(buckets: Option<&[f64]>) -> Option<Vec<u64>> {
        buckets.map(|list| list.iter().map(|b| b.to_bits()).collect())
    }

    /// A file that does not parse is refused naming its line and column, but
    /// never quoting the line: it may be a password's.
    #[test]
    fn a_parse_error_never_quotes_the_line_at_fault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 5000\nadmin_password = \"hunter2\n").unwrap();
        let cli = Cli {
            config_file: Some(path),
            ..Cli::default()
        };
        let message = format!("{:#}", Config::load_from(cli, &FakeEnv::new()).unwrap_err());
        assert!(!message.contains("hunter2"), "{message}");
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("config.toml"), "{message}");
        assert!(!message.contains(": in "), "{message}");
    }

    /// A secret written without quotes is read as text, so its type never
    /// makes an error that would quote it.
    #[test]
    fn an_unquoted_secret_is_read_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "admin_password = 123456\ntvdb_subscriber_pin = 4321\nomdb_api_key = true\n",
        )
        .unwrap();
        let cli = Cli {
            config_file: Some(path),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &FakeEnv::new()).unwrap();
        assert_eq!(cfg.admin_password, "123456");
        assert_eq!(cfg.tvdb_subscriber_pin, "4321");
        assert_eq!(cfg.omdb_api_key, "true");
    }

    /// A key `config.toml` cannot hold is a warning that names it, and the
    /// variable's stem where the key is spelled otherwise names the key to
    /// use; the file's other keys still apply, and the stem is no alias.
    #[test]
    fn an_unknown_key_is_a_warning_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "tvdb_pin = \"1234\"\nbogus = 1\nport = 5000\nServer_Name = \"x\"\n",
        )
        .unwrap();
        let cli = Cli {
            config_file: Some(path),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &FakeEnv::new()).unwrap();
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.tvdb_subscriber_pin, "", "the stem is not an alias");
        assert_eq!(cfg.load_warnings.len(), 3, "{:?}", cfg.load_warnings);
        let about = |key: &str| {
            cfg.load_warnings
                .iter()
                .find(|w| w.contains(&format!("`{key}`")))
                .unwrap_or_else(|| panic!("no warning about {key}: {:?}", cfg.load_warnings))
        };
        assert!(
            about("tvdb_pin").contains("`tvdb_subscriber_pin`"),
            "{:?}",
            cfg.load_warnings
        );
        assert!(
            !about("bogus").contains("did you mean"),
            "{:?}",
            cfg.load_warnings
        );
        assert!(
            about("Server_Name").contains("`server_name`"),
            "{:?}",
            cfg.load_warnings
        );
        assert_ne!(cfg.server_name, "x", "keys are case-sensitive");
    }

    /// Every `FERROFIN_*` variable this module reads has a `config.toml`
    /// key, its stem or the one [`KEYS_NOT_NAMED_BY_STEM`] gives it, and
    /// every key has a variable; `docs/CONFIG.md` has a row for every
    /// variable and lists every key that is not a stem. A new setting that
    /// misses any of these fails here.
    #[test]
    fn every_variable_has_a_key_and_a_row_in_the_docs() {
        let source = include_str!("config.rs");
        let code = &source[..source.find("\nmod tests {").expect("the test module")];
        let docs = include_str!("../../../docs/CONFIG.md");
        let vars: std::collections::BTreeSet<String> = code
            .match_indices("\"FERROFIN_")
            .map(|(at, _)| {
                code[at + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                    .collect::<String>()
            })
            .filter(|name| name.len() > "FERROFIN_".len())
            .collect();
        let key_of = |var: &str| {
            KEYS_NOT_NAMED_BY_STEM
                .iter()
                .find(|(named, _)| *named == var)
                .map_or_else(
                    || var["FERROFIN_".len()..].to_ascii_lowercase(),
                    |(_, key)| (*key).to_owned(),
                )
        };
        let keys = file_config_keys();
        let mut problems = Vec::new();
        for var in &vars {
            if !docs.contains(&format!("| `{var}` |")) {
                problems.push(format!("{var}: no row in docs/CONFIG.md"));
            }
            let key = key_of(var);
            if var != "FERROFIN_CONFIG_FILE" && !keys.contains(&key.as_str()) {
                problems.push(format!("{var}: config.toml has no key `{key}`"));
            }
            if var != "FERROFIN_CONFIG_FILE"
                && !crate::settings::SETTINGS
                    .iter()
                    .any(|row| row.key == key && row.env == Some(var.as_str()))
            {
                problems.push(format!(
                    "{var}: no settings row has key `{key}` and this variable"
                ));
            }
        }
        for key in keys {
            if !vars.iter().any(|var| key_of(var) == *key) {
                problems.push(format!("`{key}`: no FERROFIN_* variable reads it"));
            }
        }
        for (var, key) in KEYS_NOT_NAMED_BY_STEM {
            if !docs.contains(&format!("| `{var}` | `{key}` |")) {
                problems.push(format!(
                    "{var} → {key}: missing from docs/CONFIG.md's key table"
                ));
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
        assert!(
            vars.len() > 40,
            "the scan found too few variables: {vars:?}"
        );
    }

    /// A variable that does not parse is a warning naming it, and
    /// `config.toml` or the default applies as before; a blank one is unset.
    #[test]
    fn an_unparsable_variable_is_a_warning_naming_it() {
        let env = FakeEnv::new()
            .with("FERROFIN_PORT", "80g")
            .with("FERROFIN_ENABLE_METRICS", "maybe")
            .with("FERROFIN_HTTPS_PORT", "")
            .with("FERROFIN_SHUTDOWN_TIMEOUT_SECS", " 45 ");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.port, DEFAULT_HTTP_PORT);
        assert_eq!(cfg.enable_metrics, None);
        assert_eq!(cfg.https_port, DEFAULT_HTTPS_PORT);
        assert_eq!(
            cfg.shutdown_timeout_secs, 45,
            "surrounding blanks are not an error"
        );
        assert_eq!(cfg.load_warnings.len(), 2, "{:?}", cfg.load_warnings);
        assert!(
            cfg.load_warnings[0].contains("FERROFIN_PORT"),
            "{:?}",
            cfg.load_warnings
        );
        assert!(
            cfg.load_warnings[1].contains("FERROFIN_ENABLE_METRICS"),
            "{:?}",
            cfg.load_warnings
        );
    }

    /// The provider request limits and the log format are settings of the
    /// file too, not unknown keys.
    #[test]
    fn the_provider_limits_and_the_log_format_are_config_toml_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "provider_timeout_seconds = 30\nprovider_max_wait_seconds = 120\nlog_format = \"text\"\n",
        )
        .unwrap();
        let cli = Cli {
            config_file: Some(path),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli.clone(), &FakeEnv::new()).unwrap();
        assert!(cfg.load_warnings.is_empty(), "{:?}", cfg.load_warnings);
        assert_eq!(cfg.provider_timeout_seconds, 30);
        assert_eq!(cfg.provider_max_wait_seconds, 120);
        assert_eq!(cfg.log_format, LogFormat::Text);

        // The variables win; out of range is clamped with a warning, an
        // unknown format warns and is JSON.
        let env = FakeEnv::new()
            .with("FERROFIN_PROVIDER_TIMEOUT_SECONDS", "5")
            .with("FERROFIN_PROVIDER_MAX_WAIT_SECONDS", "45")
            .with("FERROFIN_LOG_FORMAT", "pretty");
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.provider_timeout_seconds, 10);
        assert_eq!(cfg.provider_max_wait_seconds, 45);
        assert_eq!(cfg.log_format, LogFormat::Json);
        assert_eq!(cfg.load_warnings.len(), 2, "{:?}", cfg.load_warnings);
        assert!(
            cfg.load_warnings[0].contains("FERROFIN_PROVIDER_TIMEOUT_SECONDS"),
            "{:?}",
            cfg.load_warnings
        );
        assert!(
            cfg.load_warnings[1].contains("`pretty`"),
            "{:?}",
            cfg.load_warnings
        );

        // Unset: the defaults, which the provider crate defines.
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(
            (cfg.provider_timeout_seconds, cfg.provider_max_wait_seconds),
            (
                RequestLimits::DEFAULT_TIMEOUT_SECONDS,
                RequestLimits::DEFAULT_MAX_WAIT_SECONDS
            )
        );
        assert_eq!(cfg.log_format, LogFormat::Json);
    }

    /// The config file is TOML: a file named for another format is refused,
    /// not read in a format the docs never describe, and so is one with no
    /// extension, as before.
    #[test]
    fn config_file_in_another_format_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("config.json", "{\"port\": 5000}"),
            ("config.json5", "{port: 5000}"),
            ("config.yaml", "port: 5000\n"),
            ("config.yml", "port: 5000\n"),
            ("config.ini", "port=5000\n"),
            ("config.ron", "(port: 5000)"),
            ("config", "port = 5000\n"),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            let cli = Cli {
                config_file: Some(path),
                ..Cli::default()
            };
            let err = Config::load_from(cli, &FakeEnv::new()).unwrap_err();
            assert!(
                err.to_string()
                    .contains("is not of a supported file format"),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn db_pool_defaults_to_auto() {
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.db_pool, None, "no knob set ⇒ auto sizing");
    }

    /// The extension kill-switch resolves env > file > off, and accepts the
    /// boolean spellings people actually write.
    ///
    /// `bool::from_str` takes ONLY `"true"`/`"false"`, so before
    /// [`parse_bool_var`] a `FERROFIN_DISABLE_EXTENSIONS=1` parsed as `None`,
    /// fell through to the default, and disabled nothing while reporting
    /// nothing. `suite/micro/serve.sh` had been setting exactly that for the
    /// whole life of the harness.
    #[test]
    fn disable_extensions_accepts_the_usual_boolean_spellings() {
        let load = |env: &FakeEnv| {
            Config::load_from(Cli::default(), env)
                .unwrap()
                .disable_extensions
        };
        for on in ["1", "true", "TRUE", "yes", "on"] {
            assert!(
                load(&FakeEnv::new().with("FERROFIN_DISABLE_EXTENSIONS", on)),
                "{on:?} must read as true"
            );
        }
        for off in ["0", "false", "no", "off"] {
            assert!(
                !load(&FakeEnv::new().with("FERROFIN_DISABLE_EXTENSIONS", off)),
                "{off:?} must read as false"
            );
        }
        // Unset defaults to OFF — extensions are a product feature; only the
        // benchmark turns them off.
        assert!(!load(&FakeEnv::new()));
        // Unparseable is ignored rather than guessed.
        assert!(!load(
            &FakeEnv::new().with("FERROFIN_DISABLE_EXTENSIONS", "maybe")
        ));

        // config.toml sets it, and env still wins.
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "disable_extensions = true\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert!(
            Config::load_from(cli(), &FakeEnv::new())
                .unwrap()
                .disable_extensions
        );
        assert!(
            !Config::load_from(
                cli(),
                &FakeEnv::new().with("FERROFIN_DISABLE_EXTENSIONS", "0")
            )
            .unwrap()
            .disable_extensions
        );
    }

    /// `FFmpeg:imgExtractPerfTradeoff` is off unless set; the env var beats
    /// `config.toml`.
    #[test]
    fn the_img_extract_perf_tradeoff_is_off_unless_set_and_env_beats_file() {
        const VAR: &str = "FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF";
        let load = |cli: Cli, env: &FakeEnv| {
            Config::load_from(cli, env)
                .unwrap()
                .ffmpeg_img_extract_perf_tradeoff
        };
        assert!(!load(Cli::default(), &FakeEnv::new()));
        assert!(load(Cli::default(), &FakeEnv::new().with(VAR, "1")));
        assert!(!load(Cli::default(), &FakeEnv::new().with(VAR, "maybe")));

        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "ffmpeg_img_extract_perf_tradeoff = true\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert!(load(cli(), &FakeEnv::new()));
        assert!(!load(cli(), &FakeEnv::new().with(VAR, "off")));
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

        // FERROFIN_ENABLE_METRICS env beats the file (can force off, too).
        let env = FakeEnv::new().with("FERROFIN_ENABLE_METRICS", "false");
        assert_eq!(
            Config::load_from(cli(), &env).unwrap().enable_metrics,
            Some(false)
        );
    }

    /// `FERROFIN_TVDB_CACHE_HOURS` beats `tvdb_cache_hours` in `config.toml`,
    /// and unset is the plugin's one hour.
    #[test]
    fn tvdb_cache_hours_env_beats_file() {
        let hours = |config: &Config| config.tvdb_cache_duration().as_secs() / 3600;
        let unset = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(unset.tvdb_cache_hours, None);
        assert_eq!(hours(&unset), 1, "the plugin's CacheDurationInHours");
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "tvdb_cache_hours = 6\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert_eq!(
            hours(&Config::load_from(cli(), &FakeEnv::new()).unwrap()),
            6,
            "the file's"
        );
        let env = FakeEnv::new().with("FERROFIN_TVDB_CACHE_HOURS", " 12 ");
        assert_eq!(
            hours(&Config::load_from(cli(), &env).unwrap()),
            12,
            "the env beats the file"
        );
        let blank = FakeEnv::new().with("FERROFIN_TVDB_CACHE_HOURS", "  ");
        assert_eq!(
            hours(&Config::load_from(cli(), &blank).unwrap()),
            6,
            "a blank env var is unset"
        );
    }

    /// A value that is not a whole number of hours above zero — from the
    /// env or the file — warns and falls back to the default, never stops
    /// the server.
    #[test]
    fn a_bad_tvdb_cache_hours_falls_back_to_the_default() {
        let cases: [(Option<&str>, &str); 9] = [
            (Some("0"), ""),
            (Some("-3"), ""),
            (Some("1.5"), ""),
            (Some("6h"), ""),
            (None, "tvdb_cache_hours = 0\n"),
            (None, "tvdb_cache_hours = -2\n"),
            (None, "tvdb_cache_hours = 1.5\n"),
            (None, "tvdb_cache_hours = \"6h\"\n"),
            (None, "tvdb_cache_hours = true\n"),
        ];
        for (env_value, file) in cases {
            let dir = tempfile::tempdir().unwrap();
            let toml = dir.path().join("config.toml");
            std::fs::write(&toml, file).unwrap();
            let cli = Cli {
                config_file: Some(toml),
                ..Cli::default()
            };
            let env = match env_value {
                Some(value) => FakeEnv::new().with("FERROFIN_TVDB_CACHE_HOURS", value),
                None => FakeEnv::new(),
            };
            let config = Config::load_from(cli, &env).unwrap();
            assert!(
                config.tvdb_cache_hours.is_some(),
                "kept as written: {env_value:?} {file:?}"
            );
            assert_eq!(
                config.tvdb_cache_duration(),
                ferrofin_providers::tvdb::DEFAULT_CACHE_DURATION,
                "{env_value:?} {file:?}"
            );
        }
    }

    /// `FERROFIN_TVDB_TOKEN_DAYS` beats `tvdb_token_days` in `config.toml`,
    /// unset is the plugin's 25 days, and a value that is not a whole number
    /// of days from 1 to 29 warns and falls back to it.
    #[test]
    fn tvdb_token_days_env_beats_file_and_bad_values_fall_back() {
        let days = |config: &Config| config.tvdb_token_max_age().as_secs() / 86_400;
        let unset = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(days(&unset), 25, "the plugin's LoginAsync");
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "tvdb_token_days = 7\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert_eq!(days(&Config::load_from(cli(), &FakeEnv::new()).unwrap()), 7);
        let env = FakeEnv::new().with("FERROFIN_TVDB_TOKEN_DAYS", " 29 ");
        assert_eq!(days(&Config::load_from(cli(), &env).unwrap()), 29);
        let blank = FakeEnv::new().with("FERROFIN_TVDB_TOKEN_DAYS", "  ");
        assert_eq!(days(&Config::load_from(cli(), &blank).unwrap()), 7);
        for bad in ["0", "30", "-1", "1.5", "25d"] {
            let env = FakeEnv::new().with("FERROFIN_TVDB_TOKEN_DAYS", bad);
            assert_eq!(days(&Config::load_from(cli(), &env).unwrap()), 25, "{bad}");
        }
        for bad in ["30", "1.5", "\"25d\"", "true"] {
            std::fs::write(&toml, format!("tvdb_token_days = {bad}\n")).unwrap();
            let config = Config::load_from(cli(), &FakeEnv::new()).unwrap();
            assert_eq!(days(&config), 25, "file {bad}");
        }
    }

    #[test]
    fn scan_duration_buckets_env_beats_file_and_defers_when_unset() {
        assert_eq!(
            Config::load_from(Cli::default(), &FakeEnv::new())
                .unwrap()
                .metrics_scan_duration_buckets,
            None,
            "unset: the default buckets"
        );
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        // Integers and floats mix in the TOML array.
        std::fs::write(&toml, "metrics_scan_duration_buckets = [0.5, 30, 3600]\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        assert_eq!(
            Config::load_from(cli(), &FakeEnv::new())
                .unwrap()
                .metrics_scan_duration_buckets,
            Some(vec![0.5, 30.0, 3600.0])
        );
        let env = FakeEnv::new().with("FERROFIN_METRICS_SCAN_DURATION_BUCKETS", " 0.1, 1 ,10");
        assert_eq!(
            Config::load_from(cli(), &env)
                .unwrap()
                .metrics_scan_duration_buckets,
            Some(vec![0.1, 1.0, 10.0])
        );
        // A typo is kept as NaN for the install to refuse (with a warning),
        // never dropped into a silently shorter list.
        let env = FakeEnv::new().with("FERROFIN_METRICS_SCAN_DURATION_BUCKETS", "0.1,1s,10");
        let buckets = Config::load_from(cli(), &env)
            .unwrap()
            .metrics_scan_duration_buckets
            .expect("set");
        assert_eq!(buckets.len(), 3);
        assert!(buckets[1].is_nan());
        assert!(ferrofin_core::scan_metrics::validate_duration_buckets(&buckets).is_err());
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
        let env = FakeEnv::new().with("FERROFIN_DB_POOL", "32");
        assert_eq!(Config::load_from(cli(), &env).unwrap().db_pool, Some(32));

        // Env `auto` explicitly restores the formula over a file pin.
        let env = FakeEnv::new().with("FERROFIN_DB_POOL", "auto");
        assert_eq!(Config::load_from(cli(), &env).unwrap().db_pool, None);

        // Empty env value is "unset", not an error (compose passes `${VAR:-}`).
        let env = FakeEnv::new().with("FERROFIN_DB_POOL", "");
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
    fn trickplay_concurrency_resolves_integers_auto_and_garbage() {
        let resolve =
            |env: FakeEnv| Config::load_from(Cli::default(), &env).map(|c| c.trickplay_concurrency);
        let env = |v: &str| FakeEnv::new().with("FERROFIN_TRICKPLAY_CONCURRENCY", v);
        assert_eq!(resolve(FakeEnv::new()).unwrap(), None, "unset is auto");
        assert_eq!(resolve(env("auto")).unwrap(), None);
        assert_eq!(resolve(env("AUTO")).unwrap(), None);
        assert_eq!(resolve(env("")).unwrap(), None, "empty is unset");
        assert_eq!(resolve(env("0")).unwrap(), None, "zero is auto");
        assert_eq!(resolve(env("-3")).unwrap(), None, "negative is auto");
        assert_eq!(resolve(env("2")).unwrap(), Some(2));
        let err = resolve(env("lots")).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid FERROFIN_TRICKPLAY_CONCURRENCY")
        );

        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        let file = |body: &str| {
            std::fs::write(&toml, body).unwrap();
            let cli = Cli {
                config_file: Some(toml.clone()),
                ..Cli::default()
            };
            Config::load_from(cli, &FakeEnv::new()).map(|c| c.trickplay_concurrency)
        };
        assert_eq!(file("trickplay_concurrency = 3\n").unwrap(), Some(3));
        assert_eq!(file("trickplay_concurrency = \"auto\"\n").unwrap(), None);
        assert_eq!(file("trickplay_concurrency = 0\n").unwrap(), None);
        assert_eq!(file("trickplay_concurrency = -1\n").unwrap(), None);
        let err = file("trickplay_concurrency = \"many\"\n").unwrap_err();
        assert!(err.to_string().contains("invalid trickplay_concurrency"));

        // Env beats the file.
        std::fs::write(&toml, "trickplay_concurrency = 3\n").unwrap();
        let cli = Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env("auto")).unwrap();
        assert_eq!(cfg.trickplay_concurrency, None);
    }

    #[test]
    fn db_pool_env_rejects_garbage() {
        let env = FakeEnv::new().with("FERROFIN_DB_POOL", "many");
        let err = Config::load_from(Cli::default(), &env).unwrap_err();
        assert!(err.to_string().contains("invalid FERROFIN_DB_POOL"));
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
    fn quick_connect_pending_per_ip_env_beats_file() {
        let unset = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(unset.quick_connect_pending_per_ip, None);
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(&toml, "quick_connect_pending_per_ip = 4\n").unwrap();
        let cli = || Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        let from_file = Config::load_from(cli(), &FakeEnv::new()).unwrap();
        assert_eq!(from_file.quick_connect_pending_per_ip, Some(4));
        let env = FakeEnv::new().with("FERROFIN_QUICK_CONNECT_PENDING_PER_IP", "20");
        let from_env = Config::load_from(cli(), &env).unwrap();
        assert_eq!(from_env.quick_connect_pending_per_ip, Some(20));
    }

    #[test]
    fn wasm_settings_resolve_from_env_and_file() {
        // Unset ⇒ None (ferrofin-wasm applies its 30/128/256 defaults).
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.wasm_call_timeout_secs, None);
        assert_eq!(cfg.wasm_memory_limit_mb, None);
        assert_eq!(cfg.wasm_event_queue_capacity, None);
        assert_eq!(cfg.max_plugin_download_mb, None);

        // The plugin-download cap resolves from env like the others.
        let env = FakeEnv::new().with("FERROFIN_MAX_PLUGIN_DOWNLOAD_MB", "256");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.max_plugin_download_mb, Some(256));

        // So does the analysis decode budget.
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.wasm_analysis_concurrency, None, "default = cores/4");
        let env = FakeEnv::new().with("FERROFIN_WASM_ANALYSIS_CONCURRENCY", "2");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.wasm_analysis_concurrency, Some(2));

        // And the per-plugin state cap.
        let env = FakeEnv::new().with("FERROFIN_WASM_STATE_LIMIT_MB", "64");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.wasm_state_limit_mb, Some(64));

        // And the four write/extraction/artwork caps (unset ⇒ None ⇒ the
        // ferrofin-wasm 20/30/2/10 defaults).
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.wasm_image_download_mb, None);
        assert_eq!(cfg.wasm_image_timeout_secs, None);
        assert_eq!(cfg.wasm_write_content_mb, None);
        assert_eq!(cfg.wasm_subtitle_extract_mb, None);
        let env = FakeEnv::new()
            .with("FERROFIN_WASM_IMAGE_DOWNLOAD_MB", "40")
            .with("FERROFIN_WASM_IMAGE_TIMEOUT_SECS", "60")
            .with("FERROFIN_WASM_WRITE_CONTENT_MB", "4")
            .with("FERROFIN_WASM_SUBTITLE_EXTRACT_MB", "20");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.wasm_image_download_mb, Some(40));
        assert_eq!(cfg.wasm_image_timeout_secs, Some(60));
        assert_eq!(cfg.wasm_write_content_mb, Some(4));
        assert_eq!(cfg.wasm_subtitle_extract_mb, Some(20));

        // Env values apply.
        let env = FakeEnv::new()
            .with("FERROFIN_WASM_CALL_TIMEOUT_SECS", "10")
            .with("FERROFIN_WASM_MEMORY_LIMIT_MB", "64")
            .with("FERROFIN_WASM_EVENT_QUEUE_CAPACITY", "32");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.wasm_call_timeout_secs, Some(10));
        assert_eq!(cfg.wasm_memory_limit_mb, Some(64));
        assert_eq!(cfg.wasm_event_queue_capacity, Some(32));

        // config.toml supplies them; env beats file.
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(
            &toml,
            "wasm_call_timeout_secs = 5\nwasm_memory_limit_mb = 256\n",
        )
        .unwrap();
        let env = FakeEnv::new().with("FERROFIN_WASM_CALL_TIMEOUT_SECS", "15");
        let cli = Cli {
            config_file: Some(toml),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.wasm_call_timeout_secs, Some(15), "env beats file");
        assert_eq!(cfg.wasm_memory_limit_mb, Some(256), "file applies");

        // The private-HTTP allowlist resolves env > file, empty = unset.
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.wasm_private_http_allow, None, "deny by default");
        let env = FakeEnv::new().with("FERROFIN_WASM_PRIVATE_HTTP_ALLOW", "*");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.wasm_private_http_allow.as_deref(), Some("*"));
        let env = FakeEnv::new().with("FERROFIN_WASM_PRIVATE_HTTP_ALLOW", "  ");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.wasm_private_http_allow, None, "blank is unset");
    }

    #[test]
    fn cache_ttls_resolve_and_reject_values_out_of_range() {
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(
            (cfg.auth_cache_ttl_secs, cfg.row_cache_ttl_secs),
            (None, None)
        );
        let env = FakeEnv::new()
            .with("FERROFIN_AUTH_CACHE_TTL_SECS", "5")
            .with("FERROFIN_ROW_CACHE_TTL_SECS", "300");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(
            (cfg.auth_cache_ttl_secs, cfg.row_cache_ttl_secs),
            (Some(5), Some(300))
        );
        for key in [
            "FERROFIN_AUTH_CACHE_TTL_SECS",
            "FERROFIN_ROW_CACHE_TTL_SECS",
        ] {
            for value in ["0", "4", "301", "soon"] {
                let err = Config::load_from(Cli::default(), &FakeEnv::new().with(key, value))
                    .unwrap_err()
                    .to_string();
                assert!(err.contains(key), "{key}={value}: {err}");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        std::fs::write(
            &toml,
            "auth_cache_ttl_secs = 60
row_cache_ttl_secs = 10
",
        )
        .unwrap();
        let cli = Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        let cfg = Config::load_from(cli, &FakeEnv::new()).unwrap();
        assert_eq!(
            (cfg.auth_cache_ttl_secs, cfg.row_cache_ttl_secs),
            (Some(60), Some(10))
        );
        std::fs::write(
            &toml,
            "row_cache_ttl_secs = 1000
",
        )
        .unwrap();
        let cli = Cli {
            config_file: Some(toml),
            ..Cli::default()
        };
        let err = Config::load_from(cli, &FakeEnv::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("row_cache_ttl_secs"), "{err}");
    }

    #[test]
    fn scan_directory_cache_limits_resolve_and_reject_zero() {
        // Unset → None (the scanner keeps upstream's values).
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.scan_directory_cache_records, None);
        assert_eq!(cfg.scan_directory_cache_idle_secs, None);

        let env = FakeEnv::new()
            .with("FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS", "5000")
            .with("FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS", "60");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.scan_directory_cache_records, Some(5000));
        assert_eq!(cfg.scan_directory_cache_idle_secs, Some(60));

        for (key, value) in [
            ("FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS", "0"),
            ("FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS", "lots"),
            ("FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS", "0"),
            ("FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS", "-5"),
        ] {
            let err = Config::load_from(Cli::default(), &FakeEnv::new().with(key, value))
                .unwrap_err()
                .to_string();
            assert!(err.contains(key), "{key}={value}: {err}");
        }

        let dir = tempfile::tempdir().unwrap();
        let toml = dir.path().join("config.toml");
        let load = |body: &str| {
            std::fs::write(&toml, body).unwrap();
            let cli = Cli {
                config_file: Some(toml.clone()),
                ..Cli::default()
            };
            Config::load_from(cli, &FakeEnv::new())
        };
        let cfg =
            load("scan_directory_cache_records = 7\nscan_directory_cache_idle_secs = 9\n").unwrap();
        assert_eq!(cfg.scan_directory_cache_records, Some(7));
        assert_eq!(cfg.scan_directory_cache_idle_secs, Some(9));
        let err = load("scan_directory_cache_idle_secs = 0\n").unwrap_err();
        assert!(
            err.to_string().contains("scan_directory_cache_idle_secs"),
            "{err}"
        );
        // Env wins over the file.
        std::fs::write(&toml, "scan_directory_cache_records = 7\n").unwrap();
        let cli = Cli {
            config_file: Some(toml.clone()),
            ..Cli::default()
        };
        let env = FakeEnv::new().with("FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS", "11");
        let cfg = Config::load_from(cli, &env).unwrap();
        assert_eq!(cfg.scan_directory_cache_records, Some(11));
    }

    #[test]
    fn scan_progress_every_resolves_from_env_and_preserves_zero() {
        // Env value is used verbatim.
        let env = FakeEnv::new().with("FERROFIN_SCAN_PROGRESS_EVERY", "250");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.scan_progress_every, Some(250));

        // Unset → None (the scanner applies its 100-item default).
        let cfg = Config::load_from(Cli::default(), &FakeEnv::new()).unwrap();
        assert_eq!(cfg.scan_progress_every, None);

        // 0 is preserved (disables progress logs), not treated as unset.
        let env = FakeEnv::new().with("FERROFIN_SCAN_PROGRESS_EVERY", "0");
        let cfg = Config::load_from(Cli::default(), &env).unwrap();
        assert_eq!(cfg.scan_progress_every, Some(0));
    }

    /// A test server's scan never reaches a public metadata provider:
    /// `test_stub` points each one a scan reaches at [`OFFLINE_PROVIDER_URL`],
    /// where nothing answers, so its requests fail at once instead of waiting
    /// on the network (a three-library scan once spent a minute on 181
    /// requests to the live providers).
    #[test]
    fn test_stub_keeps_every_remote_provider_offline() {
        let cfg = Config::test_stub(Path::new("/tmp/ferrofin"));
        // Exhaustive: a new provider field fails to compile here until it is
        // listed, and the assertion below then requires it offline.
        let ProviderEndpoints {
            tmdb,
            tmdb_images,
            omdb,
            opensubtitles,
            tvdb,
            fanart,
            audiodb,
            lrclib,
        } = cfg.provider_endpoints;
        for endpoint in [
            tmdb,
            tmdb_images,
            omdb,
            opensubtitles,
            tvdb,
            fanart,
            audiodb,
            lrclib,
        ] {
            assert_eq!(endpoint.as_deref(), Some(OFFLINE_PROVIDER_URL));
        }
        assert_eq!(cfg.musicbrainz_base_url, OFFLINE_PROVIDER_URL);
        assert_eq!(cfg.studios_repo_url, OFFLINE_PROVIDER_URL);
        let addr = OFFLINE_PROVIDER_URL
            .strip_prefix("http://")
            .expect("a plain http URL");
        assert!(
            std::net::TcpStream::connect(addr).is_err(),
            "something answers on {OFFLINE_PROVIDER_URL}, so test servers would reach it"
        );
    }
}
