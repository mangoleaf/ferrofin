//! The settings table's drift tests and the report's sources, locks, notes
//! and saved values, against the real loader with a fake environment and a
//! temporary `config.toml`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{CommandFactory, Parser};
use ferrofin_model::configuration::TrickplayOptions;
use ferrofin_traits::plugins::PluginManager;

use super::*;

/// A fake environment backed by a map — no real `std::env` mutation.
#[derive(Clone, Default)]
pub(super) struct FakeEnv(HashMap<String, String>);

impl FakeEnv {
    /// An environment whose default data directory no host has, so a test
    /// reads no `config.toml` of the host's.
    pub(super) fn new() -> Self {
        Self::default().with("XDG_DATA_HOME", "/nonexistent/ferrofin-settings-test")
    }

    pub(super) fn with(mut self, key: &str, value: &str) -> Self {
        self.0.insert(key.to_owned(), value.to_owned());
        self
    }

    fn without(mut self, key: &str) -> Self {
        self.0.remove(key);
        self
    }
}

impl Env for FakeEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// A temporary data directory and its `config.toml`, written from `lines`
/// (none: no file).
pub(super) struct DataDir {
    dir: tempfile::TempDir,
}

impl DataDir {
    pub(super) fn new(lines: &[String]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        if !lines.is_empty() {
            std::fs::write(dir.path().join("config.toml"), lines.join("\n")).expect("write");
        }
        Self { dir }
    }

    pub(super) fn path(&self) -> &Path {
        self.dir.path()
    }

    pub(super) fn config_file(&self) -> PathBuf {
        self.path().join("config.toml")
    }

    /// The flags that start the server on this data directory.
    pub(super) fn cli(&self) -> Cli {
        Cli {
            data_dir: Some(self.path().to_path_buf()),
            ..Cli::default()
        }
    }
}

/// A server configuration manager holding one configuration in memory, as
/// the real one serves it to `GET /System/Configuration`: the JSON it would
/// answer is [`json`](Self::json), and every read and update is counted. One
/// built [`failing`](Self::failing) refuses its updates.
pub(super) struct FakeConfiguration {
    current: std::sync::Mutex<Arc<ServerConfiguration>>,
    reads: std::sync::atomic::AtomicUsize,
    updates: std::sync::atomic::AtomicUsize,
    fail: bool,
    paths: Arc<ferrofin_core::FerrofinServerApplicationPaths>,
}

impl FakeConfiguration {
    pub(super) fn new(configuration: ServerConfiguration) -> Arc<Self> {
        Arc::new(Self {
            current: std::sync::Mutex::new(Arc::new(configuration)),
            reads: std::sync::atomic::AtomicUsize::new(0),
            updates: std::sync::atomic::AtomicUsize::new(0),
            fail: false,
            paths: Arc::new(ferrofin_core::FerrofinServerApplicationPaths::new(
                "/nonexistent/data",
                "/nonexistent/log",
                "/nonexistent/config",
                "/nonexistent/cache",
                "/nonexistent/web",
            )),
        })
    }

    pub(super) fn failing(configuration: ServerConfiguration) -> Arc<Self> {
        let mut fake = Arc::into_inner(Self::new(configuration)).expect("one owner");
        fake.fail = true;
        Arc::new(fake)
    }

    /// `GET /System/Configuration`'s body: the configuration as it serializes.
    pub(super) fn json(&self) -> String {
        serde_json::to_string(&**self.current.lock().expect("lock")).expect("serialize")
    }

    pub(super) fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) fn updates(&self) -> usize {
        self.updates.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ServerConfigurationManager for FakeConfiguration {
    fn application_paths(&self) -> Arc<dyn ferrofin_traits::system::ServerApplicationPaths> {
        Arc::clone(&self.paths) as Arc<_>
    }

    async fn configuration(&self) -> Result<Arc<ServerConfiguration>, ServiceError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::clone(&self.current.lock().expect("lock")))
    }

    async fn update_configuration(
        &self,
        configuration: &ServerConfiguration,
    ) -> Result<(), ServiceError> {
        if self.fail {
            return Err(ServiceError::backend("the configuration cannot be saved"));
        }
        self.updates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.current.lock().expect("lock") = Arc::new(configuration.clone());
        Ok(())
    }

    async fn get_branding(
        &self,
    ) -> Result<ferrofin_model::branding::BrandingOptions, ServiceError> {
        Ok(ferrofin_model::branding::BrandingOptions::default())
    }

    async fn update_branding(
        &self,
        _branding: &ferrofin_model::branding::BrandingOptions,
    ) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// The key of a row stored in the server configuration: Image Quality, a
/// choice with a note per option, whose other `Qscale` and chroma pairs are
/// Custom.
pub(super) const STORED: &str = "trickplay_image_quality";

pub(super) fn spec(key: &str) -> &'static SettingSpec {
    SETTINGS
        .iter()
        .find(|spec| spec.key == key)
        .unwrap_or_else(|| panic!("no row `{key}`"))
}

/// A `config.toml` value for `spec`'s key that the loader takes and that is
/// not the default.
pub(super) fn sample(spec: &SettingSpec) -> String {
    match spec.key {
        "bind_addr" => "\"127.0.0.2\"".to_owned(),
        "log_level" => "\"debug\"".to_owned(),
        "wasm_private_http_allow" => "\"*\"".to_owned(),
        "ffmpeg_analyze_duration" => "\"8M\"".to_owned(),
        "ffmpeg_probe_size" => "\"32M\"".to_owned(),
        _ => match spec.kind {
            // The value that is not the default, so setting it changes something.
            SettingKind::Bool => (spec.default != "true").to_string(),
            // Inside the range and not the default.
            SettingKind::Integer { min, .. } if (min + 1).to_string() == spec.default => {
                (min + 2).to_string()
            }
            SettingKind::Integer { min, .. } => (min + 1).to_string(),
            SettingKind::CountOrAuto => "2".to_owned(),
            SettingKind::Text | SettingKind::Secret => "\"sample\"".to_owned(),
            SettingKind::Url => "\"http://sample.invalid\"".to_owned(),
            SettingKind::Path => "\"/sample\"".to_owned(),
            SettingKind::PathList => "[\"/sample\"]".to_owned(),
            SettingKind::NumberList => "[0.5, 1.0]".to_owned(),
            // An option other than the default.
            SettingKind::Choice => spec
                .options
                .iter()
                .find(|option| option.value != spec.default)
                .map_or_else(
                    || panic!("`{}` offers nothing but its default", spec.key),
                    |option| format!("\"{}\"", option.value),
                ),
        },
    }
}

/// The rows `config.toml` holds: not environment-only, not stored in the
/// server configuration.
fn file_rows() -> impl Iterator<Item = &'static SettingSpec> {
    SETTINGS.iter().filter(|spec| {
        spec.access != SettingAccess::EnvOnly && matches!(spec.store, SettingStore::ConfigFile)
    })
}

