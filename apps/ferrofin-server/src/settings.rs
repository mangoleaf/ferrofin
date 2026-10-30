//! The Ferrofin settings page's server side: [`SETTINGS`], one row per
//! bootstrap setting (CLI flag, `FERROFIN_*` environment variable,
//! `config.toml` key), and [`FerrofinServerSettings`], which reports each
//! one's running value and where it comes from over `GET /Ferrofin/Settings`.
//!
//! The table is the one description of the settings that the report, the
//! page and saving read. Its tests tie it to what the loader reads
//! ([`Config::load`]): every `config.toml` key has a row and every row's key
//! is one, every row's variable has a row in `docs/CONFIG.md`, every key
//! round-trips through `config.toml`, the source a row reports is the layer
//! the loader applied, every flag is a row's, and the rows that apply only on
//! a process restart are what an in-process restart keeps
//! ([`Config::for_restart`]).
//!
//! Precedence is the loader's: a flag, else the variable, else `config.toml`,
//! else the default. A value a flag or a variable sets is locked on the page
//! (owner decision D1 of the settings-page milestone): one saved to
//! `config.toml` underneath would not apply while it is set.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ferrofin_core::system_manager::LifecycleController;
use ferrofin_model::server_settings::{
    ServerSettingApplies as Applies, ServerSettingDto, ServerSettingGroup as Group,
    ServerSettingKind, ServerSettingSource as Source, ServerSettingsDto,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::server_settings::ServerSettingsManager;
use serde_json::{Value, json};

use crate::config::{self, Cli, Config, Env, LogFormat};

/// The kind of value a setting holds, with an integer's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// On or off.
    Bool,
    /// A whole number from `min` to `max`.
    Integer {
        /// The smallest value the loader accepts.
        min: i64,
        /// The largest value the loader accepts.
        max: i64,
    },
    /// A whole number, or `auto`.
    CountOrAuto,
    /// Free text.
    Text,
    /// A URL.
    Url,
    /// A filesystem path.
    Path,
    /// A list of filesystem paths.
    PathList,
    /// A list of numbers.
    NumberList,
    /// A key, a PIN or a password: shown to admins, never logged.
    Secret,
}

impl SettingKind {
    /// The kind as the settings page receives it, and an integer's bounds.
    fn to_dto(self) -> (ServerSettingKind, Option<i64>, Option<i64>) {
        match self {
            Self::Bool => (ServerSettingKind::Bool, None, None),
            Self::Integer { min, max } => (ServerSettingKind::Integer, Some(min), Some(max)),
            Self::CountOrAuto => (ServerSettingKind::CountOrAuto, None, None),
            Self::Text => (ServerSettingKind::Text, None, None),
            Self::Url => (ServerSettingKind::Url, None, None),
            Self::Path => (ServerSettingKind::Path, None, None),
            Self::PathList => (ServerSettingKind::PathList, None, None),
            Self::NumberList => (ServerSettingKind::NumberList, None, None),
            Self::Secret => (ServerSettingKind::Secret, None, None),
        }
    }
}

/// Whether the settings page may change a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingAccess {
    /// Editable from the page, unless a flag or a variable sets it.
    Editable,
    /// Never from the page; the note says why, or where it is changed instead.
    ReadOnly(&'static str),
    /// An environment variable `config.toml` has no key for.
    EnvOnly,
}

/// How the loader reads a setting's variable, which decides when the
/// variable is where the value in use comes from.
#[derive(Debug, Clone, Copy)]
pub enum EnvRead {
    /// Any value applies, a blank one included: a blank key or path is set,
    /// not unset.
    Raw,
    /// A blank value is unset; any other applies.
    NonBlank,
    /// The value applies when the function says the loader takes it: a blank
    /// one is unset, and one that does not parse is ignored with a warning,
    /// so `config.toml` or the default applies.
    Parsed(fn(&str) -> bool),
}

/// One bootstrap setting, as the settings page describes it.
#[derive(Debug, Clone, Copy)]
pub struct SettingSpec {
    /// The `config.toml` key, exactly a `FileConfig` field; for an
    /// [`SettingAccess::EnvOnly`] row, which has none, its variable.
    pub key: &'static str,
    /// The environment variable that sets it.
    pub env: Option<&'static str>,
    /// How the loader reads [`env`](Self::env).
    pub env_read: EnvRead,
    /// The command-line flag that sets it, e.g. `--port`.
    pub flag: Option<&'static str>,
    /// The group the page lists it under.
    pub group: Group,
    /// The kind of value it holds.
    pub kind: SettingKind,
    /// Whether the page may change it.
    pub access: SettingAccess,
    /// What a change needs before it applies.
    pub applies: Applies,
    /// The value that applies when nothing sets it, as the page shows it.
    pub default: &'static str,
    /// What it does, in one line.
    pub description: &'static str,
    /// The value in use, secrets included (owner decision D3: admins see
    /// them, as on Jellyfin's plugin pages). `null` when unset and the
    /// default applies.
    pub running: fn(&Config) -> Value,
}

/// The note of the directories the server is started on.
const SET_AT_STARTUP: &str =
    "Set at startup. Changing it from the web could point the server at other files.";

/// The note of the executables: Jellyfin refuses the same change over its API
/// (`EncodingConfigurationStore.Validate`, "Unable to update encoder app path.").
const NO_EXECUTABLE_FROM_THE_WEB: &str = "The web may not choose an executable the server runs.";

/// The note of the settings read only when the database is created.
const FIRST_BOOT_ONLY: &str = "Only used when the database is created.";

