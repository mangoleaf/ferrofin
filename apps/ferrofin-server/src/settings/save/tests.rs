//! Saving to `config.toml`, against the real loader with a fake environment
//! and a temporary file: what a save keeps, what it refuses, and that a
//! refusal leaves the file byte-identical.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ferrofin_core::system_manager::LifecycleController as _;
use ferrofin_model::server_settings::{ServerSettingsUpdateDto, ServerSettingsUpdateResultDto};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::server_settings::ServerSettingsManager;
use serde_json::{Value, json};
use tracing_subscriber::fmt::MakeWriter;

use ferrofin_model::configuration::{ServerConfiguration, TrickplayOptions};

use super::{Stage, stage, to_toml};
use crate::config::{Config, Env};
use crate::settings::tests::{
    DataDir, FakeConfiguration, FakeEnv, STORED, load, plugins, sample, spec,
};
use crate::settings::{
    FerrofinServerSettings, SAVES, SETTINGS, SettingAccess, SettingKind, SettingSpec, SettingStore,
};

/// Saves `changes` to `running`'s `config.toml` as a save of only
/// `config.toml` rows runs: staged, then renamed over the file. The rows
/// whose entry changed.
fn save(
    running: &Config,
    env: &dyn Env,
    changes: &BTreeMap<String, Option<Value>>,
) -> Result<Vec<&'static SettingSpec>, ServiceError> {
    stage(SETTINGS, running, env, changes)?.map_or_else(|| Ok(Vec::new()), Stage::commit)
}

/// A `Changes` map from `(key, value)` pairs; a `null` removes the key.
fn changes(pairs: &[(&str, Value)]) -> BTreeMap<String, Option<Value>> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (!value.is_null()).then(|| value.clone())))
        .collect()
}

/// Saves `pairs` to `data`'s `config.toml` under `env`; the keys it changed.
fn saved(
    data: &DataDir,
    env: &FakeEnv,
    pairs: &[(&str, Value)],
) -> Result<Vec<&'static str>, ServiceError> {
    let rows = save(&load(data.cli(), env), env, &changes(pairs))?;
    Ok(rows.into_iter().map(|spec| spec.key).collect())
}

fn contents(data: &DataDir) -> String {
    std::fs::read_to_string(data.config_file()).expect("read config.toml")
}

/// The data directory's files, apart from `config.toml`: a save leaves none.
fn leftovers(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name != "config.toml")
        .collect();
    files.sort();
    files
}

/// `err`'s message when it is [`ServiceError::InvalidInput`].
fn refused(err: ServiceError) -> String {
    match err {
        ServiceError::InvalidInput(message) => message,
        other => panic!("not refused as invalid input: {other:?}"),
    }
}

/// `err`'s message when it is [`ServiceError::Conflict`].
fn conflict(err: ServiceError) -> String {
    match err {
        ServiceError::Conflict(message) => message,
        other => panic!("not a conflict: {other:?}"),
    }
}

// ---- what a save keeps ---------------------------------------------------------

/// B7: comments, layout and every other key survive, an existing key keeps its
/// place and the comment at the end of its line, a new key is added, and the
/// loader reads the result.
#[test]
fn a_save_changes_only_what_it_names() {
    let data = DataDir::new(&[]);
    let original = "# Ferrofin on the NAS.\n\
                    #\n\
                    # Keep this file private.\n\
                    \n\
                    server_name = \"nas\"\n\
                    scan_probe_concurrency = 2   # the disks are slow\n\
                    \n\
                    # A comment that belongs to nothing.\n\
                    #wasm_memory_limit_mb = 32\n\
                    log_level   =   'info'\n\
                    \n\
                    # The end of the file.\n";
    std::fs::write(data.config_file(), original).expect("write");
    let env = FakeEnv::new();

    let keys = saved(
        &data,
        &env,
        &[
            ("scan_probe_concurrency", json!(4)),
            ("omdb_api_key", json!("key \"with\" quotes")),
        ],
    )
    .expect("saved");

    assert_eq!(keys, ["omdb_api_key", "scan_probe_concurrency"]);
    assert_eq!(
        contents(&data),
        "# Ferrofin on the NAS.\n\
         #\n\
         # Keep this file private.\n\
         \n\
         server_name = \"nas\"\n\
         scan_probe_concurrency = 4   # the disks are slow\n\
         \n\
         # A comment that belongs to nothing.\n\
         #wasm_memory_limit_mb = 32\n\
         log_level   =   'info'\n\
         omdb_api_key = 'key \"with\" quotes'\n\
         \n\
         # The end of the file.\n"
    );
    let loaded = load(data.cli(), &env);
    assert_eq!(loaded.scan_probe_concurrency, Some(4));
    assert_eq!(loaded.omdb_api_key, "key \"with\" quotes");
    assert_eq!(loaded.server_name, "nas");
    assert!(
        leftovers(data.path()).is_empty(),
        "{:?}",
        leftovers(data.path())
    );
}

/// B7: `null` removes the key, so the setting goes back to its default. The
/// comment lines above it stay; the first key of a file holds the file's
/// header, and a save must not lose it.
#[test]
fn null_removes_a_key_and_keeps_the_comment_lines_above_it() {
    let data = DataDir::new(&[]);
    std::fs::write(
        data.config_file(),
        "# The file's header.\n\
         scan_probe_concurrency = 4 # gone with its line\n\
         \n\
         # About the log.\n\
         log_level = \"debug\"\n\
         # About the pool.\n\
         db_pool = 4\n",
    )
    .expect("write");
    let env = FakeEnv::new();

    let keys = saved(
        &data,
        &env,
        &[
            ("scan_probe_concurrency", Value::Null),
            ("db_pool", Value::Null),
        ],
    )
    .expect("saved");

    assert_eq!(keys, ["db_pool", "scan_probe_concurrency"]);
    assert_eq!(
        contents(&data),
        "# The file's header.\n\
         \n\
         # About the log.\n\
         log_level = \"debug\"\n\
         # About the pool.\n"
    );
    let loaded = load(data.cli(), &env);
    assert_eq!(loaded.scan_probe_concurrency, None);
    assert_eq!(loaded.db_pool, None);
}

