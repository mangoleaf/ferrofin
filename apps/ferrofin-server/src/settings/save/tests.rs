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

use super::{save, to_toml};
use crate::config::Config;
use crate::settings::tests::{DataDir, FakeEnv, load, plugins, sample, spec};
use crate::settings::{
    FerrofinServerSettings, SAVES, SETTINGS, SettingAccess, SettingKind, save_and_report,
};

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
        if spec.access != SettingAccess::Editable {
            continue;
        }
        seen += 1;
        let value = match (spec.key, spec.kind) {
            ("log_level", _) => json!("debug"),
            ("log_format", _) => json!("text"),
            (_, SettingKind::Bool) => json!(true),
            (_, SettingKind::Integer { min, .. }) => json!(min + 1),
            (_, SettingKind::CountOrAuto) => json!(2),
            (_, SettingKind::Text | SettingKind::Secret) => json!("sample"),
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
        ("log_format", json!("xml"), "json or text"),
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
        ("scan_probe_concurrency", json!(-1), "whole number from 0"),
        ("scan_probe_concurrency", json!(4.5), "whole number from 0"),
        ("scan_probe_concurrency", json!("4"), "whole number from 0"),
        ("scan_probe_concurrency", json!(true), "whole number from 0"),
        ("auth_cache_ttl_secs", json!(4), "from 5 to 300"),
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
        ("log_format", json!(3), "takes text"),
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
    )
    .with_env(env);
    (manager, lifecycle, plugins)
}

fn update(pairs: &[(&str, Value)]) -> ServerSettingsUpdateDto {
    ServerSettingsUpdateDto {
        changes: changes(pairs),
    }
}

/// B7: a save marks the restart, reports what it changed and what needs the
/// process restarted (the rows an in-process restart keeps, gl-526), and logs
/// the key names, never a value.
#[test]
fn a_save_marks_a_restart_and_logs_key_names_only() {
    let data = DataDir::new(&[]);
    let env = FakeEnv::new();
    let (shutdown, _rx) = tokio::sync::oneshot::channel();
    let lifecycle = crate::state::FerrofinLifecycleController::new(shutdown);
    let running = load(data.cli(), &env);

    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        save_and_report(
            &running,
            &env,
            &lifecycle,
            &changes(&[
                ("log_level", json!("debug")),
                ("omdb_api_key", json!("hunter2")),
                ("scan_probe_concurrency", json!(4)),
            ]),
        )
    })
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
    let keys = SETTINGS
        .iter()
        .filter(|spec| spec.access == SettingAccess::Editable)
        .filter(|spec| matches!(spec.kind, SettingKind::Integer { max, .. } if max > 1_000))
        .map(|spec| spec.key)
        .take(8);
    let mut saves = tokio::task::JoinSet::new();
    for key in keys {
        let manager = Arc::clone(&manager);
        saves.spawn(async move { manager.update(update(&[(key, json!(100))])).await });
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
