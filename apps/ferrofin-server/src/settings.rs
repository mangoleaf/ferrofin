//! The Ferrofin settings page's server side: [`SETTINGS`], one row per
//! bootstrap setting (CLI flag, `FERROFIN_*` environment variable,
//! `config.toml` key) and per setting stored in the server configuration
//! (the trickplay rows), and [`FerrofinServerSettings`], which reports each
//! one's running value and where it comes from over `GET /Ferrofin/Settings`
//! and saves the page's changes to `config.toml`, or to the server
//! configuration, over `POST /Ferrofin/Settings` (the `save` module).
//!
//! The table is the one description of the settings that the report, the
//! page and saving read. Its tests tie it to what the loader reads
//! ([`Config::load`]): every `config.toml` key has a row and every such row's
//! key is one, every row's variable (a stored row's label) has a row in
//! `docs/CONFIG.md`, every key round-trips through `config.toml`, the source
//! a row reports is the layer the loader applied, every flag is a row's, and
//! the rows that apply only on a process restart are what an in-process
//! restart keeps ([`Config::for_restart`]).
//!
//! Precedence is the loader's: a flag, else the variable, else `config.toml`,
//! else the default. A value a flag or a variable sets is locked on the page
//! (owner decision D1 of the settings-page milestone): one saved to
//! `config.toml` underneath would not apply while it is set. A setting
//! jellyfin-web's Networking page covers ([`SettingSpec::network`]) takes the
//! page's saved value between the variable and `config.toml` (owner decisions
//! on gl-554 and gl-555).

use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ferrofin_core::system_manager::LifecycleController;
use ferrofin_core::user_manager::LoginBackoffSettings;
use ferrofin_drawing::ImageLimits;
use ferrofin_mediaencoding::encoder::media_encoder::{
    ANALYZE_DURATION_RANGE, FfmpegCount, PROBE_SIZE_RANGE,
};
use ferrofin_mediaencoding::transcoding::TranscodeLimits;
use ferrofin_model::configuration::ServerConfiguration;
use ferrofin_model::server_settings::{
    ServerSettingApplies as Applies, ServerSettingDto, ServerSettingGroup as Group,
    ServerSettingKind, ServerSettingOptionDto, ServerSettingSource as Source, ServerSettingStore,
    ServerSettingUnit as Unit, ServerSettingsDto, ServerSettingsUpdateDto,
    ServerSettingsUpdateResultDto,
};
use ferrofin_networking::NetworkConfiguration;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::PluginManager;
use ferrofin_traits::server_settings::ServerSettingsManager;
use serde_json::{Value, json};
use tracing::Instrument as _;

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
    /// One of the row's [`options`](SettingSpec::options).
    Choice,
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
            Self::Choice => (ServerSettingKind::Choice, None, None),
        }
    }
}

/// One option of a [`SettingKind::Choice`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingOption {
    /// The value it stores, and the one the report gives when it is picked.
    pub value: &'static str,
    /// Its readable name.
    pub label: &'static str,
    /// What picking it does; empty for none.
    pub note: &'static str,
}

/// Where a row's value is stored.
#[derive(Debug, Clone, Copy)]
pub enum SettingStore {
    /// `config.toml`, read at startup (with its flag and its variable, which
    /// outrank it).
    ConfigFile,
    /// The server configuration the Dashboard edits, which applies at once.
    /// Nothing outranks it: no variable or flag sets such a row.
    ServerConfiguration(ConfigurationField),
}

/// How a row stored in the server configuration reads and writes it.
#[derive(Debug, Clone, Copy)]
pub struct ConfigurationField {
    /// The value the report gives, from the configuration. For a
    /// [`SettingKind::Choice`], the text of the option it fits, or of the
    /// stored value when it fits none.
    pub read: fn(&ServerConfiguration) -> Value,
    /// Puts `value`, one the row's kind takes, into the configuration.
    pub write: fn(&mut ServerConfiguration, &Value),
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
    /// Its readable name, which heads its row on the page.
    pub label: &'static str,
    /// The tab the page lists it on.
    pub group: Group,
    /// The kind of value it holds.
    pub kind: SettingKind,
    /// The unit its value is in, which the page shows beside it.
    pub unit: Option<Unit>,
    /// Whether the page may change it.
    pub access: SettingAccess,
    /// What a change needs before it applies.
    pub applies: Applies,
    /// The value that applies when nothing sets it, as the page shows it.
    pub default: &'static str,
    /// What it does, in one line.
    pub description: &'static str,
    /// What it does at more length, shown under the description; empty for
    /// none.
    pub note: &'static str,
    /// The options of a [`SettingKind::Choice`] row, in the order the page
    /// offers them; empty for any other kind.
    pub options: &'static [SettingOption],
    /// Where its value is stored.
    pub store: SettingStore,
    /// The value in use, secrets included (owner decision D3: admins see
    /// them, as on Jellyfin's plugin pages). `null` when unset and the
    /// default applies.
    pub running: fn(&Config) -> Value,
    /// The value the Networking page saved in the network configuration a
    /// server lifetime started with, which outranks `config.toml` but not a
    /// flag or a variable; `null` when it sets none, or covers no such
    /// setting. Only for a read-only row: a `config.toml` save would not apply
    /// under it, and nothing locks the row for it.
    pub network: fn(&NetworkConfiguration) -> Value,
    /// A rule a value the page saves must meet beyond its kind: one the
    /// server applies by warning and falling back to the default, which a save
    /// refuses instead, so a value the server would ignore is never saved.
    /// Given a value of the row's kind.
    pub check: fn(&Value) -> Result<(), String>,
}

/// The note of the directories the server is started on.
const SET_AT_STARTUP: &str =
    "Set at startup. Changing it from the web could point the server at other files.";

/// The note of the executables: Jellyfin refuses the same change over its API
/// (`EncodingConfigurationStore.Validate`, "Unable to update encoder app path.").
const NO_EXECUTABLE_FROM_THE_WEB: &str = "The web may not choose an executable the server runs.";

/// The note of the settings read only when the database is created.
const FIRST_BOOT_ONLY: &str = "Only used when the database is created.";

/// The note of the trickplay rows stored in the server configuration.
const TRICKPLAY_CHANGE: &str = "A change takes full effect after the next Generate Trickplay \
                                Images run. That run decodes every video again in the libraries \
                                with trickplay images on, at below-normal priority: a large \
                                library can take days.";