/// Removing a key the file does not hold changes nothing: no write, and no
/// file is created for it.
#[test]
fn removing_a_key_the_file_lacks_writes_nothing() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    let keys = saved(&data, &env, &[("db_pool", Value::Null)]).expect("saved");
    assert!(keys.is_empty(), "{keys:?}");
    assert!(
        !data.config_file().exists(),
        "a save of nothing made a file"
    );

    std::fs::write(data.config_file(), "log_level = \"debug\"\n").expect("write");
    let keys = saved(&data, &env, &[("db_pool", Value::Null)]).expect("saved");
    assert!(keys.is_empty(), "{keys:?}");
    assert_eq!(contents(&data), "log_level = \"debug\"\n");
}

#[test]
fn a_new_key_starts_a_file_with_no_final_newline_on_its_own_line() {
    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let env = FakeEnv::new();
    saved(&data, &env, &[("db_pool", json!(4))]).expect("saved");
    assert_eq!(contents(&data), "log_level = \"debug\"\ndb_pool = 4\n");
}

/// (c) of the drift tests, through the save: every setting the page may
/// change, sent as the page sends it, loads back as its hand-written
/// `config.toml` line does.
#[test]
fn every_editable_setting_saves_what_a_hand_written_line_holds() {
    let env = FakeEnv::new();
    let mut seen = 0;
    for spec in SETTINGS {
        // A row stored in the server configuration has no line: the stored
        // rows' own tests save it.
        if spec.access != SettingAccess::Editable || !matches!(spec.store, SettingStore::ConfigFile)
        {
            continue;
        }
        seen += 1;
        let value = match (spec.key, spec.kind) {
            ("log_level", _) => json!("debug"),
            // As `sample`: the value that is not the default.
            (_, SettingKind::Bool) => json!(spec.default != "true"),
            (_, SettingKind::Integer { .. }) => {
                json!(sample(spec).parse::<i64>().expect("a number"))
            }
            (_, SettingKind::CountOrAuto) => json!(2),
            // As `sample`, unquoted.
            (_, SettingKind::Text | SettingKind::Secret | SettingKind::Choice) => {
                json!(sample(spec).trim_matches('"'))
            }
            (_, SettingKind::Url) => json!("http://sample.invalid"),
            (_, SettingKind::NumberList) => json!([0.5, 1.0]),
            (_, SettingKind::Path | SettingKind::PathList) => {
                panic!("{}: no editable row holds paths", spec.key)
            }
        };
        let data = DataDir::new(&[]);
        let keys = saved(&data, &env, &[(spec.key, value)])
            .unwrap_or_else(|err| panic!("{}: {err}", spec.key));
        assert_eq!(keys, [spec.key]);
        let by_hand = DataDir::new(&[format!("{} = {}", spec.key, sample(spec))]);
        assert_eq!(
            (spec.running)(&load(data.cli(), &env)),
            (spec.running)(&load(by_hand.cli(), &env)),
            "{}",
            spec.key
        );
    }
    assert!(seen > 30, "only {seen} editable rows");
}

/// No editable row holds paths yet; the kinds convert all the same, so a row
/// that becomes editable needs no change here.
#[test]
fn path_kinds_convert_to_toml() {
    assert_eq!(
        to_toml(spec("data_dir"), &json!("/srv"))
            .expect("a path")
            .to_string(),
        "\"/srv\""
    );
    assert_eq!(
        to_toml(spec("library_roots"), &json!(["/a", "/b"]))
            .expect("a list")
            .to_string(),
        "[\"/a\", \"/b\"]"
    );
    assert!(to_toml(spec("data_dir"), &json!(1)).is_err());
    assert!(to_toml(spec("library_roots"), &json!([1])).is_err());
}

/// A URL is saved trimmed: the providers use the value as it is, and only an
/// empty one means the default.
#[test]
fn a_url_is_saved_trimmed() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    saved(
        &data,
        &env,
        &[
            ("musicbrainz_base_url", json!(" http://mb.example ")),
            ("studios_repo_url", json!("   ")),
        ],
    )
    .expect("saved");
    let loaded = load(data.cli(), &env);
    assert_eq!(loaded.musicbrainz_base_url, "http://mb.example");
    assert_eq!(loaded.studios_repo_url, "");
}

/// A value the loader accepts in `auto`'s other spellings, and a count.
#[test]
fn count_or_auto_settings_take_a_count_and_auto() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    saved(
        &data,
        &env,
        &[
            ("db_pool", json!("AUTO")),
            ("trickplay_concurrency", json!(3)),
        ],
    )
    .expect("saved");
    let loaded = load(data.cli(), &env);
    assert_eq!(loaded.db_pool, None);
    assert_eq!(loaded.trickplay_concurrency, Some(3));
}

// ---- modes ----------------------------------------------------------------------

#[cfg(unix)]
mod modes {
    use std::os::unix::fs::PermissionsExt as _;

    use serde_json::json;

    use super::{DataDir, FakeEnv, contents, saved};

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    /// B7: a missing file is created owner-only: it may hold provider keys.
    #[test]
    fn a_new_file_is_owner_only() {
        let data = DataDir::new(&[]);
        saved(&data, &FakeEnv::new(), &[("omdb_api_key", json!("a-key"))]).expect("saved");
        assert_eq!(mode(&data.config_file()), 0o600);
        assert_eq!(contents(&data), "omdb_api_key = \"a-key\"\n");
    }