/// The report `config` gives under `env`, the way the route runs it.
fn report(config: &Config, env: &FakeEnv) -> ServerSettingsDto {
    snapshot(
        SETTINGS,
        config,
        env,
        &ServerConfiguration::default(),
        &NetworkConfiguration::default(),
        false,
    )
    .expect("snapshot")
}

fn entry<'a>(report: &'a ServerSettingsDto, key: &str) -> &'a ServerSettingDto {
    report
        .settings
        .iter()
        .find(|setting| setting.key == key)
        .unwrap_or_else(|| panic!("no setting `{key}` in the report"))
}

pub(super) fn load(cli: Cli, env: &FakeEnv) -> Config {
    Config::load_from(cli, env).expect("load")
}

// ---- drift: the table against the loader and the docs ----------------------

/// (a) Every `config.toml` key has exactly one row, and every row's key is
/// one (or, environment-only, has none).
#[test]
fn every_config_toml_key_has_one_row_and_every_rows_key_is_one() {
    let keys: BTreeSet<&str> = config::file_config_keys().iter().copied().collect();
    let rows: Vec<&str> = file_rows().map(|spec| spec.key).collect();
    let unique: BTreeSet<&str> = SETTINGS.iter().map(|spec| spec.key).collect();
    assert_eq!(unique.len(), SETTINGS.len(), "a key has two rows");
    assert_eq!(
        rows.iter().copied().collect::<BTreeSet<_>>(),
        keys,
        "rows and config.toml keys differ"
    );
    for spec in SETTINGS
        .iter()
        .filter(|s| s.access == SettingAccess::EnvOnly)
    {
        assert!(!keys.contains(spec.key), "{} is a key", spec.key);
        assert_eq!(
            spec.env,
            Some(spec.key),
            "an env-only row is keyed by its variable"
        );
    }
}

/// (b) Every row's variable, or the label of a row stored in the server
/// configuration (which has none), has a row in `docs/CONFIG.md`; every row
/// has a description and a default.
#[test]
fn every_rows_variable_has_a_row_in_the_docs() {
    let docs = include_str!("../../../../docs/CONFIG.md");
    for spec in SETTINGS {
        let named = match spec.store {
            SettingStore::ServerConfiguration(_) => format!("| {} |", spec.label),
            SettingStore::ConfigFile => format!(
                "| `{}` |",
                spec.env
                    .unwrap_or_else(|| panic!("{} has no variable", spec.key))
            ),
        };
        assert!(docs.contains(&named), "{named}: no row in docs/CONFIG.md");
        assert!(!spec.description.is_empty(), "{}: no description", spec.key);
        assert!(!spec.default.is_empty(), "{}: no default", spec.key);
    }
}

/// The page's tabs and the rows on each, in order: owner decision D5, and the
/// owner's placement of the rows D5's table predates (on gl-1172) and of
/// `detect_network_change` (on gl-1196).
const TABS: &[(Group, &[&str])] = &[
    (
        Group::Network,
        &[
            "bind_addr",
            "port",
            "https_port",
            "published_url",
            "base_url",
            "server_name",
            "quick_connect_pending_per_ip",
            "detect_network_change",
            "login_backoff",
            "login_backoff_attempts",
            "login_backoff_max_delay_secs",
            "login_backoff_window_mins",
            "login_backoff_max_tracked",
        ],
    ),
    (
        Group::Library,
        &[
            "scan_probe_concurrency",
            "scan_progress_every",
            "scan_directory_cache_records",
            "scan_directory_cache_idle_secs",
            "book_archive_max_entries",
        ],
    ),
    (
        Group::Trickplay,
        &[
            "trickplay_preview_size",
            "trickplay_image_quality",
            "trickplay_concurrency",
        ],
    ),
    (
        Group::Images,
        &[
            "image_decode_max_megapixels",
            "image_decode_max_memory_mb",
            "image_resize_budget_mb",
            "ffmpeg_img_extract_perf_tradeoff",
            "image_download_max_mb",
            "image_negative_cache_ttl_secs",
            "image_private_http_allow",
        ],
    ),
    (
        Group::Providers,
        &[
            "omdb_api_key",
            "tvdb_api_key",
            "tvdb_subscriber_pin",
            "fanart_personal_api_key",
            "tvdb_cache_hours",
            "tvdb_token_days",
            "musicbrainz_base_url",
            "studios_repo_url",
            "provider_timeout_seconds",
            "provider_max_wait_seconds",
            "subtitle_private_http_allow",
        ],
    ),
    (
        Group::Performance,
        &[
            "db_pool",
            "db_statement_cache",
            "item_values_write_chunk",
            "auth_cache_ttl_secs",
            "row_cache_ttl_secs",
            "max_transcodes",
            "max_transcodes_per_user",
            "ffmpeg_analyze_duration",
            "ffmpeg_probe_size",
            "playback_metrics_queue",
            "session_idle_hours",
        ],
    ),
    (
        Group::Logging,
        &[
            "log_level",
            "log_format",
            "enable_metrics",
            "metrics_sample_interval",
            "metrics_scan_duration_buckets",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_TRACES_SAMPLER_ARG",
        ],
    ),
    (
        Group::Plugins,
        &[
            "wasm_call_timeout_secs",
            "wasm_call_grace_secs",
            "wasm_memory_limit_mb",
            "wasm_event_queue_capacity",
            "wasm_private_http_allow",
            "max_plugin_download_mb",
            "wasm_analysis_concurrency",
            "wasm_state_limit_mb",
            "wasm_image_download_mb",
            "wasm_image_timeout_secs",
            "wasm_write_content_mb",
            "wasm_subtitle_extract_mb",
        ],
    ),
    (
        Group::System,
        &[
            "data_dir",
            "config_dir",
            "cache_dir",
            "web_dir",
            "ffmpeg_path",
            "ffprobe_path",
            "library_roots",
            "admin_user",
            "admin_password",
            "shutdown_timeout_secs",
            "disable_extensions",
        ],
    ),
];

/// A tab's place in the page's order. Exhaustive, so a new tab must be
/// placed here and in [`TABS`].
fn tab_position(group: Group) -> usize {
    match group {
        Group::Network => 0,
        Group::Library => 1,
        Group::Trickplay => 2,
        Group::Images => 3,
        Group::Providers => 4,
        Group::Performance => 5,
        Group::Logging => 6,
        Group::Plugins => 7,
        Group::System => 8,
    }
}

/// The table lists exactly [`TABS`], each tab's rows together and the tabs in
/// the page's order: the page opens its tabs in the order the rows come.
#[test]
fn every_row_is_on_its_tab_and_the_tabs_come_in_order() {
    let rows: Vec<(Group, &str)> = SETTINGS.iter().map(|spec| (spec.group, spec.key)).collect();
    let pinned: Vec<(Group, &str)> = TABS
        .iter()
        .flat_map(|(group, keys)| keys.iter().map(move |key| (*group, *key)))
        .collect();
    assert_eq!(rows, pinned);
    let tabs: Vec<usize> = TABS.iter().map(|(group, _)| tab_position(*group)).collect();
    assert_eq!(tabs, (0..9).collect::<Vec<_>>(), "nine tabs, in order");
}

