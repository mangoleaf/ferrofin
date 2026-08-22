//! `disable_extensions` — the benchmark's fairness switch, end to end.
//!
//! A benchmark leg has to compare like with like. The Jellyfin leg runs with no
//! plugins installed, so leaving Ferrofin's compiled-in extensions on means the
//! intro skipper's and merge-versions' scheduled tasks and event hooks fire
//! inside Ferrofin's measurement window and not inside Jellyfin's.
//!
//! This is an integration test because the switch lives in the composition
//! root: the config unit tests can only prove the value RESOLVES, not that
//! anything acts on it. Replacing the condition in `build_app_state` with
//! `false` left all 117 server unit tests green.

use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;

/// Boots the composition root with the switch in the given position and returns
/// how many plugin descriptors and scheduled tasks it ends up with.
async fn wired_counts(disable_extensions: bool) -> (usize, usize) {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config {
        disable_extensions,
        ..Config::test_stub(temp.path())
    };
    let db = ferrofin_db::Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        filters: Vec::new(),
        encoders: Vec::new(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");

    let plugins = wired
        .state
        .plugins
        .list_plugins()
        .await
        .expect("plugins")
        .len();
    let tasks = wired.state.tasks.get_tasks().await.expect("tasks").len();
    (plugins, tasks)
}

#[tokio::test]
async fn disabling_extensions_removes_their_plugins_and_tasks() {
    let (on_plugins, on_tasks) = wired_counts(false).await;
    let (off_plugins, off_tasks) = wired_counts(true).await;

    assert!(
        off_plugins < on_plugins,
        "disable_extensions must drop the compiled-in extensions from /Plugins \
         (on={on_plugins}, off={off_plugins})"
    );
    assert!(
        off_tasks < on_tasks,
        "disable_extensions must drop the extensions' scheduled tasks — those \
         are what fire inside a measurement window (on={on_tasks}, off={off_tasks})"
    );

    // Off is a subset, not an empty server: the built-in maintenance tasks and
    // any non-extension plugin registration must survive.
    assert!(
        off_tasks > 0,
        "the server's own scheduled tasks must remain, only the extensions' go"
    );
}