    /// The Debian package's `0640` stays `0640`, and so does a wider mode.
    #[test]
    fn an_existing_file_keeps_its_mode() {
        for kept in [0o640, 0o644] {
            let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
            std::fs::set_permissions(data.config_file(), std::fs::Permissions::from_mode(kept))
                .expect("chmod");
            saved(&data, &FakeEnv::new(), &[("db_pool", json!(4))]).expect("saved");
            assert_eq!(mode(&data.config_file()), kept);
            assert!(contents(&data).contains("db_pool"), "nothing was saved");
        }
    }

    /// A linked `config.toml` is saved through the link, which stays a link.
    #[test]
    fn a_linked_file_stays_a_link() {
        let data = DataDir::new(&[]);
        let elsewhere = tempfile::tempdir().expect("temp dir");
        let real = elsewhere.path().join("ferrofin.toml");
        std::fs::write(&real, "log_level = \"debug\"\n").expect("write");
        std::os::unix::fs::symlink(&real, data.config_file()).expect("link");

        saved(&data, &FakeEnv::new(), &[("db_pool", json!(4))]).expect("saved");

        let link = std::fs::symlink_metadata(data.config_file()).expect("stat");
        assert!(link.file_type().is_symlink(), "the link was replaced");
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            "log_level = \"debug\"\ndb_pool = 4\n"
        );
    }
}

// ---- what a save refuses ---------------------------------------------------------

/// B3: a setting a variable or a flag sets is refused with the note the
/// snapshot shows, naming it, and the file is untouched.
#[test]
fn a_setting_a_variable_sets_is_refused_with_its_note() {
    let data = DataDir::new(&["db_pool = 8".to_owned()]);
    let env = FakeEnv::new()
        .with("FERROFIN_DB_POOL", "4")
        .with("RUST_LOG", "warn");
    let before = contents(&data);
    let running = load(data.cli(), &env);

    for (key, value, variable) in [
        ("db_pool", json!(2), "FERROFIN_DB_POOL"),
        ("db_pool", Value::Null, "FERROFIN_DB_POOL"),
        ("log_level", json!("debug"), "RUST_LOG"),
    ] {
        let err = save(&running, &env, &changes(&[(key, value)])).expect_err("refused");
        let message = refused(err);
        assert!(message.contains(key), "{message}");
        assert!(message.contains(variable), "{message}");
    }
    assert_eq!(contents(&data), before);
}

/// A variable the loader ignores sets nothing, so the page may change the key.
#[test]
fn a_variable_the_loader_ignores_locks_nothing() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new().with("FERROFIN_SCAN_PROBE_CONCURRENCY", "many");
    let keys = saved(&data, &env, &[("scan_probe_concurrency", json!(4))]).expect("saved");
    assert_eq!(keys, ["scan_probe_concurrency"]);
}

/// B4, B13: what the web may never change is refused with its reason, even in
/// a request made by hand, and a flag on it is named too.
#[test]
fn a_read_only_setting_is_refused_with_its_reason() {
    let data = DataDir::new(&["server_name = \"nas\"".to_owned()]);
    let env = FakeEnv::new();
    let before = contents(&data);
    let mut cli = data.cli();
    cli.ffmpeg_path = Some("/usr/bin/ffmpeg".into());
    let running = Config::load_from(cli, &env).expect("load");

    for (key, value, reason) in [
        (
            "ffmpeg_path",
            json!("/bin/sh"),
            "may not choose an executable",
        ),
        ("ffmpeg_path", json!("/bin/sh"), "--ffmpeg"),
        (
            "ffprobe_path",
            json!("/bin/sh"),
            "may not choose an executable",
        ),
        ("wasm_private_http_allow", json!("*"), "an admin session"),
        (
            "image_private_http_allow",
            json!("192.168.0.0/16"),
            "an admin session",
        ),
        (
            "subtitle_private_http_allow",
            json!("192.168.0.0/16"),
            "an admin session",
        ),
        ("admin_password", json!("hunter2"), "database is created"),
        ("data_dir", json!("/tmp"), "other files"),
        ("disable_extensions", json!(true), "measurement switch"),
        ("server_name", Value::Null, "Dashboard → General"),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            json!("http://otel"),
            "OpenTelemetry",
        ),
    ] {
        let err = save(&running, &env, &changes(&[(key, value)])).expect_err("refused");
        let message = refused(err);
        assert!(message.contains(key), "{message}");
        assert!(message.contains(reason), "{key}: {message}");
        assert!(!message.contains("hunter2"), "{message}");
    }
    assert_eq!(contents(&data), before);
    assert_eq!(leftovers(data.path()), Vec::<String>::new());
}

/// One refused change refuses the whole save: nothing else is written.
#[test]
fn one_refused_change_writes_nothing() {
    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let before = contents(&data);
    let err = saved(
        &data,
        &FakeEnv::new(),
        &[
            ("scan_probe_concurrency", json!(4)),
            ("no_such_setting", json!(1)),
        ],
    )
    .expect_err("refused");
    let message = refused(err);
    assert!(message.contains("no_such_setting"), "{message}");
    assert_eq!(contents(&data), before);
}