/// Every row has a readable name of its own, which is not its key.
#[test]
fn every_row_has_a_label_of_its_own() {
    let mut labels = BTreeSet::new();
    for spec in SETTINGS {
        assert!(!spec.label.trim().is_empty(), "{}: no label", spec.key);
        assert_ne!(spec.label, spec.key, "{}: the label is the key", spec.key);
        assert!(
            labels.insert(spec.label),
            "{}: the label `{}` is another row's",
            spec.key,
            spec.label
        );
    }
}

/// A row that shows its unit beside the value does not state it again in its
/// description.
#[test]
fn a_rows_description_leaves_its_unit_to_the_unit() {
    for spec in SETTINGS {
        let stated = match spec.unit {
            Some(Unit::Mebibytes) => "mib",
            Some(Unit::Seconds) => "second",
            Some(Unit::Minutes) => "minute",
            Some(Unit::Hours) => "hour",
            Some(Unit::Days) => "day",
            Some(Unit::Megapixels) => "megapixel",
            // What is counted is the description's subject.
            Some(Unit::Items | Unit::Events) | None => continue,
        };
        // The unit as a word, singular or plural: not "today" for "day".
        let description = spec.description.to_lowercase();
        let states_it = description
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word.strip_suffix('s').unwrap_or(word) == stated);
        assert!(!states_it, "{}: {}", spec.key, spec.description);
    }
}

/// The report carries each row's label and unit.
#[test]
fn the_report_carries_each_rows_label_and_unit() {
    let env = FakeEnv::new();
    let report = report(&load(Cli::default(), &env), &env);
    for spec in SETTINGS {
        let setting = entry(&report, spec.key);
        assert_eq!(setting.label, spec.label, "{}", spec.key);
        assert_eq!(setting.unit, spec.unit, "{}", spec.key);
        assert_eq!(setting.group, spec.group, "{}", spec.key);
    }
    let subtitles = entry(&report, "wasm_subtitle_extract_mb");
    assert_eq!(subtitles.label, "Subtitle extraction limit");
    assert_eq!(subtitles.unit, Some(Unit::Mebibytes));
    assert_eq!(entry(&report, "port").unit, None);
}

/// Every numeric row has a range of its own, not the bounds of the number
/// type the loader reads it as; [`sample`] lies inside it and is not the
/// default; and no description says `0` means the default, since the page
/// refuses `0` where the loader reads it so (Reset restores a default).
#[test]
fn every_numeric_row_has_a_range_of_its_own() {
    for spec in SETTINGS {
        assert!(
            !spec.description.contains("0 means the default"),
            "{}: {}",
            spec.key,
            spec.description
        );
        let SettingKind::Integer { min, max } = spec.kind else {
            continue;
        };
        assert!(
            max < i64::from(u32::MAX),
            "{}: {min} to {max} is its number type's",
            spec.key
        );
        let sample: i64 = sample(spec).parse().expect("a number");
        assert!(
            (min..=max).contains(&sample),
            "{}: the sample {sample} is outside {min} to {max}",
            spec.key
        );
        assert_ne!(
            sample.to_string(),
            spec.default,
            "{}: the sample is the default",
            spec.key
        );
    }
}

/// The page and the loader share each range: a value past either end in
/// `config.toml` boots and runs inside the row's range, so the page never
/// shows a value in use it would refuse to save.
#[test]
fn the_value_in_use_stays_inside_each_rows_range() {
    // Out of range, these stop the server: the ports do not fit their number
    // type, and the database's limits and the login backoff's security
    // settings are checked strictly.
    let refused_at_boot = [
        "port",
        "https_port",
        "db_statement_cache",
        "auth_cache_ttl_secs",
        "row_cache_ttl_secs",
        "login_backoff_attempts",
        "login_backoff_max_delay_secs",
        "login_backoff_window_mins",
        "login_backoff_max_tracked",
    ];
    let in_use = |key: &str, value: i64| {
        let line = format!("{key} = {value}");
        let data = DataDir::new(std::slice::from_ref(&line));
        let config = Config::load_from(data.cli(), &FakeEnv::new())
            .unwrap_or_else(|err| panic!("{line} stops the server: {err:#}"));
        (spec(key).running)(&config)
            .as_i64()
            .unwrap_or_else(|| panic!("{line}: no number in use"))
    };
    for spec in file_rows() {
        let SettingKind::Integer { min, max } = spec.kind else {
            continue;
        };
        // Out of range, TheTVDB's token age is the default where the client
        // is built; the row shows the value as written.
        if refused_at_boot.contains(&spec.key) || spec.key == "tvdb_token_days" {
            continue;
        }
        let value = in_use(spec.key, max + 1);
        assert!(value <= max, "{} = {}: {value} in use", spec.key, max + 1);
        // `0` below a range that starts at 1 is the default on some rows,
        // which `config::tests` covers.
        if min > 1 {
            let value = in_use(spec.key, min - 1);
            assert!(value >= min, "{} = {}: {value} in use", spec.key, min - 1);
        }
    }
}

/// (c) Every key round-trips: `key = <sample>` in `config.toml` loads
/// without a warning and changes the value in use.
#[test]
fn every_key_round_trips_through_config_toml() {
    let env = FakeEnv::new();
    let defaults = load(Cli::default(), &env);
    for spec in file_rows() {
        let line = format!("{} = {}", spec.key, sample(spec));
        let data = DataDir::new(std::slice::from_ref(&line));
        // Named, so the file may set `data_dir` too; no flag outranks it.
        let named = Cli {
            config_file: Some(data.config_file()),
            ..Cli::default()
        };
        let loaded = Config::load_from(named, &env).unwrap_or_else(|e| panic!("{line}: {e:#}"));
        assert!(
            loaded.load_warnings.is_empty(),
            "{line}: {:?}",
            loaded.load_warnings
        );
        assert_ne!(
            (spec.running)(&loaded),
            (spec.running)(&defaults),
            "{line} changed nothing"
        );
        // A key in the file is the file's, until a flag or variable sets it.
        assert_eq!(
            entry(&report(&loaded, &env), spec.key).source,
            Source::ConfigFile,
            "{line}"
        );
    }
}

/// The source a row reports is the layer the loader applied: a variable is
/// the source exactly when the value in use is not the file's, for a valid,
/// a blank and an unparsable value. This is what [`EnvRead`] encodes.
#[test]
fn the_source_is_the_layer_the_loader_applied() {
    for spec in file_rows() {
        let var = spec.env.expect("a variable");
        let line = format!("{} = {}", spec.key, sample(spec));
        let data = DataDir::new(std::slice::from_ref(&line));
        let file_keys = vec![spec.key.to_owned()];
        let base = FakeEnv::new().with(
            "FERROFIN_CONFIG_FILE",
            &data.config_file().to_string_lossy(),
        );
        let Ok(file_only) = Config::load_from(Cli::default(), &base) else {
            panic!("{line} does not load");
        };
        for raw in ["7", "", " ", "x?"] {
            let env = base.clone().with(var, raw);
            let Ok(loaded) = Config::load_from(Cli::default(), &env) else {
                // The loader refuses this value: no server runs with it.
                continue;
            };
            let applied = (spec.running)(&loaded) != (spec.running)(&file_only);
            let expected = if applied {
                Source::Environment
            } else {
                Source::ConfigFile
            };
            assert_eq!(
                source(spec, &Cli::default(), &env, &file_keys),
                expected,
                "{var}={raw:?} over {line}"
            );
        }
    }
}