/// The note of the OpenTelemetry variables.
const ENV_ONLY: &str = "OpenTelemetry's standard variable, read from the environment only: \
                        config.toml has no key for it. Set it where the server's environment is \
                        defined (compose file, Helm values, service unit) and restart the server \
                        process.";

/// `RUST_LOG`, which outranks `FERROFIN_LOG` and `log_level` when it holds a
/// filter (`bootstrap::init_tracing`).
const RUST_LOG: &str = "RUST_LOG";

/// The largest `u32`, the bound of the settings the loader reads as one.
const U32_MAX: i64 = 4_294_967_295;

/// An integer the loader reads as a `u32`.
const U32: SettingKind = SettingKind::Integer {
    min: 0,
    max: U32_MAX,
};

/// A count the loader requires to be positive (`resolve_positive`).
const POSITIVE: SettingKind = SettingKind::Integer {
    min: 1,
    max: U32_MAX,
};

/// A port.
const PORT: SettingKind = SettingKind::Integer {
    min: 0,
    max: 65_535,
};

/// The cache lifetimes' bounds ([`config::CACHE_TTL_RANGE_SECS`]).
const CACHE_TTL: SettingKind = SettingKind::Integer {
    min: *config::CACHE_TTL_RANGE_SECS.start() as i64,
    max: *config::CACHE_TTL_RANGE_SECS.end() as i64,
};

/// [`EnvRead::Parsed`] for a variable the loader reads as a `u32`.
const PARSED_U32: EnvRead = EnvRead::Parsed(parses::<u32>);

/// [`EnvRead::Parsed`] for an on/off variable.
const PARSED_BOOL: EnvRead = EnvRead::Parsed(parses_as_bool);

/// What every row starts from: an editable, unflagged setting whose variable
/// applies whatever it holds. Each row names its key, variable, group, kind,
/// default, description and running value.
const ROW: SettingSpec = SettingSpec {
    key: "",
    env: None,
    env_read: EnvRead::Raw,
    flag: None,
    group: Group::Library,
    kind: SettingKind::Text,
    access: SettingAccess::Editable,
    applies: Applies::Restart,
    default: "",
    description: "",
    running: |_| Value::Null,
};