/// B6: every invalid value is refused with its rule's message; `config.toml`
/// is byte-identical afterwards and no staged file stays behind. The rules that
/// only warn at boot are enforced here, so a value the server would ignore is
/// never saved.
#[test]
fn an_invalid_value_leaves_the_file_byte_identical() {
    let original = "# Hands off.\nlog_level = \"debug\"\n";
    let data = DataDir::new(&[]);
    std::fs::write(data.config_file(), original).expect("write");
    let env = FakeEnv::new();

    for (key, value, rule) in [
        // The loader's own rules, in the loader's own words.
        (
            "db_pool",
            json!(0),
            "the pool needs at least one connection",
        ),
        ("db_pool", json!("x"), "expected an integer or \"auto\""),
        (
            "trickplay_concurrency",
            json!("x"),
            "expected an integer or \"auto\"",
        ),
        // Rules that only warn at boot.
        ("tvdb_token_days", json!(40), "from 1 to 29"),
        ("tvdb_token_days", json!(0), "from 1 to 29"),
        ("tvdb_cache_hours", json!(0), "from 1 to"),
        (
            "provider_timeout_seconds",
            json!(100_000),
            "whole number from",
        ),
        ("log_format", json!("xml"), "takes one of json, text"),
        ("log_level", json!("=not[a]filter"), "RUST_LOG syntax"),
        ("metrics_scan_duration_buckets", json!([]), "empty"),
        (
            "metrics_scan_duration_buckets",
            json!([2.0, 1.0]),
            "strictly increasing",
        ),
        (
            "metrics_scan_duration_buckets",
            json!([0.0, 1.0]),
            "above zero",
        ),
        // The kinds.
        ("scan_probe_concurrency", json!(-1), "whole number from 1"),
        ("scan_probe_concurrency", json!(4.5), "whole number from 1"),
        ("scan_probe_concurrency", json!("4"), "whole number from 1"),
        ("scan_probe_concurrency", json!(true), "whole number from 1"),
        ("auth_cache_ttl_secs", json!(4), "from 5 to 300"),
        ("image_decode_max_megapixels", json!(49), "from 50 to 1000"),
        (
            "image_decode_max_memory_mb",
            json!(4096),
            "from 256 to 2048",
        ),
        ("image_resize_budget_mb", json!(0), "from 512 to 8192"),
        ("image_download_max_mb", json!(15), "from 16 to 256"),
        (
            "image_negative_cache_ttl_secs",
            json!(299),
            "from 300 to 86400",
        ),
        (
            "ffmpeg_img_extract_perf_tradeoff",
            json!("yes"),
            "true or false",
        ),
        (
            "trickplay_concurrency",
            json!(1.5),
            "whole number or \"auto\"",
        ),
        (
            "trickplay_concurrency",
            json!([]),
            "whole number or \"auto\"",
        ),
        ("log_format", json!(3), "takes one of json, text"),
        (
            "musicbrainz_base_url",
            json!("not a url"),
            "http or https URL",
        ),
        (
            "musicbrainz_base_url",
            json!("ftp://example.org"),
            "http or https URL",
        ),
        ("musicbrainz_base_url", json!(1), "takes text"),
        ("metrics_scan_duration_buckets", json!(1), "list of numbers"),
        (
            "metrics_scan_duration_buckets",
            json!(["a"]),
            "list of numbers",
        ),
    ] {
        let err = saved(&data, &env, &[(key, value.clone())]).expect_err("refused");
        let message = refused(err);
        assert!(message.contains(key), "{key} = {value}: {message}");
        assert!(message.contains(rule), "{key} = {value}: {message}");
        assert_eq!(contents(&data), original, "{key} = {value}");
        assert!(
            leftovers(data.path()).is_empty(),
            "{key} = {value}: {:?}",
            leftovers(data.path())
        );
    }
}

/// The transcode caps take 0, which is off, to 1024, and saving refuses the
/// rest, where the loader would clamp them.
#[test]
fn a_transcode_cap_outside_its_range_is_refused() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    for (key, value) in [
        ("max_transcodes", json!(1025)),
        ("max_transcodes_per_user", json!(-1)),
    ] {
        let err = saved(&data, &env, &[(key, value)]).expect_err("refused");
        let message = refused(err);
        assert!(message.contains("from 0 to 1024"), "{message}");
    }
}

/// gl-734: the ffmpeg input-analysis settings take ffmpeg's syntax inside
/// their ranges, and blank for the default; saving refuses what the loader
/// would ignore or clamp.
#[test]
fn an_ffmpeg_input_analysis_value_is_checked_on_save() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    for (key, value, why) in [
        ("ffmpeg_analyze_duration", "601M", "is outside 1M to 600M"),
        ("ffmpeg_probe_size", "999K", "is outside 1M to 4G"),
        ("ffmpeg_probe_size", "1GB", "is not an ffmpeg value"),
    ] {
        let err = saved(&data, &env, &[(key, json!(value))]).expect_err("refused");
        let message = refused(err);
        assert!(message.contains(key) && message.contains(why), "{message}");
    }
    saved(
        &data,
        &env,
        &[
            ("ffmpeg_analyze_duration", json!("8M")),
            ("ffmpeg_probe_size", json!("32Mi")),
        ],
    )
    .expect("saved");
    let file = contents(&data);
    assert!(
        file.contains("ffmpeg_analyze_duration = \"8M\"")
            && file.contains("ffmpeg_probe_size = \"32Mi\""),
        "{file}"
    );
    // The page sends an emptied field as blank: saved, and loaded as the default.
    saved(&data, &env, &[("ffmpeg_probe_size", json!(""))]).expect("blank saved");
    assert!(
        contents(&data).contains("ffmpeg_probe_size = \"\""),
        "{}",
        contents(&data)
    );
}