/// A key in a `config.toml` found in the data directory is the source
/// exactly when the loader applies it: every key but `data_dir`, which such a
/// file cannot move.
#[test]
fn a_key_in_a_found_config_toml_is_the_source_when_the_loader_applies_it() {
    for spec in file_rows() {
        let xdg = tempfile::tempdir().expect("temp dir");
        let env = FakeEnv::new().with("XDG_DATA_HOME", &xdg.path().to_string_lossy());
        let without = load(Cli::default(), &env);
        let line = format!("{} = {}", spec.key, sample(spec));
        std::fs::create_dir_all(&without.data_dir).expect("data dir");
        std::fs::write(&without.config_file, &line).expect("write");
        let with = load(Cli::default(), &env);
        assert!(with.config_file_found, "{line}");
        let applied = (spec.running)(&with) != (spec.running)(&without);
        let expected = if applied {
            Source::ConfigFile
        } else {
            Source::Default
        };
        assert_eq!(
            source(spec, &Cli::default(), &env, &[spec.key.to_owned()]),
            expected,
            "{line} in {}",
            with.config_file.display()
        );
    }
    assert_eq!(
        source(
            spec("data_dir"),
            &Cli::default(),
            &FakeEnv::new(),
            &["data_dir".to_owned()]
        ),
        Source::Default,
        "a found file's data_dir is ignored"
    );
}

/// Every flag but `--config` sets a row's key, the row names it, and a flag
/// on the command line is reported as the source.
#[test]
fn every_flag_is_a_rows_and_is_reported_as_its_source() {
    let command = Cli::command();
    let mut flagged = BTreeSet::new();
    for arg in command.get_arguments() {
        let (id, Some(long)) = (arg.get_id().as_str(), arg.get_long()) else {
            continue;
        };
        if id == "config_file" {
            continue;
        }
        let flag = format!("--{long}");
        assert_eq!(spec(id).flag, Some(flag.as_str()), "{id}");
        let cli = Cli::try_parse_from(["ferrofin-server", flag.as_str(), "1"]).expect("parse");
        assert!(flag_set(&cli, id), "{flag} is not seen as set");
        assert_eq!(source(spec(id), &cli, &FakeEnv::new(), &[]), Source::Flag);
        flagged.insert(id);
    }
    let rows: BTreeSet<&str> = SETTINGS
        .iter()
        .filter(|spec| spec.flag.is_some())
        .map(|spec| spec.key)
        .collect();
    assert_eq!(rows, flagged);
}

/// The rows that apply only on a process restart are what an in-process
/// restart keeps from the running configuration.
#[test]
fn process_restart_rows_are_what_an_in_process_restart_keeps() {
    let env = FakeEnv::new();
    let empty = DataDir::new(&[]);
    let running = load(empty.cli(), &env);
    let lines: Vec<String> = file_rows()
        .map(|spec| format!("{} = {}", spec.key, sample(spec)))
        .collect();
    let data = DataDir::new(&lines);
    let next = load(
        Cli {
            config_file: Some(data.config_file()),
            ..Cli::default()
        },
        &env,
    );
    let (_, kept) = running.for_restart(next);
    let kept: BTreeSet<&str> = kept.into_iter().collect();
    let rows: BTreeSet<&str> = file_rows()
        .filter(|spec| spec.applies == Applies::ProcessRestart)
        .map(|spec| spec.key)
        .collect();
    assert_eq!(kept, rows);
}

// ---- the report ---------------------------------------------------------------

#[test]
fn each_source_is_reported_and_only_file_and_default_values_are_editable() {
    let data = DataDir::new(&["scan_probe_concurrency = 4".to_owned()]);
    let env = FakeEnv::new().with("FERROFIN_DB_POOL", "4");
    let cli = Cli {
        port: Some(8097),
        ..data.cli()
    };
    let report = report(&load(cli, &env), &env);

    let file = entry(&report, "scan_probe_concurrency");
    assert_eq!(file.source, Source::ConfigFile);
    assert_eq!(file.value, Some(json!(4)));
    assert!(file.editable && file.locked_reason.is_none(), "{file:?}");

    let variable = entry(&report, "db_pool");
    assert_eq!(variable.source, Source::Environment);
    assert_eq!(variable.value, Some(json!(4)));
    assert!(!variable.editable);
    let reason = variable.locked_reason.as_deref().expect("a reason");
    assert!(
        reason.contains("`FERROFIN_DB_POOL` environment variable"),
        "{reason}"
    );
    assert!(reason.contains("restart the server process"), "{reason}");

    let flag = entry(&report, "port");
    assert_eq!(flag.source, Source::Flag);
    assert_eq!(flag.value, Some(json!(8097)));
    assert_eq!(flag.flag.as_deref(), Some("--port"));

    let default = entry(&report, "trickplay_concurrency");
    assert_eq!(default.source, Source::Default);
    assert_eq!(default.value, None, "auto is unset");
    assert_eq!(default.default.as_deref(), Some("auto"));
    assert!(default.editable);

    assert_eq!(report.config_file, data.config_file().display().to_string());
    assert!(report.config_file_writable);
    assert!(!report.restart_pending);
    assert_eq!(report.settings.len(), SETTINGS.len());
}

/// gl-554: the Networking page's ports outrank `config.toml`, and a flag or a
/// variable outranks them; each row reports the port the server listens on.
#[test]
fn the_networking_page_ports_outrank_the_file_but_not_a_flag_or_variable() {
    let data = DataDir::new(&["port = 5000\nhttps_port = 5443".to_owned()]);
    let page = NetworkConfiguration {
        internal_http_port: 8097,
        internal_https_port: 8921,
        ..NetworkConfiguration::default()
    };
    let rows = |cli: Cli, env: &FakeEnv, network: &NetworkConfiguration| {
        let running = load(cli, env);
        let report = snapshot(
            SETTINGS,
            &running,
            env,
            &ServerConfiguration::default(),
            network,
            false,
        )
        .expect("report");
        let (http, https) = crate::listen::ports(&running, network);
        let rows = (
            entry(&report, "port").clone(),
            entry(&report, "https_port").clone(),
        );
        assert_eq!(rows.0.value, Some(json!(http)), "{rows:?}");
        assert_eq!(rows.1.value, Some(json!(https)), "{rows:?}");
        rows
    };

    let (http, https) = rows(data.cli(), &FakeEnv::new(), &page);
    assert_eq!(
        (http.source, http.value, http.saved_value),
        (
            Source::NetworkConfiguration,
            Some(json!(8097)),
            Some(json!(5000))
        )
    );
    assert_eq!(
        (https.source, https.value),
        (Source::NetworkConfiguration, Some(json!(8921)))
    );

    let (http, _) = rows(
        data.cli(),
        &FakeEnv::new(),
        &NetworkConfiguration::default(),
    );
    assert_eq!(
        (http.source, http.value),
        (Source::ConfigFile, Some(json!(5000)))
    );

    let flag = Cli {
        port: Some(7000),
        ..data.cli()
    };
    let (http, _) = rows(flag, &FakeEnv::new(), &page);
    assert_eq!((http.source, http.value), (Source::Flag, Some(json!(7000))));
    let variable = FakeEnv::new().with("FERROFIN_HTTPS_PORT", "7443");
    let (_, https) = rows(data.cli(), &variable, &page);
    assert_eq!(
        (https.source, https.value),
        (Source::Environment, Some(json!(7443)))
    );
}

