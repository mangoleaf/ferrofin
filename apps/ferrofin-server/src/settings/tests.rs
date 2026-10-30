//! The settings table's drift tests and the report's sources, locks, notes
//! and saved values, against the real loader with a fake environment and a
//! temporary `config.toml`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{CommandFactory, Parser};
use ferrofin_traits::plugins::PluginManager;

use super::*;

/// A fake environment backed by a map — no real `std::env` mutation.
#[derive(Clone, Default)]
struct FakeEnv(HashMap<String, String>);

impl FakeEnv {
    /// An environment whose default data directory no host has, so a test
    /// reads no `config.toml` of the host's.
    fn new() -> Self {
        Self::default().with("XDG_DATA_HOME", "/nonexistent/ferrofin-settings-test")
    }

    fn with(mut self, key: &str, value: &str) -> Self {
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
struct DataDir {
    dir: tempfile::TempDir,
}

impl DataDir {
    fn new(lines: &[String]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        if !lines.is_empty() {
            std::fs::write(dir.path().join("config.toml"), lines.join("\n")).expect("write");
        }
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_file(&self) -> PathBuf {
        self.path().join("config.toml")
    }

    /// The flags that start the server on this data directory.
    fn cli(&self) -> Cli {
        Cli {
            data_dir: Some(self.path().to_path_buf()),
            ..Cli::default()
        }
    }
}

fn spec(key: &str) -> &'static SettingSpec {
    SETTINGS
        .iter()
        .find(|spec| spec.key == key)
        .unwrap_or_else(|| panic!("no row `{key}`"))
}

/// A `config.toml` value for `spec`'s key that the loader takes and that is
/// not the default.
fn sample(spec: &SettingSpec) -> String {
    match spec.key {
        "bind_addr" => "\"127.0.0.2\"".to_owned(),
        "log_level" => "\"debug\"".to_owned(),
        "log_format" => "\"text\"".to_owned(),
        "wasm_private_http_allow" => "\"*\"".to_owned(),
        _ => match spec.kind {
            SettingKind::Bool => "true".to_owned(),
            SettingKind::Integer { min, .. } => (min + 1).to_string(),
            SettingKind::CountOrAuto => "2".to_owned(),
            SettingKind::Text | SettingKind::Secret => "\"sample\"".to_owned(),
            SettingKind::Url => "\"http://sample.invalid\"".to_owned(),
            SettingKind::Path => "\"/sample\"".to_owned(),
            SettingKind::PathList => "[\"/sample\"]".to_owned(),
            SettingKind::NumberList => "[0.5, 1.0]".to_owned(),
        },
    }
}

fn file_rows() -> impl Iterator<Item = &'static SettingSpec> {
    SETTINGS
        .iter()
        .filter(|spec| spec.access != SettingAccess::EnvOnly)
}

/// The report `config` gives under `env`, the way the route runs it.
fn report(config: &Config, env: &FakeEnv) -> ServerSettingsDto {
    snapshot(config, env, false).expect("snapshot")
}

fn entry<'a>(report: &'a ServerSettingsDto, key: &str) -> &'a ServerSettingDto {
    report
        .settings
        .iter()
        .find(|setting| setting.key == key)
        .unwrap_or_else(|| panic!("no setting `{key}` in the report"))
}

fn load(cli: Cli, env: &FakeEnv) -> Config {
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

/// (b) Every row's variable has a row in `docs/CONFIG.md`; every row has a
/// description and a default.
#[test]
fn every_rows_variable_has_a_row_in_the_docs() {
    let docs = include_str!("../../../../docs/CONFIG.md");
    for spec in SETTINGS {
        let var = spec
            .env
            .unwrap_or_else(|| panic!("{} has no variable", spec.key));
        assert!(
            docs.contains(&format!("| `{var}` |")),
            "{var}: no row in docs/CONFIG.md"
        );
        assert!(!spec.description.is_empty(), "{}: no description", spec.key);
        assert!(!spec.default.is_empty(), "{}: no default", spec.key);
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
        // D2a: until the Networking page drives them.
        (
            "port",
            "Set it with the port key in config.toml, the FERROFIN_PORT",
        ),
        (
            "https_port",
            "Set it with the https_port key in config.toml or the FERROFIN_HTTPS_PORT",
        ),
        (
            "base_url",
            "Set it with the base_url key in config.toml or the FERROFIN_BASE_URL",
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
    let err = snapshot(&running, &env, false).expect_err("refused");
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
        let err = snapshot(&running, &env, false).expect_err("refused");
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
fn plugins(dir: &Path) -> Arc<dyn PluginManager> {
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