/// B1: a value outside a setting's range is refused naming the range, and the
/// file is untouched. That includes `0` on the rows where the loader reads it
/// as the default: Reset is how a value goes back to its default.
#[test]
fn a_value_outside_a_settings_range_is_refused_naming_it() {
    let original = "log_level = \"debug\"\n";
    let data = DataDir::new(&[]);
    std::fs::write(data.config_file(), original).expect("write");
    let env = FakeEnv::new();
    for (key, value, range) in [
        ("quick_connect_pending_per_ip", json!(0), "from 1 to 1000"),
        ("metrics_sample_interval", json!(0), "from 1 to 300"),
        ("wasm_call_timeout_secs", json!(0), "from 1 to 600"),
        ("wasm_call_grace_secs", json!(1), "from 2 to 600"),
        ("wasm_memory_limit_mb", json!(0), "from 16 to 4096"),
        ("wasm_event_queue_capacity", json!(0), "from 16 to 65536"),
        ("max_plugin_download_mb", json!(0), "from 8 to 1024"),
        ("wasm_subtitle_extract_mb", json!(0), "from 1 to 128"),
        ("wasm_subtitle_extract_mb", json!(129), "from 1 to 128"),
        ("scan_progress_every", json!(100_001), "from 0 to 100000"),
        (
            "scan_directory_cache_records",
            json!(999),
            "from 1000 to 10000000",
        ),
        (
            "book_archive_max_entries",
            json!(999),
            "from 1000 to 10000000",
        ),
        ("tvdb_cache_hours", json!(169), "from 1 to 168"),
        ("shutdown_timeout_secs", json!(4), "from 5 to 600"),
        ("item_values_write_chunk", json!(0), "from 1 to 5000"),
        ("item_values_write_chunk", json!(5001), "from 1 to 5000"),
        ("session_idle_hours", json!(0), "from 1 to 168"),
        ("session_idle_hours", json!(169), "from 1 to 168"),
    ] {
        let err = saved(&data, &env, &[(key, value.clone())]).expect_err("refused");
        let message = refused(err);
        assert!(message.contains(key), "{key} = {value}: {message}");
        assert!(message.contains(range), "{key} = {value}: {message}");
        assert_eq!(contents(&data), original, "{key} = {value}");
    }
    // `0` is "off" for the scan's progress lines, inside its range.
    let keys = saved(&data, &env, &[("scan_progress_every", json!(0))]).expect("saved");
    assert_eq!(keys, ["scan_progress_every"]);
}

/// A message from the loader names `config.toml`, not the staged file it read.
#[test]
fn a_loader_message_names_config_toml() {
    let data = DataDir::new(&[]);
    std::fs::write(data.config_file(), "log_level = \"debug\"\n").expect("write");
    let env = FakeEnv::new();
    // Past the kind check: the loader refuses the file's type for this key.
    let err = saved(&data, &env, &[("db_pool", json!(-1))]).expect_err("refused");
    let message = refused(err);
    assert!(
        message.contains(&data.config_file().display().to_string()),
        "{message}"
    );
    assert!(!message.contains(".ferrofin-settings-save"), "{message}");
}

/// A secret of the wrong kind is refused without quoting it.
#[test]
fn a_refusal_quotes_no_secret() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    for value in [
        json!(["hunter2"]),
        json!({ "nested": "hunter2" }),
        json!(12345),
    ] {
        let err = saved(&data, &env, &[("omdb_api_key", value)]).expect_err("refused");
        let message = refused(err);
        assert!(message.contains("omdb_api_key"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("12345"), "{message}");
    }
    assert!(!data.config_file().exists());
}

// ---- a file that cannot be written or read ---------------------------------------

/// B9: a read-only `config.toml` answers a conflict naming it, even though the
/// rename a save does would succeed: a read-only file is the admin's choice.
#[cfg(unix)]
#[test]
fn a_read_only_file_is_a_conflict_naming_it() {
    use std::os::unix::fs::PermissionsExt as _;

    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let before = contents(&data);
    let path = data.config_file();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod");

    let err = saved(&data, &FakeEnv::new(), &[("db_pool", json!(4))]).expect_err("refused");

    let message = conflict(err);
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert_eq!(contents(&data), before);
    assert_eq!(leftovers(data.path()), Vec::<String>::new());
    assert!(
        !crate::settings::config_file_writable(&path),
        "the report calls it writable"
    );
}

/// B9: a directory that takes no new file answers a conflict naming the file.
#[cfg(unix)]
#[test]
fn a_directory_that_takes_no_file_is_a_conflict_naming_it() {
    use std::os::unix::fs::PermissionsExt as _;

    /// Gives the directory back, so the test's clean-up can remove it.
    struct Restore<'a>(&'a Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let before = contents(&data);
    let path = data.config_file();
    let running = load(data.cli(), &FakeEnv::new());
    let _restore = Restore(data.path());
    std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o555)).expect("chmod");
    if crate::settings::directory_accepts_a_file(&path) {
        // Root ignores the mode; nothing here can refuse it.
        return;
    }

    let err = save(
        &running,
        &FakeEnv::new(),
        &changes(&[("db_pool", json!(4))]),
    )
    .expect_err("refused");

    let message = conflict(err);
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(
        message.contains(&data.path().display().to_string()),
        "{message}"
    );
    assert_eq!(contents(&data), before);
}

/// A file that stopped parsing is reported as the snapshot reports it, without
/// quoting the line at fault, which may hold a secret.
#[test]
fn a_file_that_no_longer_parses_is_a_conflict_that_quotes_no_secret() {
    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    let broken = "omdb_api_key = \"unterminated-secret\n";
    std::fs::write(data.config_file(), broken).expect("edit");

    let err = save(&running, &env, &changes(&[("db_pool", json!(4))])).expect_err("refused");

    let message = conflict(err);
    assert!(
        message.contains(&data.config_file().display().to_string()),
        "{message}"
    );
    assert!(!message.contains("unterminated-secret"), "{message}");
    assert_eq!(contents(&data), broken);
}

/// A save re-reads the file: a hand edit since the server started is kept, not
/// overwritten with what the server read at boot.
#[test]
fn a_save_keeps_a_hand_edit_made_since_the_server_started() {
    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    std::fs::write(data.config_file(), "log_level = \"trace\"\n").expect("edit");

    save(&running, &env, &changes(&[("db_pool", json!(4))])).expect("saved");

    assert_eq!(contents(&data), "log_level = \"trace\"\ndb_pool = 4\n");
}