/// gl-555: the Networking page's base URL outranks `config.toml`, and the
/// variable outranks it; the row reports the base URL the server serves under.
#[test]
fn the_networking_page_base_url_outranks_the_file_but_not_the_variable() {
    let data = DataDir::new(&["base_url = \"/file\"".to_owned()]);
    let page = NetworkConfiguration {
        base_url: "/page/".to_owned(),
        ..NetworkConfiguration::default()
    };
    let row = |env: &FakeEnv, network: &NetworkConfiguration| {
        let running = load(data.cli(), env);
        let report = snapshot(
            SETTINGS,
            &running,
            env,
            &ServerConfiguration::default(),
            network,
            false,
        )
        .expect("report");
        let row = entry(&report, "base_url").clone();
        let served =
            crate::base_url::resolve(&running.base_url, running.base_url_from_env, network);
        assert_eq!(row.value, Some(json!(served)), "{row:?}");
        row
    };

    let from_page = row(&FakeEnv::new(), &page);
    assert_eq!(from_page.source, Source::NetworkConfiguration);
    assert_eq!(from_page.value, Some(json!("/page")));
    assert_eq!(from_page.saved_value, Some(json!("/file")), "not in use");
    assert!(!from_page.editable);

    let from_file = row(&FakeEnv::new(), &NetworkConfiguration::default());
    assert_eq!(from_file.source, Source::ConfigFile);
    assert_eq!(from_file.value, Some(json!("/file")));

    let from_variable = row(&FakeEnv::new().with("FERROFIN_BASE_URL", "/env"), &page);
    assert_eq!(from_variable.source, Source::Environment);
    assert_eq!(from_variable.value, Some(json!("/env")));

    // The common install: no base_url in config.toml at all.
    let no_file = DataDir::new(&[]);
    let running = load(no_file.cli(), &FakeEnv::new());
    let report = snapshot(
        SETTINGS,
        &running,
        &FakeEnv::new(),
        &ServerConfiguration::default(),
        &page,
        false,
    )
    .expect("report");
    let page_alone = entry(&report, "base_url");
    assert_eq!(page_alone.source, Source::NetworkConfiguration);
    let served = crate::base_url::resolve(&running.base_url, running.base_url_from_env, &page);
    assert_eq!(page_alone.value, Some(json!(served)));
    assert_eq!(served, "/page");
    assert_eq!(page_alone.saved_value, None);
}

#[test]
fn a_flag_locks_an_editable_setting_and_names_itself() {
    let row = SettingSpec {
        key: "port",
        flag: Some("--port"),
        ..*spec("scan_probe_concurrency")
    };
    let reason = locked_reason(&row, Source::Flag, &FakeEnv::new()).expect("locked");
    assert_eq!(
        reason,
        "Set by the `--port` command-line flag. Change the command that starts the server and \
         restart it."
    );
}

#[test]
fn a_blank_variable_still_sets_a_text_setting_but_not_a_number() {
    let data = DataDir::new(&[
        "omdb_api_key = \"from-the-file\"".to_owned(),
        "scan_probe_concurrency = 4".to_owned(),
    ]);
    let env = FakeEnv::new()
        .with("FERROFIN_OMDB_KEY", "")
        .with("FERROFIN_SCAN_PROBE_CONCURRENCY", " ");
    let report = report(&load(data.cli(), &env), &env);

    // The loader takes the blank key, so a value saved to the file would not
    // apply: locked, and the file's value shown as saved.
    let key = entry(&report, "omdb_api_key");
    assert_eq!(key.source, Source::Environment);
    assert!(!key.editable);
    assert_eq!(key.value, Some(json!("")));
    assert_eq!(key.saved_value, Some(json!("from-the-file")));

    let count = entry(&report, "scan_probe_concurrency");
    assert_eq!(count.source, Source::ConfigFile);
    assert!(count.editable);
}

#[test]
fn read_only_settings_carry_their_note_whatever_sets_them() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new().with("FERROFIN_FFMPEG_PATH", "/opt/ffmpeg");
    let report = report(&load(data.cli(), &env), &env);
    let notes = [
        ("data_dir", SET_AT_STARTUP),
        ("config_dir", SET_AT_STARTUP),
        ("cache_dir", SET_AT_STARTUP),
        ("web_dir", SET_AT_STARTUP),
        ("ffmpeg_path", NO_EXECUTABLE_FROM_THE_WEB),
        ("ffprobe_path", NO_EXECUTABLE_FROM_THE_WEB),
        ("admin_user", FIRST_BOOT_ONLY),
        ("admin_password", FIRST_BOOT_ONLY),
        ("library_roots", FIRST_BOOT_ONLY),
        ("disable_extensions", "A measurement switch"),
        (
            "wasm_private_http_allow",
            "Lets the named WASM plugins reach",
        ),
        (
            "image_private_http_allow",
            "Lets image downloads reach the LAN",
        ),
        (
            "subtitle_private_http_allow",
            "Lets OpenSubtitles download links reach the LAN",
        ),
        // Owner decision D2: jellyfin-web's own pages cover these.
        (
            "bind_addr",
            "Dashboard → Networking → Local network addresses",
        ),
        ("server_name", "Dashboard → General → Server name"),
        ("enable_metrics", "Dashboard → General → Enable metrics"),
        (
            "published_url",
            "Dashboard → Networking → Published server URIs",
        ),
        ("base_url", "Dashboard → Networking → Base URL sets it"),
        (
            "port",
            "Dashboard → Networking → Local HTTP port number sets it",
        ),
        (
            "https_port",
            "Dashboard → Networking → Local HTTPS port number sets it",
        ),
    ];
    for (key, note) in notes {
        let setting = entry(&report, key);
        assert!(!setting.editable, "{key} is editable");
        let reason = setting.locked_reason.as_deref().unwrap_or_default();
        assert!(reason.starts_with(note), "{key}: {reason}");
    }
    let read_only: BTreeSet<&str> = SETTINGS
        .iter()
        .filter(|spec| matches!(spec.access, SettingAccess::ReadOnly(_)))
        .map(|spec| spec.key)
        .collect();
    assert_eq!(read_only, notes.iter().map(|(key, _)| *key).collect());

    // A read-only setting a variable sets says both.
    let ffmpeg = entry(&report, "ffmpeg_path");
    assert_eq!(ffmpeg.source, Source::Environment);
    let reason = ffmpeg.locked_reason.as_deref().expect("a reason");
    assert!(
        reason.contains("`FERROFIN_FFMPEG_PATH` environment variable"),
        "{reason}"
    );
}