/// Preview Size as `WidthResolutions` holds it: `320`, `640` or `both`, in
/// any order and counting a width once, else the widths themselves (Custom),
/// or `none`.
fn trickplay_preview_size(configuration: &ServerConfiguration) -> Value {
    let mut widths = configuration.trickplay_options.width_resolutions.clone();
    widths.sort_unstable();
    widths.dedup();
    json!(match widths.as_slice() {
        [320] => "320".to_owned(),
        [640] => "640".to_owned(),
        [320, 640] => "both".to_owned(),
        [] => "none".to_owned(),
        _ => widths
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// Image Quality as `Qscale` and the chroma member hold it: `disk` (4:2:0,
/// `Qscale` 4) or `quality` (4:4:4, `Qscale` 2), else both values (Custom).
fn trickplay_image_quality(configuration: &ServerConfiguration) -> Value {
    let options = &configuration.trickplay_options;
    json!(match (options.full_chroma, options.qscale) {
        (false, 4) => "disk".to_owned(),
        (true, 2) => "quality".to_owned(),
        (full_chroma, qscale) => format!(
            "Qscale {qscale}, {}",
            if full_chroma { "4:4:4" } else { "4:2:0" }
        ),
    })
}

/// The note of the OpenTelemetry variables.
const ENV_ONLY: &str = "OpenTelemetry's standard variable, read from the environment only: \
                        config.toml has no key for it. Set it where the server's environment is \
                        defined (compose file, Helm values, service unit) and restart the server \
                        process.";

/// `RUST_LOG`, which outranks `FERROFIN_LOG` and `log_level` when it holds a
/// filter (`bootstrap::init_tracing`).
const RUST_LOG: &str = "RUST_LOG";

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

/// The statement cache's bounds ([`config::STATEMENT_CACHE_RANGE`]).
const STATEMENT_CACHE: SettingKind = SettingKind::Integer {
    min: *config::STATEMENT_CACHE_RANGE.start() as i64,
    max: *config::STATEMENT_CACHE_RANGE.end() as i64,
};

/// The transcode caps' bounds ([`TranscodeLimits::RANGE`]).
const TRANSCODE_CAP: SettingKind = SettingKind::Integer {
    min: *TranscodeLimits::RANGE.start() as i64,
    max: *TranscodeLimits::RANGE.end() as i64,
};

/// The login backoff's attempts ([`LoginBackoffSettings::ATTEMPTS_RANGE`]).
const LOGIN_BACKOFF_ATTEMPTS: SettingKind = SettingKind::Integer {
    min: *LoginBackoffSettings::ATTEMPTS_RANGE.start() as i64,
    max: *LoginBackoffSettings::ATTEMPTS_RANGE.end() as i64,
};

/// The login backoff's maximum delay ([`LoginBackoffSettings::MAX_DELAY_RANGE_SECS`]).
const LOGIN_BACKOFF_MAX_DELAY: SettingKind = SettingKind::Integer {
    min: *LoginBackoffSettings::MAX_DELAY_RANGE_SECS.start() as i64,
    max: *LoginBackoffSettings::MAX_DELAY_RANGE_SECS.end() as i64,
};

/// The login backoff's key cap ([`LoginBackoffSettings::MAX_TRACKED_RANGE`]).
const LOGIN_BACKOFF_MAX_TRACKED: SettingKind = SettingKind::Integer {
    min: *LoginBackoffSettings::MAX_TRACKED_RANGE.start() as i64,
    max: *LoginBackoffSettings::MAX_TRACKED_RANGE.end() as i64,
};

/// The login backoff's window ([`LoginBackoffSettings::WINDOW_RANGE_MINS`]).
const LOGIN_BACKOFF_WINDOW: SettingKind = SettingKind::Integer {
    min: *LoginBackoffSettings::WINDOW_RANGE_MINS.start() as i64,
    max: *LoginBackoffSettings::WINDOW_RANGE_MINS.end() as i64,
};

/// [`EnvRead::Parsed`] for a variable the loader reads as a `u32`.
const PARSED_U32: EnvRead = EnvRead::Parsed(parses::<u32>);

/// [`EnvRead::Parsed`] for an on/off variable.
const PARSED_BOOL: EnvRead = EnvRead::Parsed(parses_as_bool);

/// What every row starts from: an editable, unflagged setting whose variable
/// applies whatever it holds. Each row names its key, variable, label, tab,
/// kind, default, description and running value, and its unit when it has
/// one.
const ROW: SettingSpec = SettingSpec {
    key: "",
    env: None,
    env_read: EnvRead::Raw,
    flag: None,
    label: "",
    group: Group::Library,
    kind: SettingKind::Text,
    unit: None,
    access: SettingAccess::Editable,
    applies: Applies::Restart,
    default: "",
    description: "",
    note: "",
    options: &[],
    store: SettingStore::ConfigFile,
    running: |_| Value::Null,
    network: |_| Value::Null,
    check: |_| Ok(()),
};

/// Every bootstrap setting, in the order the page lists them.
// One row per setting: the length is the table's.
pub static SETTINGS: &[SettingSpec] = &[
    // ---- Network -----------------------------------------------------------
    SettingSpec {
        key: "bind_addr",
        env: Some("FERROFIN_BIND_ADDR"),
        flag: Some("--bind"),
        label: "Listen address",
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
    // The port, HTTPS port and base URL notes name the Networking page that
    // drives them (owner decisions on gl-554 and gl-555).
    SettingSpec {
        key: "port",
        env: Some("FERROFIN_PORT"),
        env_read: EnvRead::Parsed(parses::<u16>),
        flag: Some("--port"),
        label: "HTTP port",
        group: Group::Network,
        kind: PORT,
        access: SettingAccess::ReadOnly(
            "Dashboard → Networking → Local HTTP port number sets it, and a change there applies \
             after a restart. While that holds the default, 8096, the port key in config.toml \
             does. The FERROFIN_PORT environment variable and the --port flag outrank both.",
        ),
        default: "8096",
        description: "HTTP port.",
        running: |c| json!(c.port),
        network: |n| {
            crate::listen::page_ports(n)
                .0
                .map_or(Value::Null, |port| json!(port))
        },
        ..ROW
    },
    SettingSpec {
        key: "https_port",
        env: Some("FERROFIN_HTTPS_PORT"),
        env_read: EnvRead::Parsed(parses::<u16>),
        label: "HTTPS port",
        group: Group::Network,
        kind: PORT,
        access: SettingAccess::ReadOnly(
            "Dashboard → Networking → Local HTTPS port number sets it, and a change there \
             applies after a restart. While that holds the default, 8920, the https_port key in \
             config.toml does. The FERROFIN_HTTPS_PORT environment variable outranks both.",
        ),
        default: "8920",
        description: "HTTPS port, served when Dashboard → Networking enables HTTPS.",
        running: |c| json!(c.https_port),
        network: |n| {
            crate::listen::page_ports(n)
                .1
                .map_or(Value::Null, |port| json!(port))
        },
        ..ROW
    },
    SettingSpec {
        key: "published_url",
        env: Some("FERROFIN_PUBLISHED_URL"),
        flag: Some("--published-url"),
        label: "Published server URL",
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
        label: "Base URL",
        group: Group::Network,
        access: SettingAccess::ReadOnly(
            "Dashboard → Networking → Base URL sets it, and a change there applies after a \
             restart. While that is empty, the base_url key in config.toml does. The \
             FERROFIN_BASE_URL environment variable outranks both.",
        ),
        default: "none",
        description: "URL path prefix the server is mounted under.",
        running: |c| json!(c.base_url),
        network: |n| {
            let base = crate::base_url::normalize(&n.base_url);
            if base.is_empty() {
                Value::Null
            } else {
                json!(base)
            }
        },
        ..ROW
    },
    SettingSpec {
        key: "server_name",
        env: Some("FERROFIN_SERVER_NAME"),
        label: "Server name",
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
        key: "quick_connect_pending_per_ip",
        env: Some("FERROFIN_QUICK_CONNECT_PENDING_PER_IP"),
        env_read: PARSED_U32,
        label: "Pending Quick Connect requests per address",
        group: Group::Network,
        kind: whole_numbers(config::QUICK_CONNECT_PENDING_PER_IP_RANGE),
        default: "10",
        description: "Unauthorized Quick Connect requests one client IP may hold at once.",
        running: |c| json!(c.quick_connect_pending_per_ip),
        ..ROW
    },
    SettingSpec {
        key: "detect_network_change",
        env: Some("FERROFIN_DETECT_NETWORK_CHANGE"),
        env_read: PARSED_BOOL,
        label: "Follow network changes",
        group: Group::Network,
        kind: SettingKind::Bool,
        default: "true",
        description: "Read the network interfaces again 2 seconds after the operating system \
                      reports a network change, so local URLs and bind addresses follow it.",
        running: |c| json!(c.detect_network_change),
        ..ROW
    },
    SettingSpec {
        key: "login_backoff",
        env: Some("FERROFIN_LOGIN_BACKOFF"),
        env_read: PARSED_BOOL,
        label: "Login backoff",
        group: Group::Network,
        kind: SettingKind::Bool,
        default: "true",
        description: "Make an account's and a client address's next logins wait after \
                      repeated failures. The backoff never disables an account.",
        running: |c| json!(c.login_backoff),
        ..ROW
    },
    SettingSpec {
        key: "login_backoff_attempts",
        env: Some("FERROFIN_LOGIN_BACKOFF_ATTEMPTS"),
        env_read: PARSED_U32,
        label: "Failed logins before backoff",
        group: Group::Network,
        kind: LOGIN_BACKOFF_ATTEMPTS,
        default: "5",
        description: "Failed logins an account or a client address may make before its next \
                      attempts are made to wait.",
        running: |c| json!(c.login_backoff_attempts),
        ..ROW
    },
    SettingSpec {
        key: "login_backoff_max_delay_secs",
        env: Some("FERROFIN_LOGIN_BACKOFF_MAX_DELAY_SECS"),
        env_read: PARSED_U32,
        label: "Longest login wait",
        group: Group::Network,
        kind: LOGIN_BACKOFF_MAX_DELAY,
        unit: Some(Unit::Seconds),
        default: "30",
        description: "Longest wait the login backoff gives an attempt. An address that would wait \
                      longer is refused as a wrong password; an account only waits.",
        running: |c| json!(c.login_backoff_max_delay_secs),
        ..ROW
    },
    SettingSpec {
        key: "login_backoff_window_mins",
        env: Some("FERROFIN_LOGIN_BACKOFF_WINDOW_MINS"),
        env_read: PARSED_U32,
        label: "Failed login memory",
        group: Group::Network,
        kind: LOGIN_BACKOFF_WINDOW,
        unit: Some(Unit::Minutes),
        default: "15",
        description: "How long the login backoff remembers an account's or an address's failed \
                      logins after the last one.",
        running: |c| json!(c.login_backoff_window_mins),
        ..ROW
    },
    SettingSpec {
        key: "login_backoff_max_tracked",
        env: Some("FERROFIN_LOGIN_BACKOFF_MAX_TRACKED"),
        env_read: PARSED_U32,
        label: "Login backoff tracking limit",
        group: Group::Network,
        kind: LOGIN_BACKOFF_MAX_TRACKED,
        unit: Some(Unit::Items),
        default: "100000",
        description: "Most accounts and addresses the login backoff remembers at once; past \
                      it, the ones not yet made to wait are forgotten first.",
        running: |c| json!(c.login_backoff_max_tracked),
        ..ROW
    },
    // ---- Library -----------------------------------------------------------
    SettingSpec {
        key: "scan_probe_concurrency",
        env: Some("FERROFIN_SCAN_PROBE_CONCURRENCY"),
        env_read: PARSED_U32,
        label: "Concurrent scan probes",
        group: Group::Library,
        kind: whole_numbers(config::SCAN_PROBE_CONCURRENCY_RANGE),
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
        label: "Scan progress interval",
        group: Group::Library,
        kind: whole_numbers(config::SCAN_PROGRESS_EVERY_RANGE),
        unit: Some(Unit::Items),
        default: "100",
        description: "Items between scan-progress log lines; 0 turns them off.",
        running: |c| json!(c.scan_progress_every),
        ..ROW
    },
    SettingSpec {
        key: "scan_directory_cache_records",
        env: Some("FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS"),
        env_read: PARSED_U32,
        label: "Scan directory cache size",
        group: Group::Library,
        kind: whole_numbers(config::SCAN_DIRECTORY_CACHE_RECORDS_RANGE),
        unit: Some(Unit::Items),
        default: "100000",
        description: "Directory-listing records one library scan keeps before it lists folders \
                      again.",
        running: |c| json!(c.scan_directory_cache_records),
        ..ROW
    },
    SettingSpec {
        key: "scan_directory_cache_idle_secs",
        env: Some("FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS"),
        env_read: PARSED_U32,
        label: "Scan directory cache lifetime",
        group: Group::Library,
        kind: whole_numbers(config::SCAN_DIRECTORY_CACHE_IDLE_SECS_RANGE),
        unit: Some(Unit::Seconds),
        default: "300",
        description: "How long a library scan's directory listings survive unread.",
        running: |c| json!(c.scan_directory_cache_idle_secs),
        ..ROW
    },
    SettingSpec {
        key: "book_archive_max_entries",
        env: Some("FERROFIN_BOOK_ARCHIVE_MAX_ENTRIES"),
        env_read: PARSED_U32,
        label: "Comic archive entry limit",
        group: Group::Library,
        kind: whole_numbers(config::BOOK_ARCHIVE_MAX_ENTRIES_RANGE),
        unit: Some(Unit::Items),
        default: "100000",
        description: "Most entries a comic archive may hold for its pages to set the book's \
                      runtime; a larger one is not counted.",
        running: |c| json!(c.book_archive_max_entries),
        ..ROW
    },
    // ---- Trickplay ---------------------------------------------------------
    // The two rows stored in the server configuration (gl-1139): the same
    // `TrickplayOptions` the dashboard's Trickplay page edits.
    SettingSpec {
        key: "trickplay_preview_size",
        label: "Preview Size",
        group: Group::Trickplay,
        kind: SettingKind::Choice,
        applies: Applies::Immediately,
        default: "320",
        description: "Width of the scrubbing previews, in pixels.",
        note: TRICKPLAY_CHANGE,
        options: &[
            SettingOption {
                value: "320",
                label: "320",
                note: "Fast to fetch, small on 4K screens. One ffmpeg pass per video.",
            },
            SettingOption {
                value: "640",
                label: "640",
                note: "Sharper on 4K. Phones and 1080p screens get a large preview too. \
                       One ffmpeg pass per video. Its tiles hold fewer previews than a 320 \
                       tile, so each is about the size of a 320 tile.",
            },
            SettingOption {
                value: "both",
                label: "Both",
                note: "Clients pick by screen. Two ffmpeg passes per video and the disk of \
                       both sizes.",
            },
        ],
        store: SettingStore::ServerConfiguration(ConfigurationField {
            read: trickplay_preview_size,
            write: |configuration, value| {
                configuration.trickplay_options.width_resolutions = match value.as_str() {
                    Some("640") => vec![640],
                    Some("both") => vec![320, 640],
                    _ => vec![320],
                };
            },
        }),
        ..ROW
    },
    SettingSpec {
        key: "trickplay_image_quality",
        label: "Image Quality",
        group: Group::Trickplay,
        kind: SettingKind::Choice,
        applies: Applies::Immediately,
        default: "disk",
        description: "How much colour detail the previews keep, and the disk it costs.",
        note: TRICKPLAY_CHANGE,
        options: &[
            SettingOption {
                value: "disk",
                label: "Optimize Disk Usage",
                // Figures: docs/CONFIG.md#trickplay says what was measured.
                note: "Matches Jellyfin. Blocky colour around edges. \
                       About 3 MiB per hour of video at 320, 7 at 640.",
            },
            SettingOption {
                value: "quality",
                label: "Optimize Quality",
                note: "Full colour detail, except where trickplay's hardware acceleration \
                       decodes or encodes the video on the GPU: that stays 4:2:0. About 4 MiB \
                       per hour of video at 320, 12 at 640. As quick to generate.",
            },
        ],
        store: SettingStore::ServerConfiguration(ConfigurationField {
            read: trickplay_image_quality,
            write: |configuration, value| {
                let options = &mut configuration.trickplay_options;
                (options.full_chroma, options.qscale) = match value.as_str() {
                    Some("quality") => (true, 2),
                    _ => (false, 4),
                };
            },
        }),
        ..ROW
    },
    SettingSpec {
        key: "trickplay_concurrency",
        env: Some("FERROFIN_TRICKPLAY_CONCURRENCY"),
        env_read: EnvRead::NonBlank,
        label: "Concurrent trickplay videos",
        group: Group::Trickplay,
        kind: SettingKind::CountOrAuto,
        default: "auto",
        description: "Videos generating trickplay images at once, or auto for the image-encoding limit.",
        running: |c| json!(c.trickplay_concurrency),
        ..ROW
    },
    // ---- Images ------------------------------------------------------------
    SettingSpec {
        key: "image_decode_max_megapixels",
        env: Some("FERROFIN_IMAGE_DECODE_MAX_MEGAPIXELS"),
        env_read: PARSED_U32,
        label: "Largest decoded image",
        group: Group::Images,
        kind: whole_numbers(ImageLimits::DECODE_MEGAPIXELS),
        unit: Some(Unit::Megapixels),
        default: "200",
        description: "Largest image decoded; a larger one is served as its original file.",
        running: |c| json!(c.image_decode_max_megapixels),
        ..ROW
    },
    SettingSpec {
        key: "image_decode_max_memory_mb",
        env: Some("FERROFIN_IMAGE_DECODE_MAX_MEMORY_MB"),
        env_read: PARSED_U32,
        label: "Image decode memory limit",
        group: Group::Images,
        kind: whole_numbers(ImageLimits::DECODE_MEMORY_MB),
        unit: Some(Unit::Mebibytes),
        default: "512",
        description: "Memory one image decode may take; an image needing more is served as its \
                      original file.",
        running: |c| json!(c.image_decode_max_memory_mb),
        ..ROW
    },
    SettingSpec {
        key: "image_resize_budget_mb",
        env: Some("FERROFIN_IMAGE_RESIZE_BUDGET_MB"),
        env_read: PARSED_U32,
        label: "Image resize memory budget",
        group: Group::Images,
        kind: whole_numbers(ImageLimits::RESIZE_BUDGET_MB),
        unit: Some(Unit::Mebibytes),
        default: "2048",
        description: "Memory all image resizes and compositing (background colour, blur, \
                      foreground layer) in flight share; work needing more alone serves the \
                      original file.",
        running: |c| json!(c.image_resize_budget_mb),
        ..ROW
    },
    SettingSpec {
        key: "ffmpeg_img_extract_perf_tradeoff",
        env: Some("FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF"),
        env_read: PARSED_BOOL,
        label: "Fast image extraction",
        group: Group::Images,
        kind: SettingKind::Bool,
        default: "false",
        description: "Extract chapter images and thumbnails from keyframes only: faster, and \
                      the image may be less typical of the scene.",
        running: |c| json!(c.ffmpeg_img_extract_perf_tradeoff),
        ..ROW
    },
    SettingSpec {
        key: "image_download_max_mb",
        env: Some("FERROFIN_IMAGE_DOWNLOAD_MAX_MB"),
        env_read: PARSED_U32,
        label: "Image download limit",
        group: Group::Images,
        kind: whole_numbers(ferrofin_providers::rate_limit::RequestLimits::DOWNLOAD_MAX_MB),
        unit: Some(Unit::Mebibytes),
        default: "64",
        description: "Largest remote image downloaded, and largest metadata provider response; \
                      a larger one is refused and the read stops there.",
        running: |c| json!(c.image_download_max_mb),
        ..ROW
    },
    SettingSpec {
        key: "image_negative_cache_ttl_secs",
        env: Some("FERROFIN_IMAGE_NEGATIVE_CACHE_TTL_SECS"),
        env_read: EnvRead::Parsed(parses::<u64>),
        label: "Failed image retry delay",
        group: Group::Images,
        kind: provider_seconds(ferrofin_providers::IMAGE_NEGATIVE_CACHE_TTL_SECS),
        unit: Some(Unit::Seconds),
        default: "3600",
        description: "How long a remote image URL that gave no image is left alone before it is \
                      tried again.",
        running: |c| json!(c.image_negative_cache_ttl_secs),
        ..ROW
    },
    SettingSpec {
        key: "image_private_http_allow",
        env: Some("FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW"),
        env_read: EnvRead::NonBlank,
        label: "Private hosts for image downloads",
        group: Group::Images,
        // As wasm_private_http_allow (owner decision D5): letting the server
        // reach private addresses takes host access, not just an admin
        // session.
        access: SettingAccess::ReadOnly(
            "Lets image downloads reach the LAN, loopback and cloud-metadata addresses it lists. \
             Set it by hand in config.toml (image_private_http_allow) or \
             FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW, so an admin session alone cannot grant it.",
        ),
        default: "no private address but a Live TV tuner's host",
        description: "Private hosts, addresses and CIDR networks image downloads may reach, \
                      comma-separated; Live TV tuners' hosts are always allowed.",
        running: |c| json!(c.image_private_http_allow),
        ..ROW
    },
    // ---- Providers ---------------------------------------------------------
    SettingSpec {
        key: "omdb_api_key",
        env: Some("FERROFIN_OMDB_KEY"),
        label: "OMDb API key",
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
        label: "TheTVDB API key",
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
        label: "TheTVDB subscriber PIN",
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
        label: "fanart.tv personal API key",
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
        label: "TheTVDB cache lifetime",
        group: Group::Providers,
        kind: whole_numbers(config::TVDB_CACHE_HOURS_RANGE),
        unit: Some(Unit::Hours),
        default: "1",
        description: "How long TheTVDB lookups are reused.",
        // The hours in use: a whole number outside the range is clamped.
        running: |c| {
            let raw = c.tvdb_cache_hours.as_deref();
            match raw.map(|raw| raw.trim().parse::<u32>()) {
                Some(Ok(hours)) => json!(config::clamp_tvdb_cache_hours(hours)),
                _ => whole_number_as_written(raw),
            }
        },
        ..ROW
    },
    SettingSpec {
        key: "tvdb_token_days",
        env: Some("FERROFIN_TVDB_TOKEN_DAYS"),
        env_read: EnvRead::NonBlank,
        label: "TheTVDB token lifetime",
        group: Group::Providers,
        kind: SettingKind::Integer { min: 1, max: 29 },
        unit: Some(Unit::Days),
        default: "25",
        description: "How long TheTVDB's sign-in token is used before signing in again.",
        running: |c| whole_number_as_written(c.tvdb_token_days.as_deref()),
        ..ROW
    },
    SettingSpec {
        key: "musicbrainz_base_url",
        env: Some("FERROFIN_MUSICBRAINZ_URL"),
        label: "MusicBrainz server",
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
        label: "Studio Images repository",
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
        label: "Provider request timeout",
        group: Group::Providers,
        kind: provider_seconds(ferrofin_providers::rate_limit::RequestLimits::TIMEOUT_SECONDS),
        unit: Some(Unit::Seconds),
        default: "20",
        description: "Per-attempt timeout of a metadata-provider request.",
        running: |c| json!(c.provider_timeout_seconds),
        ..ROW
    },
    SettingSpec {
        key: "provider_max_wait_seconds",
        env: Some("FERROFIN_PROVIDER_MAX_WAIT_SECONDS"),
        env_read: EnvRead::Parsed(parses::<u64>),
        label: "Longest provider wait",
        group: Group::Providers,
        kind: provider_seconds(ferrofin_providers::rate_limit::RequestLimits::MAX_WAIT_SECONDS),
        unit: Some(Unit::Seconds),
        default: "60",
        description: "Longest pacing or quota wait before a metadata lookup is skipped.",
        running: |c| json!(c.provider_max_wait_seconds),
        ..ROW
    },
    SettingSpec {
        key: "subtitle_private_http_allow",
        env: Some("FERROFIN_SUBTITLE_PRIVATE_HTTP_ALLOW"),
        env_read: EnvRead::NonBlank,
        label: "Private hosts for subtitle downloads",
        group: Group::Providers,
        // As image_private_http_allow (owner decision D5): letting the server
        // reach private addresses takes host access, not just an admin
        // session.
        access: SettingAccess::ReadOnly(
            "Lets OpenSubtitles download links reach the LAN, loopback and cloud-metadata \
             addresses it lists. Set it by hand in config.toml (subtitle_private_http_allow) or \
             FERROFIN_SUBTITLE_PRIVATE_HTTP_ALLOW, so an admin session alone cannot grant it.",
        ),
        default: "no private address",
        description: "Private hosts, addresses and CIDR networks an OpenSubtitles download link \
                      may reach, comma-separated; the image list does not apply to it.",
        running: |c| json!(c.subtitle_private_http_allow),
        ..ROW
    },
    // ---- Performance -------------------------------------------------------
    SettingSpec {
        key: "db_pool",
        env: Some("FERROFIN_DB_POOL"),
        env_read: EnvRead::NonBlank,
        label: "Database connections",
        group: Group::Performance,
        kind: SettingKind::CountOrAuto,
        default: "auto",
        description: "SQLite connection count, or auto to size it to the cores.",
        running: |c| json!(c.db_pool),
        ..ROW
    },
    SettingSpec {
        key: "db_statement_cache",
        env: Some("FERROFIN_DB_STATEMENT_CACHE"),
        env_read: EnvRead::NonBlank,
        label: "Statement cache size",
        group: Group::Performance,
        kind: STATEMENT_CACHE,
        unit: Some(Unit::Items),
        default: "150",
        description: "Prepared statements each SQLite connection keeps.",
        running: |c| json!(c.db_statement_cache),
        ..ROW
    },
    SettingSpec {
        key: "item_values_write_chunk",
        env: Some("FERROFIN_ITEM_VALUES_WRITE_CHUNK"),
        env_read: PARSED_U32,
        label: "Item link write batch",
        group: Group::Performance,
        kind: whole_numbers(config::ITEM_VALUES_WRITE_CHUNK_RANGE),
        unit: Some(Unit::Items),
        default: "500",
        description: "Items whose genre, studio, tag and artist links one database transaction \
                      rewrites when an edit saves many at once; smaller holds the database \
                      writer for shorter stretches.",
        running: |c| json!(c.item_values_write_chunk),
        ..ROW
    },
    SettingSpec {
        key: "auth_cache_ttl_secs",
        env: Some("FERROFIN_AUTH_CACHE_TTL_SECS"),
        env_read: EnvRead::NonBlank,
        label: "Access token cache lifetime",
        group: Group::Performance,
        kind: CACHE_TTL,
        unit: Some(Unit::Seconds),
        default: "30",
        description: "How long a resolved access token is served from memory; only bounds changes \
                      made outside the server.",
        running: |c| json!(c.auth_cache_ttl_secs),
        ..ROW
    },
    SettingSpec {
        key: "row_cache_ttl_secs",
        env: Some("FERROFIN_ROW_CACHE_TTL_SECS"),
        env_read: EnvRead::NonBlank,
        label: "Access check cache lifetime",
        group: Group::Performance,
        kind: CACHE_TTL,
        unit: Some(Unit::Seconds),
        default: "30",
        description: "How long the access check serves users' and libraries' rows, and whether a \
                      Live TV tuner is configured, from memory; only bounds changes made outside \
                      the server.",
        running: |c| json!(c.row_cache_ttl_secs),
        ..ROW
    },
    SettingSpec {
        key: "max_transcodes",
        env: Some("FERROFIN_MAX_TRANSCODES"),
        env_read: PARSED_U32,
        label: "Transcode limit",
        group: Group::Performance,
        kind: TRANSCODE_CAP,
        default: "0",
        description: "Most jobs re-encoding a stream the server runs at once; past it a user's \
                      idle job on the same device is ended, and a new one is refused if that \
                      makes no room. 0 means no cap.",
        running: |c| json!(c.max_transcodes),
        ..ROW
    },
    SettingSpec {
        key: "max_transcodes_per_user",
        env: Some("FERROFIN_MAX_TRANSCODES_PER_USER"),
        env_read: PARSED_U32,
        label: "Transcode limit per user",
        group: Group::Performance,
        kind: TRANSCODE_CAP,
        default: "0",
        description: "Most jobs re-encoding a stream one user runs at once; past it their \
                      idle job on the same device is ended, and a new one is refused if none \
                      is idle. 0 means no cap.",
        running: |c| json!(c.max_transcodes_per_user),
        ..ROW
    },
    SettingSpec {
        key: "ffmpeg_analyze_duration",
        env: Some("FERROFIN_FFMPEG_ANALYZE_DURATION"),
        env_read: EnvRead::Parsed(parses::<FfmpegCount>),
        label: "ffmpeg analyze duration",
        group: Group::Performance,
        kind: SettingKind::Text,
        default: "200M",
        description: "How long ffmpeg analyses an input to find its streams, in microseconds: \
                      lower starts a slow or remote source sooner, and can miss a late stream.",
        note: "Jellyfin's FFmpeg:analyzeduration, for probes, transcodes and external \
               subtitles, in ffmpeg's syntax: 200M is 200 seconds (K, M and G are powers of \
               1000; Ki, Mi and Gi of 1024). 1M to 600M. A source's own duration, such as a \
               Live TV channel's 3 seconds, wins.",
        running: |c| json!(c.ffmpeg_analyze_duration.to_string()),
        check: |value| ffmpeg_count_within(value, &ANALYZE_DURATION_RANGE),
        ..ROW
    },
    SettingSpec {
        key: "ffmpeg_probe_size",
        env: Some("FERROFIN_FFMPEG_PROBE_SIZE"),
        env_read: EnvRead::Parsed(parses::<FfmpegCount>),
        label: "ffmpeg probe size",
        group: Group::Performance,
        kind: SettingKind::Text,
        default: "1G",
        description: "How many bytes of an input ffmpeg reads to find its streams: lower starts \
                      a slow or remote source sooner, and can miss a late stream.",
        note: "Jellyfin's FFmpeg:probesize, for probes, transcodes and external subtitles, in \
               ffmpeg's syntax: 1G is 10^9 bytes (K, M and G are powers of 1000; Ki, Mi and Gi \
               of 1024). 1M to 4G.",
        running: |c| json!(c.ffmpeg_probe_size.to_string()),
        check: |value| ffmpeg_count_within(value, &PROBE_SIZE_RANGE),
        ..ROW
    },
    SettingSpec {
        key: "playback_metrics_queue",
        env: Some("FERROFIN_PLAYBACK_METRICS_QUEUE"),
        env_read: PARSED_U32,
        label: "Playback metrics queue",
        group: Group::Performance,
        kind: whole_numbers(config::PLAYBACK_METRICS_QUEUE_RANGE),
        unit: Some(Unit::Events),
        default: "1024",
        description: "Depth of the playback-metrics write queue.",
        running: |c| json!(c.playback_metrics_queue),
        ..ROW
    },
    SettingSpec {
        key: "session_idle_hours",
        env: Some("FERROFIN_SESSION_IDLE_HOURS"),
        env_read: PARSED_U32,
        label: "Idle session lifetime",
        group: Group::Performance,
        kind: whole_numbers(config::SESSION_IDLE_HOURS_RANGE),
        unit: Some(Unit::Hours),
        default: "24",
        description: "How long a session with no WebSocket that plays nothing is kept after its \
                      last request.",
        note: "Jellyfin keeps such a session until it restarts. Ending it frees its memory, and \
               for a signed-in user the activity log notes the disconnect.",
        running: |c| json!(c.session_idle_hours),
        ..ROW
    },
    // ---- Logging -----------------------------------------------------------
    SettingSpec {
        key: "log_level",
        env: Some("FERROFIN_LOG"),
        label: "Log level",
        group: Group::Logging,
        applies: Applies::ProcessRestart,
        default: "info",
        description: "Log filter, in RUST_LOG syntax; RUST_LOG wins when it is set.",
        running: |c| json!(c.log_level),
        // An invalid filter logs at `info` (`bootstrap::init_tracing`).
        check: |value| {
            value
                .as_str()
                .filter(|filter| tracing_subscriber::EnvFilter::try_new(filter).is_ok())
                .map(drop)
                .ok_or_else(|| "is not a log filter in RUST_LOG syntax, e.g. info".to_owned())
        },
        ..ROW
    },
    SettingSpec {
        key: "log_format",
        env: Some("FERROFIN_LOG_FORMAT"),
        env_read: EnvRead::NonBlank,
        label: "Log format",
        group: Group::Logging,
        kind: SettingKind::Choice,
        applies: Applies::ProcessRestart,
        default: "json",
        description: "How the server writes its log lines to stdout.",
        // The loader takes either in any case; anything else runs as json,
        // with a warning (`config::resolve_log_format`), and shows as Custom.
        options: &[
            SettingOption {
                value: "json",
                label: "JSON",
                note: "One JSON object per line, for log collectors",
            },
            SettingOption {
                value: "text",
                label: "Text",
                note: "Plain lines, for reading by hand",
            },
        ],
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
        label: "Prometheus metrics",
        group: Group::Logging,
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
        label: "Metrics sample interval",
        group: Group::Logging,
        kind: whole_numbers(config::METRICS_SAMPLE_INTERVAL_RANGE),
        unit: Some(Unit::Seconds),
        default: "15",
        description: "Time between process-metric samples.",
        running: |c| json!(c.metrics_sample_interval),
        ..ROW
    },
    SettingSpec {
        key: "metrics_scan_duration_buckets",
        env: Some("FERROFIN_METRICS_SCAN_DURATION_BUCKETS"),
        env_read: EnvRead::NonBlank,
        label: "Scan duration buckets",
        group: Group::Logging,
        kind: SettingKind::NumberList,
        unit: Some(Unit::Seconds),
        applies: Applies::ProcessRestart,
        default: "1–2.5–5 per decade, 0.001 to 5000",
        description: "Bucket boundaries of the library-scan duration histograms.",
        running: |c| json!(c.metrics_scan_duration_buckets),
        // An invalid list is the default, with a warning (`scan_metrics::install`).
        check: |value| {
            let buckets: Vec<f64> = value
                .as_array()
                .map(|list| list.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            ferrofin_core::scan_metrics::validate_duration_buckets(&buckets)
                .map_err(|why| format!("is not a list of bucket boundaries: {why}"))
        },
        ..ROW
    },
    SettingSpec {
        key: "OTEL_EXPORTER_OTLP_ENDPOINT",
        env: Some("OTEL_EXPORTER_OTLP_ENDPOINT"),
        // A blank endpoint exports nothing (`bootstrap::build_tracer_provider`).
        env_read: EnvRead::NonBlank,
        label: "Trace export endpoint",
        group: Group::Logging,
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
        label: "Trace sampling ratio",
        group: Group::Logging,
        access: SettingAccess::EnvOnly,
        applies: Applies::ProcessRestart,
        default: "0.25",
        description: "Share of traces exported when trace export is on, 0 to 1.",
        ..ROW
    },
    // ---- Plugins -----------------------------------------------------------
    SettingSpec {
        key: "wasm_call_timeout_secs",
        env: Some("FERROFIN_WASM_CALL_TIMEOUT_SECS"),
        env_read: PARSED_U32,
        label: "Plugin call timeout",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_CALL_TIMEOUT_SECS_RANGE),
        unit: Some(Unit::Seconds),
        default: "30",
        description: "Deadline of one plugin call.",
        running: |c| json!(c.wasm_call_timeout_secs),
        ..ROW
    },
    SettingSpec {
        key: "wasm_call_grace_secs",
        env: Some("FERROFIN_WASM_CALL_GRACE_SECS"),
        env_read: PARSED_U32,
        label: "Plugin call grace",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_CALL_GRACE_SECS_RANGE),
        unit: Some(Unit::Seconds),
        default: "10",
        description: "Wait past a plugin call's deadline before the plugin is disabled as stuck.",
        running: |c| json!(c.wasm_call_grace_secs),
        ..ROW
    },
    SettingSpec {
        key: "wasm_memory_limit_mb",
        env: Some("FERROFIN_WASM_MEMORY_LIMIT_MB"),
        env_read: PARSED_U32,
        label: "Plugin memory limit",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_MEMORY_LIMIT_MB_RANGE),
        unit: Some(Unit::Mebibytes),
        default: "128",
        description: "Memory cap of one plugin.",
        running: |c| json!(c.wasm_memory_limit_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_event_queue_capacity",
        env: Some("FERROFIN_WASM_EVENT_QUEUE_CAPACITY"),
        env_read: PARSED_U32,
        label: "Plugin event queue",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_EVENT_QUEUE_CAPACITY_RANGE),
        unit: Some(Unit::Events),
        default: "256",
        description: "Event queue depth of one plugin.",
        running: |c| json!(c.wasm_event_queue_capacity),
        ..ROW
    },
    SettingSpec {
        key: "wasm_private_http_allow",
        env: Some("FERROFIN_WASM_PRIVATE_HTTP_ALLOW"),
        label: "Plugins allowed on private networks",
        group: Group::Plugins,
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
        label: "Plugin download limit",
        group: Group::Plugins,
        kind: whole_numbers(config::MAX_PLUGIN_DOWNLOAD_MB_RANGE),
        unit: Some(Unit::Mebibytes),
        default: "128",
        description: "Size cap of a plugin downloaded from a repository.",
        running: |c| json!(c.max_plugin_download_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_analysis_concurrency",
        env: Some("FERROFIN_WASM_ANALYSIS_CONCURRENCY"),
        env_read: PARSED_U32,
        label: "Concurrent plugin analyses",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_ANALYSIS_CONCURRENCY_RANGE),
        default: "a quarter of the cores, at least one",
        description: "Media decodes plugin analysis may run at once.",
        running: |c| json!(c.wasm_analysis_concurrency),
        ..ROW
    },
    SettingSpec {
        key: "wasm_state_limit_mb",
        env: Some("FERROFIN_WASM_STATE_LIMIT_MB"),
        env_read: PARSED_U32,
        label: "Plugin state limit",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_STATE_LIMIT_MB_RANGE),
        unit: Some(Unit::Mebibytes),
        default: "8",
        description: "Key/value state cap of one plugin.",
        running: |c| json!(c.wasm_state_limit_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_image_download_mb",
        env: Some("FERROFIN_WASM_IMAGE_DOWNLOAD_MB"),
        env_read: PARSED_U32,
        label: "Plugin artwork download limit",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_IMAGE_DOWNLOAD_MB_RANGE),
        unit: Some(Unit::Mebibytes),
        default: "20",
        description: "Size cap of one plugin artwork download.",
        running: |c| json!(c.wasm_image_download_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_image_timeout_secs",
        env: Some("FERROFIN_WASM_IMAGE_TIMEOUT_SECS"),
        env_read: PARSED_U32,
        label: "Plugin artwork download timeout",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_IMAGE_TIMEOUT_SECS_RANGE),
        unit: Some(Unit::Seconds),
        default: "30",
        description: "Time limit of one plugin artwork download.",
        running: |c| json!(c.wasm_image_timeout_secs),
        ..ROW
    },
    SettingSpec {
        key: "wasm_write_content_mb",
        env: Some("FERROFIN_WASM_WRITE_CONTENT_MB"),
        env_read: PARSED_U32,
        label: "Plugin lyrics and subtitles limit",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_WRITE_CONTENT_MB_RANGE),
        unit: Some(Unit::Mebibytes),
        default: "2",
        description: "Size cap of the lyrics or subtitles a plugin writes.",
        running: |c| json!(c.wasm_write_content_mb),
        ..ROW
    },
    SettingSpec {
        key: "wasm_subtitle_extract_mb",
        env: Some("FERROFIN_WASM_SUBTITLE_EXTRACT_MB"),
        env_read: PARSED_U32,
        label: "Subtitle extraction limit",
        group: Group::Plugins,
        kind: whole_numbers(config::WASM_SUBTITLE_EXTRACT_MB_RANGE),
        unit: Some(Unit::Mebibytes),
        default: "10",
        description: "Size cap of one subtitle track a plugin extracts.",
        running: |c| json!(c.wasm_subtitle_extract_mb),
        ..ROW
    },
    // ---- System ------------------------------------------------------------
    SettingSpec {
        key: "data_dir",
        env: Some("FERROFIN_DATA_DIR"),
        flag: Some("--data-dir"),
        label: "Data directory",
        group: Group::System,
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
        label: "Configuration directory",
        group: Group::System,
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
        label: "Cache directory",
        group: Group::System,
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
        label: "Web client directory",
        group: Group::System,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(SET_AT_STARTUP),
        default: "{data_dir}/web",
        description: "The built jellyfin-web served at /web; without it the server is API-only.",
        running: |c| json!(c.web_dir),
        ..ROW
    },
    SettingSpec {
        key: "ffmpeg_path",
        env: Some("FERROFIN_FFMPEG_PATH"),
        flag: Some("--ffmpeg"),
        label: "ffmpeg path",
        group: Group::System,
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
        label: "ffprobe path",
        group: Group::System,
        kind: SettingKind::Path,
        access: SettingAccess::ReadOnly(NO_EXECUTABLE_FROM_THE_WEB),
        default: "found on $PATH",
        description: "Path to ffprobe, which library scans run.",
        running: |c| json!(c.ffprobe_path),
        ..ROW
    },
    SettingSpec {
        key: "library_roots",
        env: Some("FERROFIN_LIBRARY_ROOTS"),
        label: "Initial libraries",
        group: Group::System,
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
        label: "Initial administrator",
        group: Group::System,
        access: SettingAccess::ReadOnly(FIRST_BOOT_ONLY),
        default: "admin",
        description: "Username of the administrator seeded on first boot.",
        running: |c| json!(c.admin_user),
        ..ROW
    },
    SettingSpec {
        key: "admin_password",
        env: Some("FERROFIN_ADMIN_PASSWORD"),
        label: "Initial administrator password",
        group: Group::System,
        kind: SettingKind::Secret,
        access: SettingAccess::ReadOnly(FIRST_BOOT_ONLY),
        default: "none: the administrator is seeded without a password",
        description: "Password of the administrator seeded on first boot.",
        running: |c| json!(c.admin_password),
        ..ROW
    },
    SettingSpec {
        key: "shutdown_timeout_secs",
        env: Some("FERROFIN_SHUTDOWN_TIMEOUT_SECS"),
        env_read: PARSED_U32,
        label: "Shutdown timeout",
        group: Group::System,
        kind: whole_numbers(config::SHUTDOWN_TIMEOUT_SECS_RANGE),
        unit: Some(Unit::Seconds),
        default: "30",
        description: "How long a shutdown or restart waits for requests in flight.",
        running: |c| json!(c.shutdown_timeout_secs),
        ..ROW
    },
    SettingSpec {
        key: "disable_extensions",
        env: Some("FERROFIN_DISABLE_EXTENSIONS"),
        env_read: PARSED_BOOL,
        label: "Disable extensions",
        group: Group::System,
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

/// A whole-number setting's kind: the values the loader clamps it to, from
/// the range next to where it is resolved (`config`, `ImageLimits`,
/// `RequestLimits`).
const fn whole_numbers(range: std::ops::RangeInclusive<u32>) -> SettingKind {
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

/// A save of an ffmpeg input-analysis value (`ffmpeg_analyze_duration`,
/// `ffmpeg_probe_size`): blank for the default, else ffmpeg's syntax inside
/// `range`, which the loader would otherwise ignore or clamp with a warning.
fn ffmpeg_count_within(value: &Value, range: &RangeInclusive<FfmpegCount>) -> Result<(), String> {
    let text = value.as_str().map(str::trim).unwrap_or_default();
    if text.is_empty() {
        return Ok(());
    }
    let count: FfmpegCount = text.parse().map_err(str::to_owned)?;
    if range.contains(&count) {
        Ok(())
    } else {
        Err(format!("is outside {} to {}", range.start(), range.end()))
    }
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
        // A row the Networking page covers says so in its note.
        Source::NetworkConfiguration
        | Source::ConfigFile
        | Source::ServerConfiguration
        | Source::Default => None,
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

/// The value `file`, `config.toml` as it is now, gives `spec`, when `in_use` is
/// the value in use. A count's `auto` runs as no value, so it is named here:
/// the file holds it, and the page shows it as saved.
fn file_value(spec: &SettingSpec, file: &Config, in_use: Option<&Value>) -> Option<Value> {
    match (spec.running)(file) {
        Value::Null if spec.kind == SettingKind::CountOrAuto && in_use.is_some() => {
            Some(json!("auto"))
        }
        value => present(value),
    }
}

/// What the page receives for `spec`. `running` is the configuration in use;
/// `saved` is `config.toml` as it is now, loaded alone, when it has keys;
/// `configuration` is the server configuration, for a row stored there;
/// `network` the network configuration the lifetime started with.
fn setting(
    spec: &SettingSpec,
    running: &Config,
    env: &dyn Env,
    file_keys: &[String],
    saved: Option<&Config>,
    configuration: &ServerConfiguration,
    network: &NetworkConfiguration,
) -> ServerSettingDto {
    let (kind, min, max) = spec.kind.to_dto();
    let mut dto = ServerSettingDto {
        key: spec.key.to_owned(),
        label: spec.label.to_owned(),
        group: spec.group,
        kind,
        unit: spec.unit,
        description: spec.description.to_owned(),
        note: (!spec.note.is_empty()).then(|| spec.note.to_owned()),
        env_var: spec.env.map(str::to_owned),
        flag: spec.flag.map(str::to_owned),
        default: (!spec.default.is_empty()).then(|| spec.default.to_owned()),
        value: None,
        saved_value: None,
        source: Source::Default,
        editable: true,
        locked_reason: None,
        applies: spec.applies,
        min,
        max,
        options: spec
            .options
            .iter()
            .map(|option| ServerSettingOptionDto {
                value: option.value.to_owned(),
                label: option.label.to_owned(),
                note: (!option.note.is_empty()).then(|| option.note.to_owned()),
            })
            .collect(),
        store: ServerSettingStore::ConfigFile,
        custom: false,
    };
    match spec.store {
        SettingStore::ConfigFile => {
            from_the_file(spec, running, env, file_keys, saved, network, &mut dto);
        }
        SettingStore::ServerConfiguration(field) => {
            // Nothing outranks the server configuration, and the page may
            // always change it. A new server's value is its default.
            let value = (field.read)(configuration);
            dto.source = if value == (field.read)(&ServerConfiguration::default()) {
                Source::Default
            } else {
                Source::ServerConfiguration
            };
            dto.custom = spec.kind == SettingKind::Choice
                && !value
                    .as_str()
                    .is_some_and(|text| fits_an_option(spec, text));
            dto.value = present(value);
            dto.store = ServerSettingStore::ServerConfiguration;
        }
    }
    dto
}

/// Whether `text` is one of `spec`'s options: the same value, ignoring case
/// and the spaces around it, as the loader reads a choice
/// (`config::resolve_log_format`).
fn fits_an_option(spec: &SettingSpec, text: &str) -> bool {
    spec.options
        .iter()
        .any(|option| option.value.eq_ignore_ascii_case(text.trim()))
}

/// Fills `dto` for `spec`, a row stored in `config.toml`: its value in use,
/// the file's value when that differs, its source and whether the page may
/// change it. A choice whose `config.toml` value fits no option reports that
/// value as written, as Custom: the loader runs the default in its place. A
/// value the Networking page saved in `network` outranks the file.
fn from_the_file(
    spec: &SettingSpec,
    running: &Config,
    env: &dyn Env,
    file_keys: &[String],
    saved: Option<&Config>,
    network: &NetworkConfiguration,
    dto: &mut ServerSettingDto,
) {
    let mut source = source(spec, &running.cli, env, file_keys);
    let on_the_page = present((spec.network)(network));
    if on_the_page.is_some() && matches!(source, Source::ConfigFile | Source::Default) {
        source = Source::NetworkConfiguration;
    }
    let value = match spec.access {
        _ if source == Source::NetworkConfiguration => on_the_page,
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
        .and_then(|saved| file_value(spec, saved, value.as_ref()))
        .filter(|saved| Some(saved) != value.as_ref());
    // The report read the file's keys a moment ago, so a read that fails now
    // is the file changing under it: the value then reports as it loads.
    let custom = (spec.kind == SettingKind::Choice && source == Source::ConfigFile)
        .then(|| config::config_file_text(&running.config_file, spec.key))
        .and_then(Result::ok)
        .flatten()
        .filter(|text| !fits_an_option(spec, text));
    let locked_reason = locked_reason(spec, source, env);
    dto.source = source;
    dto.editable = locked_reason.is_none();
    dto.locked_reason = locked_reason;
    if let Some(text) = custom {
        dto.value = Some(Value::String(text));
        dto.custom = true;
    } else {
        dto.value = value;
        dto.saved_value = saved_value;
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

/// Every one of `rows`, as the page receives them: those stored in
/// `config.toml` as `running` holds them, unless `network` outranks it, the
/// others as `configuration` does.
fn snapshot(
    rows: &[SettingSpec],
    running: &Config,
    env: &dyn Env,
    configuration: &ServerConfiguration,
    network: &NetworkConfiguration,
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
        config_file_writable: config_file_writable(path),
        restart_pending,
        settings: rows
            .iter()
            .map(|spec| {
                setting(
                    spec,
                    running,
                    env,
                    &file_keys,
                    saved.as_ref(),
                    configuration,
                    network,
                )
            })
            .collect(),
    })
}

/// Whether a save could write `config.toml` at `path`: the directory of the
/// file it writes (through a link, the file the link names) takes a new file,
/// and that file, when it exists, is not read-only. A save also opens the file
/// for writing first ([`save`]); the report does not, so a page load never
/// looks like a write to a tool watching the file.
fn config_file_writable(path: &Path) -> bool {
    save::write_target(path)
        .is_ok_and(|target| directory_accepts_a_file(&target) && !save::is_read_only(&target))
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

/// Serializes saves, so two never interleave their read, check and rename of
/// `config.toml`. One per process, not per manager: an in-process restart
/// builds a new manager while the old lifetime's saves may still run.
static SAVES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The settings manager behind `GET /Ferrofin/Settings` and
/// `POST /Ferrofin/Settings`: the configuration this server lifetime runs
/// with, where each value comes from, and saving changes to `config.toml` and,
/// for a row stored there, to the server configuration.
pub struct FerrofinServerSettings {
    /// The configuration in use, with the flags it was loaded under.
    config: Config,
    /// The process environment the configuration was loaded from.
    env: Arc<dyn Env + Send + Sync>,
    /// For the restart-pending flag.
    lifecycle: Arc<dyn LifecycleController>,
    /// For the Ferrofin plugin's enabled flag.
    plugins: Arc<dyn PluginManager>,
    /// The server configuration, which the rows stored there read and save.
    configuration: Arc<dyn ServerConfigurationManager>,
    /// The network configuration this lifetime started with, whose
    /// Networking page values outrank `config.toml` ([`SettingSpec::network`]).
    network: NetworkConfiguration,
    /// The rows it reports and saves: [`SETTINGS`].
    rows: &'static [SettingSpec],
}

impl FerrofinServerSettings {
    /// Reports `config`, which the process environment and the flags it
    /// records were loaded into, with `lifecycle`'s restart flag, and saves to
    /// its `config.toml`, marking a restart pending in `lifecycle`; a row
    /// stored in the server configuration is read from and saved to
    /// `configuration`. Answers only while the Ferrofin plugin is enabled in
    /// `plugins`.
    #[must_use]
    pub fn new(
        config: Config,
        lifecycle: Arc<dyn LifecycleController>,
        plugins: Arc<dyn PluginManager>,
        configuration: Arc<dyn ServerConfigurationManager>,
    ) -> Self {
        Self {
            config,
            env: Arc::new(config::EnvProvider),
            lifecycle,
            plugins,
            configuration,
            network: NetworkConfiguration::default(),
            rows: SETTINGS,
        }
    }

    /// The same, reporting the values the Networking page saved in
    /// `network`, the network configuration this lifetime started with.
    #[must_use]
    pub fn with_network(mut self, network: NetworkConfiguration) -> Self {
        self.network = network;
        self
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
        let configuration = self.configuration.configuration().await?;
        let running = self.config.clone();
        let network = self.network.clone();
        let env = Arc::clone(&self.env);
        let rows = self.rows;
        // Reads config.toml and probes its directory.
        tokio::task::spawn_blocking(move || {
            snapshot(
                rows,
                &running,
                env.as_ref(),
                &configuration,
                &network,
                restart_pending,
            )
        })
        .await
        .map_err(|err| ServiceError::backend(format!("settings report failed: {err}")))?
    }

    async fn update(
        &self,
        update: ServerSettingsUpdateDto,
    ) -> Result<ServerSettingsUpdateResultDto, ServiceError> {
        self.ensure_enabled().await?;
        let save = SaveRequest {
            running: self.config.clone(),
            env: Arc::clone(&self.env),
            lifecycle: Arc::clone(&self.lifecycle),
            configuration: Arc::clone(&self.configuration),
            rows: self.rows,
            changes: update.changes,
        };
        // A task of its own, which runs to its end even when the caller goes
        // away: the lock is never let go, or the restart left unmarked,
        // part-way through a save.
        tokio::spawn(save.run().instrument(tracing::Span::current()))
            .await
            .map_err(|err| ServiceError::backend(format!("settings save failed: {err}")))?
    }
}

/// One `POST /Ferrofin/Settings`.
struct SaveRequest {
    /// The configuration in use, whose `config.toml` the save edits.
    running: Config,
    /// The process environment, whose variables lock the rows they set.
    env: Arc<dyn Env + Send + Sync>,
    /// Marks the restart a `config.toml` change needs.
    lifecycle: Arc<dyn LifecycleController>,
    /// Saves the rows stored in the server configuration.
    configuration: Arc<dyn ServerConfigurationManager>,
    /// The rows a change may name.
    rows: &'static [SettingSpec],
    /// What to change.
    changes: save::Changes,
}

impl SaveRequest {
    /// Saves the changes under [`SAVES`], all or nothing. Every change is
    /// checked first. The new `config.toml` is then written beside the file
    /// and loaded ([`save::stage`]) before the server configuration is
    /// updated, and renamed over the file only after that update, so a
    /// refusal, a file that does not load or a failed update leaves both as
    /// they were. One case is left: a rename that fails after the update (the
    /// file was found writable and loaded just before) leaves the server
    /// configuration's change applied and `config.toml` as it was. A change to
    /// the server configuration applies at once; one to `config.toml` marks
    /// the restart, and one line logs the keys.
    async fn run(self) -> Result<ServerSettingsUpdateResultDto, ServiceError> {
        let _one_save_at_a_time = SAVES.lock().await;
        let (stored, filed) = save::split(self.rows, self.changes)?;

        // Reads, writes and loads files; a save of the server configuration
        // alone never reads `config.toml`, so a broken one cannot stop it.
        let staged = if filed.is_empty() {
            None
        } else {
            let running = self.running.clone();
            let env = Arc::clone(&self.env);
            let rows = self.rows;
            tokio::task::spawn_blocking(move || save::stage(rows, &running, env.as_ref(), &filed))
                .await
                .map_err(|err| ServiceError::backend(format!("settings save failed: {err}")))??
        };

        // Read just before it is written, so a Dashboard save made while the
        // file was staged is not written over.
        let mut applied_now = Vec::new();
        if !stored.is_empty() {
            let current = self.configuration.configuration().await?;
            let (configuration, keys) = save::configure(&current, &stored);
            if !keys.is_empty() {
                self.configuration
                    .update_configuration(&configuration)
                    .await?;
                applied_now = keys;
            }
        }
        let saved = match staged {
            Some(staged) => tokio::task::spawn_blocking(move || staged.commit())
                .await
                .map_err(|err| ServiceError::backend(format!("settings save failed: {err}")))??,
            None => Vec::new(),
        };
        Ok(report(
            &self.running,
            self.lifecycle.as_ref(),
            &saved,
            applied_now,
        ))
    }
}

/// What a save did: `saved`, the rows whose `config.toml` entry changed, mark
/// the restart in `lifecycle`; `applied_now`, the keys of the server
/// configuration that changed, applied at once. One line logs the keys.
fn report(
    running: &Config,
    lifecycle: &dyn LifecycleController,
    saved: &[&'static SettingSpec],
    applied_now: Vec<String>,
) -> ServerSettingsUpdateResultDto {
    if saved.is_empty() && applied_now.is_empty() {
        return ServerSettingsUpdateResultDto::default();
    }
    if !saved.is_empty() {
        lifecycle.mark_restart_required();
    }
    let keys: Vec<String> = saved.iter().map(|spec| spec.key.to_owned()).collect();
    // Key names only: a value may be a key or a password (owner decision D3).
    tracing::info!(
        path = %running.config_file.display(),
        keys = %keys.join(", "),
        applied_now = %applied_now.join(", "),
        "server settings saved"
    );
    ServerSettingsUpdateResultDto {
        restart_required: !keys.is_empty(),
        changed: keys,
        process_restart_required: saved
            .iter()
            .filter(|spec| spec.applies == Applies::ProcessRestart)
            .map(|spec| spec.key.to_owned())
            .collect(),
        applied_now,
    }
}

mod save;

#[cfg(test)]
mod tests;