/// A hand edit that the loader refuses blocks a save of anything else, but a
/// save that fixes it goes through: the boot rules judge the file that results.
#[test]
fn a_save_is_judged_on_the_file_it_would_leave() {
    let data = DataDir::new(&["log_level = \"debug\"".to_owned()]);
    let env = FakeEnv::new();
    let running = load(data.cli(), &env);
    std::fs::write(data.config_file(), "db_pool = 0\n").expect("edit");

    let err = save(&running, &env, &changes(&[("log_level", json!("info"))])).expect_err("refused");
    assert!(refused(err).contains("db_pool"));

    save(&running, &env, &changes(&[("db_pool", json!(4))])).expect("the fix saves");
    assert_eq!(contents(&data), "db_pool = 4\n");
}

// ---- the manager ----------------------------------------------------------------

/// Collects what a test's log lines say.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// A manager over `data`, and the lifecycle whose restart flag it sets.
fn manager(
    data: &DataDir,
    env: FakeEnv,
) -> (
    FerrofinServerSettings,
    Arc<crate::state::FerrofinLifecycleController>,
    Arc<dyn ferrofin_traits::plugins::PluginManager>,
) {
    let (shutdown, _rx) = tokio::sync::oneshot::channel();
    let lifecycle = Arc::new(crate::state::FerrofinLifecycleController::new(shutdown));
    let plugins = plugins(data.path());
    let manager = FerrofinServerSettings::new(
        load(data.cli(), &env),
        lifecycle.clone(),
        Arc::clone(&plugins),
        FakeConfiguration::new(ServerConfiguration::default()),
    )
    .with_env(env);
    (manager, lifecycle, plugins)
}

/// A manager over `data` and `configuration`.
fn stored_manager(
    data: &DataDir,
    configuration: Arc<FakeConfiguration>,
) -> (
    FerrofinServerSettings,
    Arc<crate::state::FerrofinLifecycleController>,
) {
    let env = FakeEnv::new();
    let (shutdown, _rx) = tokio::sync::oneshot::channel();
    let lifecycle = Arc::new(crate::state::FerrofinLifecycleController::new(shutdown));
    let manager = FerrofinServerSettings::new(
        load(data.cli(), &env),
        lifecycle.clone(),
        plugins(data.path()),
        configuration,
    )
    .with_env(env);
    (manager, lifecycle)
}

fn update(pairs: &[(&str, Value)]) -> ServerSettingsUpdateDto {
    ServerSettingsUpdateDto {
        changes: changes(pairs),
    }
}

/// B7: a save marks the restart, reports what it changed and what needs the
/// process restarted (the rows an in-process restart keeps, gl-526), and logs
/// the key names, never a value.
#[tokio::test]
async fn a_save_marks_a_restart_and_logs_key_names_only() {
    let data = DataDir::new(&[]);
    let (manager, lifecycle, _plugins) = manager(&data, FakeEnv::new());

    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();
    // The save's task runs on this test's one runtime thread, which logs to
    // `log`.
    let _log = tracing::subscriber::set_default(subscriber);
    let result = manager
        .update(update(&[
            ("log_level", json!("debug")),
            ("omdb_api_key", json!("hunter2")),
            ("scan_probe_concurrency", json!(4)),
        ]))
        .await
        .expect("saved");

    assert_eq!(
        result.changed,
        ["log_level", "omdb_api_key", "scan_probe_concurrency"]
    );
    assert!(result.restart_required);
    assert_eq!(result.process_restart_required, ["log_level"]);
    assert!(lifecycle.has_pending_restart(), "the restart is not marked");

    let lines = String::from_utf8(log.0.lock().expect("log").clone()).expect("utf-8");
    assert!(lines.contains("server settings saved"), "{lines}");
    assert!(lines.contains("omdb_api_key"), "{lines}");
    assert!(!lines.contains("hunter2"), "{lines}");
}

/// The manager's save, end to end: the file, the restart flag and the answer.
#[tokio::test]
async fn the_manager_saves_and_marks_a_restart() {
    let data = DataDir::new(&[]);
    let (manager, lifecycle, _plugins) = manager(&data, FakeEnv::new());

    let result = manager
        .update(update(&[("log_format", json!("text"))]))
        .await
        .expect("saved");

    assert_eq!(result.changed, ["log_format"]);
    assert_eq!(result.process_restart_required, ["log_format"]);
    assert!(lifecycle.has_pending_restart());
    assert_eq!(contents(&data), "log_format = \"text\"\n");
}