#[test]
fn the_opentelemetry_variables_are_reported_from_the_environment_and_locked() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new().with("OTEL_EXPORTER_OTLP_ENDPOINT", "http://tempo:4317");
    let report = report(&load(data.cli(), &env), &env);

    let endpoint = entry(&report, "OTEL_EXPORTER_OTLP_ENDPOINT");
    assert_eq!(endpoint.source, Source::Environment);
    assert_eq!(endpoint.value, Some(json!("http://tempo:4317")));
    assert_eq!(endpoint.locked_reason.as_deref(), Some(ENV_ONLY));
    assert_eq!(endpoint.applies, Applies::ProcessRestart);

    let ratio = entry(&report, "OTEL_TRACES_SAMPLER_ARG");
    assert_eq!(ratio.source, Source::Default);
    assert_eq!(ratio.value, None);
    assert!(!ratio.editable);

    // As the trace export reads them: a blank endpoint exports nothing, and a
    // ratio that is not a number is the default.
    let env = FakeEnv::new()
        .with("OTEL_EXPORTER_OTLP_ENDPOINT", " ")
        .with("OTEL_TRACES_SAMPLER_ARG", "half");
    let unset = self::report(&load(data.cli(), &env), &env);
    for key in ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_TRACES_SAMPLER_ARG"] {
        let setting = entry(&unset, key);
        assert_eq!(setting.source, Source::Default, "{key}");
        assert_eq!(setting.value, None, "{key}");
    }
    let env = FakeEnv::new().with("OTEL_TRACES_SAMPLER_ARG", " 2 ");
    let clamped = self::report(&load(data.cli(), &env), &env);
    assert_eq!(
        entry(&clamped, "OTEL_TRACES_SAMPLER_ARG").source,
        Source::Environment
    );
}

#[test]
fn rust_log_outranks_the_log_level() {
    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let env = FakeEnv::new().with(RUST_LOG, "warn,ferrofin=trace");
    let outranked = report(&load(data.cli(), &env), &env);
    let level = entry(&outranked, "log_level");
    assert_eq!(level.source, Source::Environment);
    assert_eq!(level.value, Some(json!("warn,ferrofin=trace")));
    assert_eq!(level.saved_value, Some(json!("debug")));
    let reason = level.locked_reason.as_deref().expect("locked");
    assert!(
        reason.contains("`RUST_LOG` environment variable, which outranks"),
        "{reason}"
    );

    // Without it, the file's level is the one in use, and editable.
    let env = env.without(RUST_LOG);
    let own = report(&load(data.cli(), &env), &env);
    let level = entry(&own, "log_level");
    assert_eq!(level.source, Source::ConfigFile);
    assert!(level.editable);
}