/// Every bootstrap setting, in the order the page lists them.
// One row per setting: the length is the table's.
pub static SETTINGS: &[SettingSpec] = &[
    // ---- Paths ------------------------------------------------------------
    SettingSpec {
        key: "data_dir",
        env: Some("FERROFIN_DATA_DIR"),
        flag: Some("--data-dir"),
        group: Group::Paths,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(SET_AT_STARTUP),
        applies: Applies::ProcessRestart,
        default: "$XDG_DATA_HOME/ferrofin, else /var/lib/ferrofin",
        description: "Program-data root: holds the database and derives the directories below.",
        running: |c| json!(c.data_dir),
        ..ROW
    },
    SettingSpec {
        key: "config_dir",
        env: Some("FERROFIN_CONFIG_DIR"),
        group: Group::Paths,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(SET_AT_STARTUP),
        applies: Applies::ProcessRestart,
        default: "{data_dir}/config",
        description: "Holds system.json and branding.json.",
        running: |c| json!(c.config_dir),
        ..ROW
    },
    SettingSpec {
        key: "cache_dir",
        env: Some("FERROFIN_CACHE_DIR"),
        group: Group::Paths,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(SET_AT_STARTUP),
        applies: Applies::ProcessRestart,
        default: "{data_dir}/cache",
        description: "Cache root, the transcode and segment cache included.",
        running: |c| json!(c.cache_dir),
        ..ROW
    },
    SettingSpec {
        key: "web_dir",
        env: Some("FERROFIN_WEB_DIR"),
        group: Group::Paths,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(SET_AT_STARTUP),
        default: "{data_dir}/web",
        description: "The built jellyfin-web served at /web; without it the server is API-only.",
        running: |c| json!(c.web_dir),
        ..ROW
    },
    // ---- Network and identity ---------------------------------------------
    SettingSpec {
        key: "bind_addr",
        env: Some("FERROFIN_BIND_ADDR"),
        flag: Some("--bind"),
        group: Group::Network,
        access: SettingAccess::ReadOnly(
            "Dashboard → Networking → Local network addresses decides which addresses the \
             server listens on, unless this is set.",
        ),
        default: "Dashboard → Networking decides",
        description: "The one address the HTTP listener binds to.",
        // Unset, the Networking settings decide, not the 0.0.0.0 fallback.
        running: |c| {
            if c.bind_addr_explicit {
                json!(c.bind_addr)
            } else {
                Value::Null
            }
        },
        ..ROW
    },
    // The port, HTTPS port and base URL notes name their key and variable
    // (owner decision D2a) until the Networking page drives them: gl-554,
    // gl-335 and gl-555 switch each note to that page.
    SettingSpec {
        key: "port",
        env: Some("FERROFIN_PORT"),
        env_read: EnvRead::Parsed(parses::<u16>),
        flag: Some("--port"),
        group: Group::Network,
        kind: PORT,
        access: SettingAccess::ReadOnly(
            "Set it with the port key in config.toml, the FERROFIN_PORT environment variable or \
             the --port flag. The port on Dashboard → Networking is not read yet; it will take \
             over.",
        ),
        default: "8096",
        description: "HTTP port.",
        running: |c| json!(c.port),
        ..ROW
    },
    SettingSpec {
        key: "https_port",
        env: Some("FERROFIN_HTTPS_PORT"),
        env_read: EnvRead::Parsed(parses::<u16>),
        group: Group::Network,
        kind: PORT,
        access: SettingAccess::ReadOnly(
            "Set it with the https_port key in config.toml or the FERROFIN_HTTPS_PORT \
             environment variable. The HTTPS port on Dashboard → Networking is not read yet; it \
             will take over.",
        ),
        default: "8920",
        description: "HTTPS port, served when Dashboard → Networking enables HTTPS.",
        running: |c| json!(c.https_port),
        ..ROW
    },
    SettingSpec {
        key: "published_url",
        env: Some("FERROFIN_PUBLISHED_URL"),
        flag: Some("--published-url"),
        group: Group::Network,
        kind: SettingKind::Url,
        access: SettingAccess::ReadOnly(
            "Dashboard → Networking → Published server URIs apply per subnet unless this is \
             set; when it is set, it overrides them all, as in Jellyfin.",
        ),
        default: "auto-detect",
        description: "Public server URL advertised to clients.",
        running: |c| json!(c.published_url),
        ..ROW
    },
    SettingSpec {
        key: "base_url",
        env: Some("FERROFIN_BASE_URL"),
        group: Group::Network,
        access: SettingAccess::ReadOnly(
            "Set it with the base_url key in config.toml or the FERROFIN_BASE_URL environment \
             variable. The base URL on Dashboard → Networking is not read yet; it will take \
             over.",
        ),
        default: "none",
        description: "URL path prefix the server is mounted under.",
        running: |c| json!(c.base_url),
        ..ROW
    },
    SettingSpec {
        key: "server_name",
        env: Some("FERROFIN_SERVER_NAME"),
        group: Group::Network,
        access: SettingAccess::ReadOnly(
            "Dashboard → General → Server name. This value is used only when that is empty.",
        ),
        default: "the host name",
        description: "Server name reported to clients.",
        running: |c| json!(c.server_name),
        ..ROW
    },
    SettingSpec {
        key: "websocket_min_stream_period_ms",
        env: Some("FERROFIN_WS_MIN_STREAM_PERIOD_MS"),
        env_read: PARSED_U32,
        group: Group::Network,
        kind: U32,
        default: "100",
        description: "Smallest period, in milliseconds, between pushes of a dashboard WebSocket \
                      stream.",
        running: |c| json!(c.websocket_min_stream_period_ms),
        ..ROW
    },
    SettingSpec {
        key: "quick_connect_pending_per_ip",
        env: Some("FERROFIN_QUICK_CONNECT_PENDING_PER_IP"),
        env_read: PARSED_U32,
        group: Group::Network,
        kind: U32,
        default: "10",
        description: "Unauthorized Quick Connect requests one client IP may hold at once; 0 \
                      means the default.",
        running: |c| json!(c.quick_connect_pending_per_ip),
        ..ROW
    },
    // ---- ffmpeg -----------------------------------------------------------
    SettingSpec {
        key: "ffmpeg_path",
        env: Some("FERROFIN_FFMPEG_PATH"),
        flag: Some("--ffmpeg"),
        group: Group::Ffmpeg,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(NO_EXECUTABLE_FROM_THE_WEB),
        default: "found on $PATH",
        description: "Path to ffmpeg. Without ffmpeg there is no transcoding.",
        running: |c| json!(c.ffmpeg_path),
        ..ROW
    },
    SettingSpec {
        key: "ffprobe_path",
        env: Some("FERROFIN_FFPROBE_PATH"),
        group: Group::Ffmpeg,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(NO_EXECUTABLE_FROM_THE_WEB),
        default: "found on $PATH",
        description: "Path to ffprobe, which library scans run.",
        running: |c| json!(c.ffprobe_path),
        ..ROW
    },
    SettingSpec {
        key: "ffmpeg_img_extract_perf_tradeoff",
        env: Some("FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF"),
        env_read: PARSED_BOOL,
        group: Group::Ffmpeg,
        kind: SettingKind::Bool,
        default: "false",
        description: "Extract chapter images and thumbnails from keyframes only: faster, and \
                      the image may be less typical of the scene.",
        running: |c| json!(c.ffmpeg_img_extract_perf_tradeoff),
        ..ROW
    },
    // ---- Library and database ---------------------------------------------
    SettingSpec {
        key: "library_roots",
        env: Some("FERROFIN_LIBRARY_ROOTS"),
        kind: SettingKind::PathList,
        access: SettingAccess::ReadOnly(FIRST_BOOT_ONLY),
        default: "none",
        description: "Folders a fresh install starts with as libraries.",
        running: |c| json!(c.library_roots),
        ..ROW
    },
    SettingSpec {
        key: "admin_user",
        env: Some("FERROFIN_ADMIN_USER"),
        access: SettingAccess::ReadOnly(FIRST_BOOT_ONLY),
        default: "admin",
        description: "Username of the administrator seeded on first boot.",
        running: |c| json!(c.admin_user),
        ..ROW
    },
    SettingSpec {
        key: "admin_password",
        env: Some("FERROFIN_ADMIN_PASSWORD"),
        kind: SettingKind::Secret,
        access: SettingAccess::ReadOnly(FIRST_BOOT_ONLY),
        default: "none: the administrator is seeded without a password",
        description: "Password of the administrator seeded on first boot.",
        running: |c| json!(c.admin_password),
        ..ROW
    },
    SettingSpec {
        key: "db_pool",
        env: Some("FERROFIN_DB_POOL"),
        env_read: EnvRead::NonBlank,
        kind: SettingKind::CountOrAuto,
        default: "auto",
        description: "SQLite connection count, or auto to size it to the cores.",
        running: |c| json!(c.db_pool),
        ..ROW
    },
    SettingSpec {
        key: "scan_probe_concurrency",
        env: Some("FERROFIN_SCAN_PROBE_CONCURRENCY"),
        env_read: PARSED_U32,
        kind: U32,
        default: "8, capped at the cores",
        description: "ffprobe processes a library scan keeps in flight. Keep it low on a \
                      spinning disk or a network mount.",
        running: |c| json!(c.scan_probe_concurrency),
        ..ROW
    },
    SettingSpec {
        key: "scan_progress_every",
        env: Some("FERROFIN_SCAN_PROGRESS_EVERY"),
        env_read: PARSED_U32,
        kind: U32,
        default: "100",
        description: "Items between scan-progress log lines; 0 turns them off.",
        running: |c| json!(c.scan_progress_every),
        ..ROW
    },
    SettingSpec {
        key: "scan_directory_cache_records",
        env: Some("FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS"),
        env_read: EnvRead::NonBlank,
        kind: POSITIVE,
        default: "100000",
        description: "Directory-listing records one library scan keeps before it lists folders \
                      again.",
        running: |c| json!(c.scan_directory_cache_records),
        ..ROW
    },
    SettingSpec {
        key: "scan_directory_cache_idle_secs",
        env: Some("FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS"),
        env_read: EnvRead::NonBlank,
        kind: POSITIVE,
        default: "300",
        description: "Seconds a library scan's directory listings survive unread.",
        running: |c| json!(c.scan_directory_cache_idle_secs),
        ..ROW
    },
    SettingSpec {
        key: "auth_cache_ttl_secs",
        env: Some("FERROFIN_AUTH_CACHE_TTL_SECS"),
        env_read: EnvRead::NonBlank,
        kind: CACHE_TTL,
        default: "30",
        description: "Seconds a resolved access token is served from memory; only bounds \
                      changes made outside the server.",
        running: |c| json!(c.auth_cache_ttl_secs),
        ..ROW
    },
    SettingSpec {
        key: "row_cache_ttl_secs",
        env: Some("FERROFIN_ROW_CACHE_TTL_SECS"),
        env_read: EnvRead::NonBlank,
        kind: CACHE_TTL,
        default: "30",
        description: "Seconds the access check serves users' and libraries' rows from memory; \
                      only bounds changes made outside the server.",
        running: |c| json!(c.row_cache_ttl_secs),
        ..ROW
    },
    SettingSpec {
        key: "trickplay_concurrency",
        env: Some("FERROFIN_TRICKPLAY_CONCURRENCY"),
        env_read: EnvRead::NonBlank,
        kind: SettingKind::CountOrAuto,
        default: "auto",
        description: "Trickplay images generated at once, or auto for the image-encoding limit.",
        running: |c| json!(c.trickplay_concurrency),
        ..ROW
    },
    SettingSpec {
        key: "playback_metrics_queue",
        env: Some("FERROFIN_PLAYBACK_METRICS_QUEUE"),
        env_read: PARSED_U32,
        kind: U32,
        default: "1024",
        description: "Depth of the playback-metrics write queue, in events.",
        running: |c| json!(c.playback_metrics_queue),
        ..ROW
    },
    // ---- Observability ----------------------------------------------------
    SettingSpec {
        key: "log_level",
        env: Some("FERROFIN_LOG"),
        group: Group::Observability,
        applies: Applies::ProcessRestart,
        default: "info",
        description: "Log filter, in RUST_LOG syntax; RUST_LOG wins when it is set.",
        running: |c| json!(c.log_level),
        ..ROW
    },
    SettingSpec {
        key: "log_format",
        env: Some("FERROFIN_LOG_FORMAT"),
        env_read: EnvRead::NonBlank,
        group: Group::Observability,
        applies: Applies::ProcessRestart,
        default: "json",
        description: "Stdout log lines: json, or text for running the server by hand.",
        running: |c| {
            json!(match c.log_format {
                LogFormat::Json => "json",
                LogFormat::Text => "text",
            })
        },
        ..ROW
    },
    SettingSpec {
        key: "enable_metrics",
        env: Some("FERROFIN_ENABLE_METRICS"),
        env_read: PARSED_BOOL,
        group: Group::Observability,
        kind: SettingKind::Bool,
        access: SettingAccess::ReadOnly(
            "Dashboard → General → Enable metrics, unless this is set.",
        ),
        default: "Dashboard → General decides",
        description: "Serve Prometheus /metrics, whatever the dashboard says.",
        running: |c| json!(c.enable_metrics),
        ..ROW
    },
    SettingSpec {
        key: "metrics_sample_interval",
        env: Some("FERROFIN_METRICS_SAMPLE_INTERVAL"),
        env_read: PARSED_U32,
        group: Group::Observability,
        kind: U32,
        default: "15",
        description: "Seconds between process-metric samples; 0 means the default.",
        running: |c| json!(c.metrics_sample_interval),
        ..ROW
    },
    SettingSpec {
        key: "metrics_scan_duration_buckets",
        env: Some("FERROFIN_METRICS_SCAN_DURATION_BUCKETS"),
        env_read: EnvRead::NonBlank,
        group: Group::Observability,
        kind: SettingKind::NumberList,
        applies: Applies::ProcessRestart,
        default: "1–2.5–5 per decade, 0.001 to 5000",
        description: "Bucket boundaries, in seconds, of the library-scan duration histograms.",
        running: |c| json!(c.metrics_scan_duration_buckets),
        ..ROW
    },
    SettingSpec {
        key: "shutdown_timeout_secs",
        env: Some("FERROFIN_SHUTDOWN_TIMEOUT_SECS"),
        env_read: PARSED_U32,
        group: Group::Observability,
        kind: U32,
        default: "30",
        description: "Seconds a shutdown or restart waits for requests in flight.",
        running: |c| json!(c.shutdown_timeout_secs),
        ..ROW
    },
    SettingSpec {
        key: "disable_extensions",
        env: Some("FERROFIN_DISABLE_EXTENSIONS"),
        env_read: PARSED_BOOL,
        group: Group::Observability,
        kind: SettingKind::Bool,
        // Not "would remove this page": owner decision D4 keeps the Ferrofin
        // settings extension registered whatever this says.
        access: SettingAccess::ReadOnly(
            "A measurement switch: it removes the other compiled-in extensions, with their \
             tasks and hooks, so a benchmark compares Ferrofin with a Jellyfin that has no \
             plugins. Set it where the benchmark starts the server.",
        ),
        default: "false",
        description: "Register none of the compiled-in extensions but this one.",
        running: |c| json!(c.disable_extensions),
        ..ROW
    },
    SettingSpec {
        key: "OTEL_EXPORTER_OTLP_ENDPOINT",
        env: Some("OTEL_EXPORTER_OTLP_ENDPOINT"),
        // A blank endpoint exports nothing (`bootstrap::build_tracer_provider`).
        env_read: EnvRead::NonBlank,
        group: Group::Observability,
        kind: SettingKind::Url,
        access: SettingAccess::EnvOnly,
        applies: Applies::ProcessRestart,
        default: "none: no trace export",
        description: "Export traces over OTLP (gRPC) to this endpoint.",
        ..ROW
    },
    SettingSpec {
        key: "OTEL_TRACES_SAMPLER_ARG",
        env: Some("OTEL_TRACES_SAMPLER_ARG"),
        env_read: EnvRead::Parsed(parses_as_ratio),
        group: Group::Observability,
        access: SettingAccess::EnvOnly,
        applies: Applies::ProcessRestart,
        default: "0.25",
        description: "Share of traces exported when trace export is on, 0 to 1.",
        ..ROW
    },
    // ---- Metadata providers -----------------------------------------------
    SettingSpec {
        key: "omdb_api_key",
        env: Some("FERROFIN_OMDB_KEY"),
        group: Group::Providers,
        kind: SettingKind::Secret,
        default: "Jellyfin's shared key",
        description: "OMDb API key override; blank uses Jellyfin's shared key.",
        running: |c| json!(c.omdb_api_key),
        ..ROW
    },
    SettingSpec {
        key: "tvdb_api_key",
        env: Some("FERROFIN_TVDB_KEY"),
        group: Group::Providers,
        kind: SettingKind::Secret,
        default: "Jellyfin's shared key",
        description: "TheTVDB API key override; blank uses Jellyfin's shared key.",
        running: |c| json!(c.tvdb_api_key),
        ..ROW
    },
    SettingSpec {
        key: "tvdb_subscriber_pin",
        env: Some("FERROFIN_TVDB_PIN"),
        group: Group::Providers,
        kind: SettingKind::Secret,
        default: "none",
        description: "TheTVDB subscriber PIN, for a paid subscription.",
        running: |c| json!(c.tvdb_subscriber_pin),
        ..ROW
    },
    SettingSpec {
        key: "fanart_personal_api_key",
        env: Some("FERROFIN_FANART_KEY"),
        group: Group::Providers,
        kind: SettingKind::Secret,
        default: "none",
        description: "fanart.tv personal key, sent alongside the shared project key.",
        running: |c| json!(c.fanart_personal_api_key),
        ..ROW
    },
    SettingSpec {
        key: "tvdb_cache_hours",
        env: Some("FERROFIN_TVDB_CACHE_HOURS"),
        env_read: EnvRead::NonBlank,
        group: Group::Providers,
        kind: POSITIVE,
        default: "1",
        description: "Hours TheTVDB lookups are reused.",
        running: |c| whole_number_as_written(c.tvdb_cache_hours.as_deref()),
        ..ROW
    },
    SettingSpec {
        key: "tvdb_token_days",
        env: Some("FERROFIN_TVDB_TOKEN_DAYS"),
        env_read: EnvRead::NonBlank,
        group: Group::Providers,
        kind: SettingKind::Integer { min: 1, max: 29 },
        default: "25",
        description: "Days TheTVDB's sign-in token is used before signing in again.",
        running: |c| whole_number_as_written(c.tvdb_token_days.as_deref()),
        ..ROW
    },
    SettingSpec {
        key: "musicbrainz_base_url",
        env: Some("FERROFIN_MUSICBRAINZ_URL"),
        group: Group::Providers,
        kind: SettingKind::Url,
        default: "https://musicbrainz.org",
        description: "MusicBrainz server, e.g. a mirror, which lifts the one-request-a-second \
                      limit.",
        running: |c| json!(c.musicbrainz_base_url),
        ..ROW
    },
    SettingSpec {
        key: "studios_repo_url",
        env: Some("FERROFIN_STUDIOS_REPO_URL"),
        group: Group::Providers,
        kind: SettingKind::Url,
        default: "Jellyfin's studio artwork repository",
        description: "Studio Images artwork repository.",
        running: |c| json!(c.studios_repo_url),
        ..ROW
    },
    SettingSpec {
        key: "provider_timeout_seconds",
        env: Some("FERROFIN_PROVIDER_TIMEOUT_SECONDS"),
        env_read: EnvRead::Parsed(parses::<u64>),
        group: Group::Providers,
        kind: provider_seconds(ferrofin_providers::rate_limit::RequestLimits::TIMEOUT_SECONDS),
        default: "20",
        description: "Per-attempt timeout of a metadata-provider request, in seconds.",
        running: |c| json!(c.provider_timeout_seconds),
        ..ROW
    },
    SettingSpec {
        key: "provider_max_wait_seconds",
        env: Some("FERROFIN_PROVIDER_MAX_WAIT_SECONDS"),
        env_read: EnvRead::Parsed(parses::<u64>),
        group: Group::Providers,
        kind: provider_seconds(ferrofin_providers::rate_limit::RequestLimits::MAX_WAIT_SECONDS),
        default: "60",
        description: "Longest pacing or quota wait before a metadata lookup is skipped, in \
                      seconds.",
        running: |c| json!(c.provider_max_wait_seconds),
        ..ROW
    },
    // ---- WASM plugins -----------------------------------------------------
    SettingSpec {
        key: "wasm_call_timeout_secs",
        env: Some("FERROFIN_WASM_CALL_TIMEOUT_SECS"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "30",
        description: "Deadline of one plugin call, in seconds; 0 means the default.",
        running: |c| json!(c.wasm_call_timeout_secs),
        ..ROW
    },
    SettingSpec {
        key: "wasm_memory_limit_mb",
        env: Some("FERROFIN_WASM_MEMORY_LIMIT_MB"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "128",
        description: "Memory cap of one plugin, in MiB; 0 means the default.",
        running: |c| json!(c.wasm_memory_limit_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_event_queue_capacity",
        env: Some("FERROFIN_WASM_EVENT_QUEUE_CAPACITY"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "256",
        description: "Event queue depth of one plugin; 0 means the default.",
        running: |c| json!(c.wasm_event_queue_capacity),
        ..ROW
    },
    SettingSpec {
        key: "wasm_private_http_allow",
        env: Some("FERROFIN_WASM_PRIVATE_HTTP_ALLOW"),
        group: Group::Wasm,
        // Owner decision D5: granting it must take host access, not just an
        // admin session.
        access: SettingAccess::ReadOnly(
            "Lets the named WASM plugins reach your LAN, loopback and cloud-metadata addresses, \
             bypassing their declared egress. Set it by hand in config.toml \
             (wasm_private_http_allow) or FERROFIN_WASM_PRIVATE_HTTP_ALLOW, so an admin session \
             alone cannot grant it.",
        ),
        default: "denied for every plugin",
        description: "Plugins allowed to reach private and loopback addresses: plugin ids, \
                      comma-separated, or * for all.",
        running: |c| json!(c.wasm_private_http_allow),
        ..ROW
    },
    SettingSpec {
        key: "max_plugin_download_mb",
        env: Some("FERROFIN_MAX_PLUGIN_DOWNLOAD_MB"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "128",
        description: "Size cap of a plugin downloaded from a repository, in MiB; 0 means the \
                      default.",
        running: |c| json!(c.max_plugin_download_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_analysis_concurrency",
        env: Some("FERROFIN_WASM_ANALYSIS_CONCURRENCY"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "a quarter of the cores, at least one",
        description: "Media decodes plugin analysis may run at once.",
        running: |c| json!(c.wasm_analysis_concurrency),
        ..ROW
    },
    SettingSpec {
        key: "wasm_state_limit_mb",
        env: Some("FERROFIN_WASM_STATE_LIMIT_MB"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "8",
        description: "Key/value state cap of one plugin, in MiB.",
        running: |c| json!(c.wasm_state_limit_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_image_download_mb",
        env: Some("FERROFIN_WASM_IMAGE_DOWNLOAD_MB"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "20",
        description: "Size cap of one plugin artwork download, in MiB.",
        running: |c| json!(c.wasm_image_download_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_image_timeout_secs",
        env: Some("FERROFIN_WASM_IMAGE_TIMEOUT_SECS"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "30",
        description: "Time limit of one plugin artwork download, in seconds.",
        running: |c| json!(c.wasm_image_timeout_secs),
        ..ROW
    },
    SettingSpec {
        key: "wasm_write_content_mb",
        env: Some("FERROFIN_WASM_WRITE_CONTENT_MB"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "2",
        description: "Size cap of the lyrics or subtitles a plugin writes, in MiB.",
        running: |c| json!(c.wasm_write_content_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_subtitle_extract_mb",
        env: Some("FERROFIN_WASM_SUBTITLE_EXTRACT_MB"),
        env_read: PARSED_U32,
        group: Group::Wasm,
        kind: U32,
        default: "10",
        description: "Size cap of one subtitle track a plugin extracts, in MiB.",
        running: |c| json!(c.wasm_subtitle_extract_mb),
        ..ROW
    },
];

/// A provider request limit's kind: the seconds the loader clamps it to.
// The bounds are a few hundred seconds, and `i64::try_from` is not const.
#[allow(clippy::cast_possible_wrap)]
const fn provider_seconds(range: std::ops::RangeInclusive<u64>) -> SettingKind {
    SettingKind::Integer {
        min: *range.start() as i64,
        max: *range.end() as i64,
    }
}

/// A whole number kept as written (TheTVDB's durations, checked where they
/// are used): the number when it is one, else the text as written, which the
/// server replaces with the default and a warning.
fn whole_number_as_written(raw: Option<&str>) -> Value {
    match raw {
        None => Value::Null,
        Some(raw) => raw
            .trim()
            .parse::<u32>()
            .map_or_else(|_| json!(raw), |n| json!(n)),
    }
}

/// [`EnvRead::Parsed`] for a variable the loader reads as a `T`
/// (`config::parse_var`): `raw` applies when that takes it.
fn parses<T: std::str::FromStr>(raw: &str) -> bool {
    config::parse_var::<T>(&OneValue(raw), "").is_some()
}

/// [`EnvRead::Parsed`] for an on/off variable (`config::parse_bool_var`).
fn parses_as_bool(raw: &str) -> bool {
    config::parse_bool_var(&OneValue(raw), "").is_some()
}

/// [`EnvRead::Parsed`] for the trace sampling ratio, as
/// `bootstrap::sample_ratio` takes it: a finite number (one outside 0 to 1 is
/// clamped); anything else is the default.
fn parses_as_ratio(raw: &str) -> bool {
    raw.trim().parse::<f64>().is_ok_and(f64::is_finite)
}

/// An environment in which every variable holds one value.
struct OneValue<'a>(&'a str);

impl Env for OneValue<'_> {
    fn var(&self, _key: &str) -> Option<String> {
        Some(self.0.to_owned())
    }
}

/// An environment that sets nothing: `config.toml` alone.
struct NoVariables;

impl Env for NoVariables {
    fn var(&self, _key: &str) -> Option<String> {
        None
    }
}

/// Whether `cli` sets the setting `key` with its flag.
fn flag_set(cli: &Cli, key: &str) -> bool {
    match key {
        "data_dir" => cli.data_dir.is_some(),
        "bind_addr" => cli.bind_addr.is_some(),
        "port" => cli.port.is_some(),
        "published_url" => cli.published_url.is_some(),
        "ffmpeg_path" => cli.ffmpeg_path.is_some(),
        _ => false,
    }
}

/// `RUST_LOG`'s value when it holds a filter, which then outranks the
/// `log_level` setting.
fn rust_log(env: &dyn Env) -> Option<String> {
    env.var(RUST_LOG)
        .filter(|raw| tracing_subscriber::EnvFilter::try_new(raw).is_ok())
}

/// Whether `spec`'s variable sets the value in use: it is set, and the loader
/// takes what it holds ([`EnvRead`]).
fn env_applies(spec: &SettingSpec, env: &dyn Env) -> bool {
    let Some(raw) = spec.env.and_then(|var| env.var(var)) else {
        return false;
    };
    match spec.env_read {
        EnvRead::Raw => true,
        EnvRead::NonBlank => !raw.trim().is_empty(),
        EnvRead::Parsed(takes) => takes(&raw),
    }
}

/// Whether `config.toml` may set `spec`: every key may but `data_dir`, which
/// only a file `--config` or `FERROFIN_CONFIG_FILE` names may move; the loader
/// ignores it in one found in the data directory.
fn file_may_set(spec: &SettingSpec, cli: &Cli, env: &dyn Env) -> bool {
    spec.access != SettingAccess::EnvOnly
        && (spec.key != "data_dir"
            || cli.config_file.is_some()
            || env
                .var("FERROFIN_CONFIG_FILE")
                .is_some_and(|file| !file.trim().is_empty()))
}

/// Where `spec`'s value in use comes from: its flag, else its variable, else
/// `config.toml` (whose top-level keys are `file_keys`), else the default.
fn source(spec: &SettingSpec, cli: &Cli, env: &dyn Env, file_keys: &[String]) -> Source {
    if flag_set(cli, spec.key) {
        Source::Flag
    } else if env_applies(spec, env) || (spec.key == "log_level" && rust_log(env).is_some()) {
        Source::Environment
    } else if file_keys.iter().any(|key| key == spec.key) && file_may_set(spec, cli, env) {
        Source::ConfigFile
    } else {
        Source::Default
    }
}

/// Why the page may not change `spec`, set from `source`, or `None` when it
/// may: the row's own note, then how a flag or a variable that sets it is
/// changed.
fn locked_reason(spec: &SettingSpec, source: Source, env: &dyn Env) -> Option<String> {
    let note = match spec.access {
        SettingAccess::Editable => None,
        SettingAccess::ReadOnly(note) => Some(note),
        // The note already says where it is set.
        SettingAccess::EnvOnly => return Some(ENV_ONLY.to_owned()),
    };
    let set_by = match source {
        Source::Flag => spec.flag.map(|flag| {
            format!(
                "Set by the `{flag}` command-line flag. Change the command that starts the \
                 server and restart it."
            )
        }),
        Source::Environment => {
            let (var, outranks) = if spec.key == "log_level" && rust_log(env).is_some() {
                (RUST_LOG, ", which outranks `FERROFIN_LOG` and `log_level`")
            } else {
                (spec.env.unwrap_or_default(), "")
            };
            Some(format!(
                "Set by the `{var}` environment variable{outranks}. Change it where the server's \
                 environment is defined (compose file, Helm values, service unit) and restart \
                 the server process."
            ))
        }
        Source::ConfigFile | Source::Default => None,
    };
    match (note, set_by) {
        (Some(note), Some(set_by)) => Some(format!("{note} {set_by}")),
        (note, set_by) => note.map(str::to_owned).or(set_by),
    }
}

/// `value`, unless it is `null`: an unset setting reports no value.
fn present(value: Value) -> Option<Value> {
    (!value.is_null()).then_some(value)
}

/// What the page receives for `spec`. `running` is the configuration in use;
/// `saved` is `config.toml` as it is now, loaded alone, when it has keys.
fn setting(
    spec: &SettingSpec,
    running: &Config,
    env: &dyn Env,
    file_keys: &[String],
    saved: Option<&Config>,
) -> ServerSettingDto {
    let source = source(spec, &running.cli, env, file_keys);
    let value = match spec.access {
        SettingAccess::EnvOnly => spec
            .env
            .filter(|_| env_applies(spec, env))
            .and_then(|var| env.var(var))
            .map(Value::String),
        _ if spec.key == "log_level" => rust_log(env)
            .map(Value::String)
            .or_else(|| present((spec.running)(running))),
        _ => present((spec.running)(running)),
    };
    let saved_value = saved
        .filter(|_| {
            spec.access != SettingAccess::EnvOnly && file_keys.iter().any(|key| key == spec.key)
        })
        .and_then(|saved| present((spec.running)(saved)))
        .filter(|saved| Some(saved) != value.as_ref());
    let locked_reason = locked_reason(spec, source, env);
    let (kind, min, max) = spec.kind.to_dto();
    ServerSettingDto {
        key: spec.key.to_owned(),
        group: spec.group,
        kind,
        description: spec.description.to_owned(),
        env_var: spec.env.map(str::to_owned),
        flag: spec.flag.map(str::to_owned),
        default: (!spec.default.is_empty()).then(|| spec.default.to_owned()),
        value,
        saved_value,
        source,
        editable: locked_reason.is_none(),
        locked_reason,
        applies: spec.applies,
        min,
        max,
    }
}

/// The error of a `config.toml` that changed since the server started and no
/// longer loads. The loader's message never quotes the file.
fn no_longer_loads(path: &Path, err: &anyhow::Error) -> ServiceError {
    ServiceError::conflict(format!(
        "`{}` changed since the server started and no longer loads: {err:#}",
        path.display()
    ))
}

/// Every setting `running` holds, as the page receives them.
fn snapshot(
    running: &Config,
    env: &dyn Env,
    restart_pending: bool,
) -> Result<ServerSettingsDto, ServiceError> {
    let path = &running.config_file;
    let file_keys = config::config_file_keys(path).map_err(|err| no_longer_loads(path, &err))?;
    // The file alone, in the data directory in use, for the values it holds now.
    let saved = if file_keys.is_empty() {
        None
    } else {
        let cli = Cli {
            data_dir: Some(running.data_dir.clone()),
            config_file: Some(path.clone()),
            ..Cli::default()
        };
        Some(Config::load_from(cli, &NoVariables).map_err(|err| no_longer_loads(path, &err))?)
    };
    Ok(ServerSettingsDto {
        config_file: path.display().to_string(),
        config_file_writable: directory_accepts_a_file(path),
        restart_pending,
        settings: SETTINGS
            .iter()
            .map(|spec| setting(spec, running, env, &file_keys, saved.as_ref()))
            .collect(),
    })
}

/// Whether a new file can be created beside `path`, which a save needs (it
/// writes a temporary file there and renames it over `path`): an empty probe
/// file is created and removed again. A read-only mount or directory fails.
fn directory_accepts_a_file(path: &Path) -> bool {
    /// Tells apart the probes of reports made at once.
    static PROBES: AtomicU64 = AtomicU64::new(0);
    let Some(dir) = path.parent() else {
        return false;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let probe = dir.join(format!(
        ".ferrofin-settings-probe.{}.{}",
        std::process::id(),
        PROBES.fetch_add(1, Ordering::Relaxed)
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            if let Err(err) = std::fs::remove_file(&probe) {
                tracing::warn!(probe = %probe.display(), %err, "could not remove the settings write probe");
            }
            true
        }
        Err(_) => false,
    }
}

/// The settings manager behind `GET /Ferrofin/Settings`: the configuration
/// this server lifetime runs with, and where each value comes from.
pub struct FerrofinServerSettings {
    /// The configuration in use, with the flags it was loaded under.
    config: Config,
    /// The process environment the configuration was loaded from.
    env: Arc<dyn Env + Send + Sync>,
    /// For the restart-pending flag.
    lifecycle: Arc<dyn LifecycleController>,
    /// For the Ferrofin plugin's enabled flag.
    plugins: Arc<dyn PluginManager>,
}

impl FerrofinServerSettings {
    /// Reports `config`, which the process environment and the flags it
    /// records were loaded into, with `lifecycle`'s restart flag; answers
    /// only while the Ferrofin plugin is enabled in `plugins`.
    #[must_use]
    pub fn new(
        config: Config,
        lifecycle: Arc<dyn LifecycleController>,
        plugins: Arc<dyn PluginManager>,
    ) -> Self {
        Self {
            config,
            env: Arc::new(config::EnvProvider),
            lifecycle,
            plugins,
        }
    }

    /// The same, with `env` in place of the process environment.
    #[cfg(test)]
    fn with_env(mut self, env: impl Env + Send + Sync + 'static) -> Self {
        self.env = Arc::new(env);
        self
    }

    /// [`ServiceError::NotFound`] while the Ferrofin plugin is disabled: a
    /// disabled plugin's routes answer `404`.
    async fn ensure_enabled(&self) -> Result<(), ServiceError> {
        match self
            .plugins
            .get_plugin(ferrofin_extensions::ferrofin_settings::EXTENSION_ID)
            .await
        {
            Ok(Some(plugin)) if plugin.enabled => Ok(()),
            _ => Err(ServiceError::not_found("the Ferrofin plugin is disabled")),
        }
    }
}

#[async_trait::async_trait]
impl ServerSettingsManager for FerrofinServerSettings {
    async fn snapshot(&self) -> Result<ServerSettingsDto, ServiceError> {
        self.ensure_enabled().await?;
        let restart_pending = self.lifecycle.has_pending_restart();
        let running = self.config.clone();
        let env = Arc::clone(&self.env);
        // Reads config.toml and probes its directory.
        tokio::task::spawn_blocking(move || snapshot(&running, env.as_ref(), restart_pending))
            .await
            .map_err(|err| ServiceError::backend(format!("settings report failed: {err}")))?
    }
}

#[cfg(test)]
mod tests;