/// A caller that goes away (a client that disconnects) does not stop a save it
/// started: the lock stays held to the end, and the restart is still marked,
/// so no second save can start on a half-done one.
#[tokio::test]
async fn a_save_outlives_its_caller() {
    let data = DataDir::new(&[]);
    let (manager, lifecycle, _plugins) = manager(&data, FakeEnv::new());

    let another_save = SAVES.lock().await;
    let caller =
        tokio::spawn(async move { manager.update(update(&[("db_pool", json!(4))])).await });
    // On this thread, the caller runs up to the wait for its blocking task.
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    caller.abort();
    assert!(
        caller.await.is_err_and(|err| err.is_cancelled()),
        "the caller finished before it could go away"
    );
    assert!(!data.config_file().exists(), "the save ran under the lock");

    drop(another_save);
    for _ in 0..1_000 {
        if lifecycle.has_pending_restart() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(
        lifecycle.has_pending_restart(),
        "the save died with its caller"
    );
    assert_eq!(contents(&data), "db_pool = 4\n");
}

/// A save that changes nothing marks nothing.
#[tokio::test]
async fn a_save_of_nothing_marks_no_restart() {
    let data = DataDir::new(&[]);
    let (manager, lifecycle, _plugins) = manager(&data, FakeEnv::new());

    for empty in [update(&[]), update(&[("db_pool", Value::Null)])] {
        let result = manager.update(empty).await.expect("saved");
        assert_eq!(result, ServerSettingsUpdateResultDto::default());
    }
    assert!(!lifecycle.has_pending_restart());
    assert!(!data.config_file().exists());
}

/// A refused save marks nothing either, and two saves at once both land whole.
#[tokio::test(flavor = "multi_thread")]
async fn saves_at_once_each_land_whole() {
    let data = DataDir::new(&[]);
    let (manager, lifecycle, _plugins) = manager(&data, FakeEnv::new());

    let refused = manager.update(update(&[("db_pool", json!(0))])).await;
    assert!(
        matches!(refused, Err(ServiceError::InvalidInput(_))),
        "{refused:?}"
    );
    assert!(
        !lifecycle.has_pending_restart(),
        "a refusal marked a restart"
    );

    let manager = Arc::new(manager);
    let changes = SETTINGS
        .iter()
        .filter(|spec| spec.access == SettingAccess::Editable)
        // A value inside each row's range.
        .filter_map(|spec| match spec.kind {
            SettingKind::Integer { min, max } if max > 1_000 => {
                Some((spec.key, 100.clamp(min, max)))
            }
            _ => None,
        })
        .take(8);
    let mut saves = tokio::task::JoinSet::new();
    for (key, value) in changes {
        let manager = Arc::clone(&manager);
        saves.spawn(async move { manager.update(update(&[(key, json!(value))])).await });
    }
    let mut written = Vec::new();
    while let Some(save) = saves.join_next().await {
        written.extend(save.expect("joined").expect("saved").changed);
    }

    let text = contents(&data);
    for key in &written {
        assert!(text.contains(key.as_str()), "{key} is lost: {text}");
    }
    assert_eq!(written.len(), 8);
}

#[tokio::test]
async fn a_save_while_the_plugin_is_disabled_is_not_found() {
    let data = DataDir::new(&[]);
    let (manager, lifecycle, plugins) = manager(&data, FakeEnv::new());
    plugins
        .disable_plugin(ferrofin_extensions::ferrofin_settings::EXTENSION_ID)
        .await
        .expect("disable");

    let result = manager.update(update(&[("db_pool", json!(4))])).await;

    assert!(
        matches!(result, Err(ServiceError::NotFound(_))),
        "{result:?}"
    );
    assert!(
        !data.config_file().exists(),
        "a disabled plugin wrote the file"
    );
    assert!(!lifecycle.has_pending_restart());
}

// ---- choices and the server configuration (gl-1174) -----------------------------

/// B10: a choice takes one of its options, by value, and nothing else.
#[test]
fn a_choice_takes_only_its_options() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();

    let err = saved(&data, &env, &[("log_format", json!("xml"))]).expect_err("refused");
    assert_eq!(refused(err), "`log_format` takes one of json, text");
    assert!(!data.config_file().exists(), "a refusal wrote the file");

    assert_eq!(
        saved(&data, &env, &[("log_format", json!("text"))]).expect("saved"),
        ["log_format"]
    );
    assert_eq!(contents(&data), "log_format = \"text\"\n");
}

/// B10: a value `config.toml` holds that fits no option (Custom) survives a
/// save that does not name it.
#[test]
fn a_custom_choice_survives_a_save_of_other_keys() {
    let data = DataDir::new(&["log_format = \"xml\"".to_owned()]);
    let env = FakeEnv::new();

    assert_eq!(
        saved(&data, &env, &[("db_pool", json!(4))]).expect("saved"),
        ["db_pool"]
    );
    assert_eq!(contents(&data), "log_format = \"xml\"\ndb_pool = 4\n");
}

/// The files a save leaves beside `config.toml` while it is staged.
fn staged_files(dir: &Path) -> Vec<String> {
    leftovers(dir)
        .into_iter()
        .filter(|name| name.starts_with(".ferrofin-settings-save."))
        .collect()
}