#[test]
fn a_file_changed_since_the_start_shows_its_saved_value() {
    let data = DataDir::new(&["scan_probe_concurrency = 4".to_owned()]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    std::fs::write(
        data.config_file(),
        "scan_probe_concurrency = 6\nwasm_memory_limit_mb = 64\n",
    )
    .expect("edit");
    let report = report(&running, &env);

    let edited = entry(&report, "scan_probe_concurrency");
    assert_eq!(edited.value, Some(json!(4)), "the running value");
    assert_eq!(edited.saved_value, Some(json!(6)));
    // Added since the start: the file's, not in use yet.
    let added = entry(&report, "wasm_memory_limit_mb");
    assert_eq!(added.source, Source::ConfigFile);
    assert_eq!(added.value, None);
    assert_eq!(added.saved_value, Some(json!(64)));
    // Unchanged or absent: nothing saved apart.
    assert_eq!(entry(&report, "db_pool").saved_value, None);
}

#[test]
fn auto_saved_over_a_running_count_shows_as_the_saved_value() {
    let data = DataDir::new(&[
        "db_pool = 8".to_owned(),
        "trickplay_concurrency = \"auto\"".to_owned(),
    ]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    std::fs::write(
        data.config_file(),
        "db_pool = \"auto\"\ntrickplay_concurrency = \"auto\"\n",
    )
    .expect("edit");
    let shown = report(&running, &env);

    // `auto` is no running value, but it is what the file holds.
    let pool = entry(&shown, "db_pool");
    assert_eq!(pool.value, Some(json!(8)), "the running value");
    assert_eq!(pool.saved_value, Some(json!("auto")));
    // Auto in the file and in use: nothing saved apart.
    let trickplay = entry(&shown, "trickplay_concurrency");
    assert_eq!(trickplay.value, None);
    assert_eq!(trickplay.saved_value, None);

    // A variable's count outranks it: the file's `auto` is saved underneath.
    let env = FakeEnv::new().with("FERROFIN_DB_POOL", "16");
    let outranked = report(&load(data.cli(), &env), &env);
    let pool = entry(&outranked, "db_pool");
    assert_eq!(pool.source, Source::Environment);
    assert_eq!(pool.value, Some(json!(16)));
    assert_eq!(pool.saved_value, Some(json!("auto")));
}

/// The page is driven by the report, so a new setting needs no page change. A
/// key the script named would be a quoted string; a bare word may be English
/// (`port` is in "report").
#[test]
fn the_settings_page_names_no_setting() {
    use ferrofin_extensions::Extension as _;

    let pages = ferrofin_extensions::ferrofin_settings::FerrofinSettingsExtension.config_pages();
    let html = String::from_utf8_lossy(&pages[0].bytes);
    for spec in SETTINGS {
        for quote in ['\'', '"', '`'] {
            let quoted = format!("{quote}{}{quote}", spec.key);
            assert!(!html.contains(&quoted), "the page names {quoted}");
        }
    }
}

#[test]
fn secrets_are_reported_in_plain_text() {
    let data = DataDir::new(&[
        "omdb_api_key = \"omdb-secret\"".to_owned(),
        "admin_password = \"seed-secret\"".to_owned(),
    ]);
    let env = FakeEnv::new().with("FERROFIN_TVDB_PIN", "pin-secret");
    let report = report(&load(data.cli(), &env), &env);
    for (key, secret) in [
        ("omdb_api_key", "omdb-secret"),
        ("admin_password", "seed-secret"),
        ("tvdb_subscriber_pin", "pin-secret"),
    ] {
        let setting = entry(&report, key);
        assert_eq!(setting.kind, ServerSettingKind::Secret, "{key}");
        assert_eq!(setting.value, Some(json!(secret)), "{key}");
        assert!(
            !format!("{setting:?}").contains(secret),
            "{key} leaks in Debug"
        );
    }
}

#[test]
fn a_config_file_that_no_longer_loads_is_a_conflict_naming_it() {
    let data = DataDir::new(&["scan_probe_concurrency = 4".to_owned()]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    std::fs::write(data.config_file(), "db_pool = 0\n").expect("edit");
    let err = snapshot(
        SETTINGS,
        &running,
        &env,
        &ServerConfiguration::default(),
        &NetworkConfiguration::default(),
        false,
    )
    .expect_err("refused");
    let ServiceError::Conflict(message) = err else {
        panic!("{err:?}");
    };
    assert!(
        message.contains(&data.config_file().display().to_string()),
        "{message}"
    );
    assert!(message.contains("db_pool"), "{message}");
}

/// The 409 never quotes a secret: not from a line that does not parse, nor
/// from a key of the wrong type.
#[test]
fn a_config_file_that_no_longer_loads_quotes_no_secret() {
    for (contents, secret) in [
        (
            "omdb_api_key = \"unterminated-secret\n",
            "unterminated-secret",
        ),
        ("tvdb_api_key = [\"listed-secret\"]\n", "listed-secret"),
        (
            "admin_password = { nested = \"tabled-secret\" }\n",
            "tabled-secret",
        ),
    ] {
        let data = DataDir::new(&["scan_probe_concurrency = 4".to_owned()]);
        let env = FakeEnv::new();
        let running = load(data.cli(), &env);
        std::fs::write(data.config_file(), contents).expect("edit");
        let err = snapshot(
            SETTINGS,
            &running,
            &env,
            &ServerConfiguration::default(),
            &NetworkConfiguration::default(),
            false,
        )
        .expect_err("refused");
        let ServiceError::Conflict(message) = err else {
            panic!("{contents}: {err:?}");
        };
        assert!(!message.contains(secret), "{contents}: {message}");
    }
}

#[test]
fn a_config_file_whose_directory_takes_no_file_is_not_writable() {
    let data = DataDir::new(&[]);
    assert!(directory_accepts_a_file(&data.config_file()));
    assert!(!directory_accepts_a_file(
        &data.path().join("missing").join("config.toml")
    ));
    let left: Vec<_> = std::fs::read_dir(data.path()).expect("list").collect();
    assert!(left.is_empty(), "the probe stays behind: {left:?}");
}

// ---- the manager ---------------------------------------------------------------

/// The Ferrofin plugin, registered as the server registers it.
pub(super) fn plugins(dir: &Path) -> Arc<dyn PluginManager> {
    Arc::new(ferrofin_core::FerrofinPluginManager::new(
        ferrofin_extensions::registered_plugins(&ferrofin_extensions::registered_extensions(true)),
        dir.to_path_buf(),
    ))
}

#[tokio::test]
async fn the_manager_reports_the_restart_flag_and_answers_not_found_while_disabled() {
    let data = DataDir::new(&[]);
    let (shutdown, _rx) = tokio::sync::oneshot::channel();
    let lifecycle = Arc::new(crate::state::FerrofinLifecycleController::new(shutdown));
    let plugins = plugins(data.path());
    let env = FakeEnv::new();
    let manager = FerrofinServerSettings::new(
        load(data.cli(), &env),
        lifecycle.clone(),
        Arc::clone(&plugins),
        FakeConfiguration::new(ServerConfiguration::default()),
    )
    .with_env(env);

    let report = manager.snapshot().await.expect("enabled by default");
    assert!(!report.restart_pending);
    lifecycle.mark_restart_required();
    assert!(manager.snapshot().await.expect("report").restart_pending);

    plugins
        .disable_plugin(ferrofin_extensions::ferrofin_settings::EXTENSION_ID)
        .await
        .expect("disable");
    let result = manager.snapshot().await;
    assert!(
        matches!(result, Err(ServiceError::NotFound(_))),
        "{result:?}"
    );
}

/// The statement cache row shows the default the database applies when the
/// setting is unset.
#[test]
fn the_statement_cache_row_shows_the_database_default() {
    assert_eq!(
        spec("db_statement_cache").default,
        ferrofin_db::DEFAULT_STATEMENT_CACHE_CAPACITY.to_string()
    );
}

/// The item-values write chunk row shows the library manager's default.
#[test]
fn the_item_values_write_chunk_row_shows_the_library_default() {
    assert_eq!(
        spec("item_values_write_chunk").default,
        ferrofin_core::library_manager::DEFAULT_ITEM_VALUES_WRITE_CHUNK.to_string()
    );
}

/// The ffmpeg input-analysis rows show Jellyfin's defaults as the encoder
/// prints them, and their notes the encoder's ranges (gl-734).
#[test]
fn the_ffmpeg_input_analysis_rows_show_the_encoders_defaults_and_ranges() {
    use ferrofin_mediaencoding::encoder::media_encoder::{
        ANALYZE_DURATION_RANGE, DEFAULT_ANALYZE_DURATION, DEFAULT_PROBE_SIZE, PROBE_SIZE_RANGE,
    };
    for (key, default, range) in [
        (
            "ffmpeg_analyze_duration",
            DEFAULT_ANALYZE_DURATION,
            ANALYZE_DURATION_RANGE,
        ),
        ("ffmpeg_probe_size", DEFAULT_PROBE_SIZE, PROBE_SIZE_RANGE),
    ] {
        let row = spec(key);
        assert_eq!(row.default, default.to_string(), "{key}");
        let range = format!("{} to {}", range.start(), range.end());
        assert!(row.note.contains(&range), "{key}: {range} in {}", row.note);
    }
}

// ---- choices, notes and the server configuration (gl-1174) -----------------

/// B10: `log_format` is a choice between JSON and Text, each with its note,
/// reported by the value of the option in use.
#[test]
fn log_format_is_a_choice_with_a_note_per_option() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    let report = report(&load(data.cli(), &env), &env);
    let row = entry(&report, "log_format");

    assert_eq!(row.kind, ServerSettingKind::Choice);
    assert_eq!(row.store, ServerSettingStore::ConfigFile);
    assert_eq!(row.value, Some(json!("json")));
    assert!(!row.custom);
    let options: Vec<(&str, &str, Option<&str>)> = row
        .options
        .iter()
        .map(|option| {
            (
                option.value.as_str(),
                option.label.as_str(),
                option.note.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        options,
        [
            (
                "json",
                "JSON",
                Some("One JSON object per line, for log collectors")
            ),
            ("text", "Text", Some("Plain lines, for reading by hand")),
        ]
    );
}

/// B10: a `config.toml` value that fits no option is reported as written, as
/// Custom, with no saved value beside it; one that fits in another case or
/// with spaces is the option's. A variable that fits none is not the file's
/// to keep: it locks the row.
#[test]
fn a_choice_in_config_toml_that_fits_no_option_is_custom() {
    for (written, value, custom) in [
        ("xml", "xml", true),
        (" TEXT ", "text", false),
        ("json", "json", false),
    ] {
        let data = DataDir::new(&[format!("log_format = \"{written}\"")]);
        let env = FakeEnv::new();
        let report = report(&load(data.cli(), &env), &env);
        let row = entry(&report, "log_format");
        assert_eq!(row.value, Some(json!(value)), "{written}");
        assert_eq!(row.custom, custom, "{written}");
        assert_eq!(row.saved_value, None, "{written}");
        assert!(row.editable, "{written}");
    }

    let data = DataDir::new(&[]);
    let env = FakeEnv::new().with("FERROFIN_LOG_FORMAT", "xml");
    let report = report(&load(data.cli(), &env), &env);
    let row = entry(&report, "log_format");
    assert!(!row.custom);
    assert!(!row.editable);
}

/// Every choice offers its default among named options of its own, every
/// other row has none, and every row stored in the server configuration
/// applies at once, is set by nothing else, and has a new server's value as
/// its default.
#[test]
fn every_choice_offers_its_default_and_every_stored_row_applies_at_once() {
    for spec in SETTINGS {
        if spec.kind == SettingKind::Choice {
            let values: BTreeSet<&str> = spec.options.iter().map(|option| option.value).collect();
            assert!(spec.options.len() >= 2, "{}: no choice", spec.key);
            assert_eq!(
                values.len(),
                spec.options.len(),
                "{}: an option twice",
                spec.key
            );
            assert!(
                values.contains(spec.default),
                "{}: the default is no option",
                spec.key
            );
            assert!(
                spec.options.iter().all(|option| !option.label.is_empty()),
                "{}: an option with no name",
                spec.key
            );
        } else {
            assert!(
                spec.options.is_empty(),
                "{}: options on a {:?}",
                spec.key,
                spec.kind
            );
        }
        if let SettingStore::ServerConfiguration(field) = spec.store {
            assert_eq!(spec.applies, Applies::Immediately, "{}", spec.key);
            assert!(spec.env.is_none() && spec.flag.is_none(), "{}", spec.key);
            assert_eq!(spec.access, SettingAccess::Editable, "{}", spec.key);
            assert_eq!(
                (field.read)(&ServerConfiguration::default()),
                json!(spec.default),
                "{}: the default is not a new server's value",
                spec.key
            );
        } else {
            assert_ne!(spec.applies, Applies::Immediately, "{}", spec.key);
        }
    }
}

/// The report's row `key` on a new server's configuration with `trickplay`
/// as its `TrickplayOptions`.
fn stored_row(key: &str, trickplay: TrickplayOptions) -> ServerSettingDto {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    let configuration = ServerConfiguration {
        trickplay_options: trickplay,
        ..ServerConfiguration::default()
    };
    let report = snapshot(
        SETTINGS,
        &running,
        &env,
        &configuration,
        &NetworkConfiguration::default(),
        false,
    )
    .expect("report");
    entry(&report, key).clone()
}

/// B11, B12 of the settings page; gl-1139 B2, B6: a row stored in the server
/// configuration is read from it, with its notes: a new server's value as the
/// default, another as set there, a pair that fits no option as Custom. It
/// applies at once and nothing locks it.
#[test]
fn a_row_stored_in_the_server_configuration_is_read_from_it() {
    let stored = |full_chroma, qscale| {
        stored_row(
            STORED,
            TrickplayOptions {
                full_chroma,
                qscale,
                ..TrickplayOptions::default()
            },
        )
    };

    let row = stored(false, 4);
    assert_eq!(row.label, "Image Quality");
    assert_eq!(row.group, Group::Trickplay);
    assert_eq!(row.note.as_deref(), Some(TRICKPLAY_CHANGE));
    assert_eq!(row.store, ServerSettingStore::ServerConfiguration);
    assert_eq!(row.applies, Applies::Immediately);
    assert_eq!(row.source, Source::Default);
    assert_eq!(row.value, Some(json!("disk")));
    assert!(row.editable, "{row:?}");
    assert_eq!(row.locked_reason, None);
    assert_eq!(row.env_var, None);
    assert!(!row.custom);
    let labels: Vec<&str> = row.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["Optimize Disk Usage", "Optimize Quality"]);
    assert!(
        row.options.iter().all(|option| option.note.is_some()),
        "{row:?}"
    );

    let row = stored(true, 2);
    assert_eq!(row.source, Source::ServerConfiguration);
    assert_eq!(row.value, Some(json!("quality")));
    assert!(!row.custom);

    // Either half of a pair alone, as a dashboard edit of Qscale leaves it.
    for (full_chroma, qscale, shown) in [
        (false, 2, "Qscale 2, 4:2:0"),
        (true, 4, "Qscale 4, 4:4:4"),
        (false, 7, "Qscale 7, 4:2:0"),
    ] {
        let row = stored(full_chroma, qscale);
        assert_eq!(row.source, Source::ServerConfiguration);
        assert_eq!(row.value, Some(json!(shown)));
        assert!(row.custom, "{row:?}");
    }
}

/// gl-1139 B1, B2: every option of a row stored in the server configuration
/// reads back as itself once written, over a new server's configuration and
/// over one whose value is Custom.
#[test]
fn every_stored_option_reads_back_as_itself() {
    let custom = ServerConfiguration {
        trickplay_options: TrickplayOptions {
            width_resolutions: vec![480],
            qscale: 7,
            full_chroma: true,
            ..TrickplayOptions::default()
        },
        ..ServerConfiguration::default()
    };
    let mut seen = 0;
    for spec in SETTINGS {
        let SettingStore::ServerConfiguration(field) = spec.store else {
            continue;
        };
        for option in spec.options {
            for start in [ServerConfiguration::default(), custom.clone()] {
                let mut configuration = start;
                (field.write)(&mut configuration, &json!(option.value));
                assert_eq!(
                    (field.read)(&configuration),
                    json!(option.value),
                    "{}: {}",
                    spec.key,
                    option.value
                );
            }
            seen += 1;
        }
    }
    assert!(seen > 0, "no row is stored in the server configuration");
}

/// gl-1139 B1, B6: Preview Size reads `WidthResolutions` in any order, a
/// width listed twice counting once, and any other list as Custom.
#[test]
fn preview_size_reads_the_widths_in_any_order() {
    for (widths, shown, custom) in [
        (vec![320], "320", false),
        (vec![640], "640", false),
        (vec![320, 640], "both", false),
        (vec![640, 320], "both", false),
        (vec![320, 320], "320", false),
        (vec![480], "480", true),
        (vec![640, 320, 480], "320, 480, 640", true),
        (vec![], "none", true),
    ] {
        let row = stored_row(
            "trickplay_preview_size",
            TrickplayOptions {
                width_resolutions: widths.clone(),
                ..TrickplayOptions::default()
            },
        );
        assert_eq!(row.value, Some(json!(shown)), "{widths:?}");
        assert_eq!(row.custom, custom, "{widths:?}");
        assert_eq!(row.source == Source::Default, shown == "320", "{widths:?}");
    }
}