/// B12: a row stored in the server configuration is saved through the
/// configuration manager and applies at once: no restart, and `config.toml`
/// is never touched.
#[tokio::test]
async fn a_row_stored_in_the_server_configuration_saves_through_it() {
    let data = DataDir::new(&[]);
    let configuration = FakeConfiguration::new(ServerConfiguration::default());
    let (manager, lifecycle) = stored_manager(&data, Arc::clone(&configuration));

    let result = manager
        .update(update(&[(STORED, json!("quality"))]))
        .await
        .expect("saved");

    assert_eq!(
        result,
        ServerSettingsUpdateResultDto {
            applied_now: vec![STORED.to_owned()],
            ..ServerSettingsUpdateResultDto::default()
        }
    );
    assert_eq!(configuration.updates(), 1);
    let json = configuration.json();
    assert!(
        json.contains(r#""Qscale":2"#) && json.contains(r#""FullChroma":true"#),
        "{json}"
    );
    assert!(!lifecycle.has_pending_restart(), "a restart was marked");
    assert!(!data.config_file().exists(), "config.toml was written");

    let report = manager.snapshot().await.expect("report");
    let row = report
        .settings
        .iter()
        .find(|setting| setting.key == STORED)
        .expect("the stored row is reported");
    assert_eq!(row.value, Some(json!("quality")));
}

/// B12: a save that changes nothing stored leaves the server configuration as
/// it was, byte for byte, so `GET /System/Configuration` answers what it did:
/// a stored row named with the value it holds, a save of `config.toml` only,
/// and `null` on a row at a new server's value.
#[tokio::test]
async fn a_save_that_changes_nothing_stored_leaves_the_configuration_byte_identical() {
    let data = DataDir::new(&[]);
    let configuration = FakeConfiguration::new(ServerConfiguration::default());
    let before = configuration.json();
    let (manager, _lifecycle) = stored_manager(&data, Arc::clone(&configuration));

    for unchanged in [
        update(&[(STORED, json!("disk"))]),
        update(&[(STORED, Value::Null)]),
    ] {
        let result = manager.update(unchanged).await.expect("saved");
        assert_eq!(result, ServerSettingsUpdateResultDto::default());
    }
    let filed = manager
        .update(update(&[("db_pool", json!(4))]))
        .await
        .expect("saved");
    assert_eq!(filed.changed, ["db_pool"]);
    assert!(filed.restart_required);
    assert_eq!(filed.applied_now, Vec::<String>::new());

    assert_eq!(configuration.updates(), 0, "the configuration was saved");
    assert_eq!(configuration.json(), before);
}

/// B10, B12: a stored value that fits no option is reported as Custom, as it
/// is, and stays until a save names its row; `null` then puts a new server's
/// value back.
#[tokio::test]
async fn a_custom_stored_value_stays_until_its_row_is_saved() {
    let custom = ServerConfiguration {
        trickplay_options: TrickplayOptions {
            qscale: 7,
            ..TrickplayOptions::default()
        },
        ..ServerConfiguration::default()
    };
    let configuration = FakeConfiguration::new(custom);
    let before = configuration.json();
    let data = DataDir::new(&[]);
    let (manager, _lifecycle) = stored_manager(&data, Arc::clone(&configuration));

    manager
        .update(update(&[("db_pool", json!(4))]))
        .await
        .expect("saved");
    assert_eq!(
        configuration.json(),
        before,
        "another key's save changed it"
    );
    let report = manager.snapshot().await.expect("report");
    let row = report
        .settings
        .iter()
        .find(|setting| setting.key == STORED)
        .expect("the stored row is reported");
    assert!(row.custom, "{row:?}");
    assert_eq!(row.value, Some(json!("Qscale 7, 4:2:0")));

    let result = manager
        .update(update(&[(STORED, Value::Null)]))
        .await
        .expect("saved");
    assert_eq!(result.applied_now, [STORED]);
    assert_eq!(
        configuration.json(),
        FakeConfiguration::new(ServerConfiguration::default()).json()
    );
}

/// A save naming both stores is all or nothing: a refused value in either,
/// or a server configuration that cannot be saved, leaves `config.toml` and
/// the configuration as they were; one that passes saves both.
#[tokio::test]
async fn a_save_naming_both_stores_is_all_or_nothing() {
    let data = DataDir::new(&["db_pool = 2".to_owned()]);
    let configuration = FakeConfiguration::new(ServerConfiguration::default());
    let before = configuration.json();
    let (manager, lifecycle) = stored_manager(&data, Arc::clone(&configuration));

    let err = manager
        .update(update(&[
            ("db_pool", json!(4)),
            (STORED, json!("Sideways")),
        ]))
        .await
        .expect_err("refused");
    assert_eq!(
        refused(err),
        "`trickplay_image_quality` takes one of disk, quality"
    );
    let err = manager
        .update(update(&[("db_pool", json!(0)), (STORED, json!("quality"))]))
        .await
        .expect_err("refused");
    assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
    assert_eq!(configuration.updates(), 0);
    assert_eq!(configuration.json(), before);
    assert_eq!(contents(&data), "db_pool = 2");
    assert!(!lifecycle.has_pending_restart());

    // A configuration that cannot be saved: the staged file goes too.
    let failing = FakeConfiguration::failing(ServerConfiguration::default());
    let (manager, lifecycle) = stored_manager(&data, failing);
    let err = manager
        .update(update(&[("db_pool", json!(4)), (STORED, json!("quality"))]))
        .await
        .expect_err("the update failed");
    assert!(matches!(err, ServiceError::Backend(_)), "{err:?}");
    assert_eq!(contents(&data), "db_pool = 2");
    assert!(
        staged_files(data.path()).is_empty(),
        "a staged file is left"
    );
    assert!(!lifecycle.has_pending_restart());

    let (manager, lifecycle) = stored_manager(&data, Arc::clone(&configuration));
    let result = manager
        .update(update(&[("db_pool", json!(4)), (STORED, json!("quality"))]))
        .await
        .expect("saved");
    assert_eq!(result.changed, ["db_pool"]);
    assert_eq!(result.applied_now, [STORED]);
    assert!(result.restart_required);
    assert!(lifecycle.has_pending_restart());
    assert_eq!(contents(&data), "db_pool = 4\n");
    assert_eq!(configuration.updates(), 1);
}

/// B12: a save of the server configuration alone never reads `config.toml`,
/// so one that no longer parses cannot stop it.
#[tokio::test]
async fn a_stored_row_saves_over_a_broken_config_toml() {
    let data = DataDir::new(&[]);
    let configuration = FakeConfiguration::new(ServerConfiguration::default());
    let (manager, lifecycle) = stored_manager(&data, Arc::clone(&configuration));
    std::fs::write(data.config_file(), "db_pool = [[[").expect("break config.toml");

    let result = manager
        .update(update(&[(STORED, json!("quality"))]))
        .await
        .expect("saved");

    assert_eq!(result.applied_now, [STORED]);
    assert_eq!(configuration.updates(), 1);
    assert_eq!(contents(&data), "db_pool = [[[");
    assert!(!lifecycle.has_pending_restart());
}

/// The server configuration is read only once `config.toml` is staged, just
/// before it is written, so a Dashboard save made meanwhile is not written
/// over: a save whose `config.toml` part is refused never reads it.
#[tokio::test]
async fn the_server_configuration_is_read_only_once_config_toml_is_staged() {
    let data = DataDir::new(&[]);
    let configuration = FakeConfiguration::new(ServerConfiguration::default());
    let (manager, _lifecycle) = stored_manager(&data, Arc::clone(&configuration));

    let err = manager
        .update(update(&[("db_pool", json!(0)), (STORED, json!("quality"))]))
        .await
        .expect_err("refused");
    assert!(matches!(err, ServiceError::InvalidInput(_)), "{err:?}");
    assert_eq!(
        configuration.reads(),
        0,
        "read before config.toml was staged"
    );

    manager
        .update(update(&[("db_pool", json!(4)), (STORED, json!("quality"))]))
        .await
        .expect("saved");
    assert_eq!(configuration.reads(), 1);
    assert_eq!(configuration.updates(), 1);
}
