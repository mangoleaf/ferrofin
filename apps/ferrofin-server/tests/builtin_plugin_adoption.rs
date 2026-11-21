//! The adopted settings of the metadata plugins Jellyfin builds in (gl-1574),
//! end to end through the composition root: a Jellyfin data directory's
//! `plugins/configurations/Jellyfin.Plugin.Tmdb.xml` is the TMDb plugin's
//! configuration once the server has wired itself.
//!
//! An integration test because the wiring lives in `build_app_state`: the
//! import's own tests pass it the paths and the plugin list, so dropping the
//! call, or handing it the wrong directory, would leave them all green.

use ferrofin_providers::builtin_plugins::TMDB;
use ferrofin_server::config::Config;
use ferrofin_server::state::build_app_state;

/// A stock Jellyfin 12.4 TMDb document, with missing episodes turned on.
const TMDB_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <TmdbApiKey />
  <ImportMissingEpisodes>true</ImportMissingEpisodes>
  <EnabledMissingEpisodeLibraries />
  <MaxCastMembers>25</MaxCastMembers>
  <SimilarItemsCacheDays>90</SimilarItemsCacheDays>
</PluginConfiguration>"#;

#[tokio::test]
async fn an_adopted_tmdb_document_is_the_plugins_configuration_at_boot() {
    let temp = tempfile::tempdir().expect("temp dir");
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.path().join(d)).expect("dir");
    }
    let config = Config::test_stub(temp.path());
    let configurations = config.data_dir.join("plugins").join("configurations");
    std::fs::create_dir_all(&configurations).expect("dir");
    std::fs::write(configurations.join(TMDB.configuration_file_name), TMDB_XML).expect("write");
    let db = ferrofin_db::Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: std::path::PathBuf::from("ffmpeg"),
        ffprobe: std::path::PathBuf::from("ffprobe"),
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (shutdown_tx, _rx) = tokio::sync::oneshot::channel();

    let wired = build_app_state(&db, &config, &ffmpeg, None, shutdown_tx)
        .await
        .expect("wire app state");

    let stored: serde_json::Value = serde_json::from_slice(
        &wired
            .state
            .plugins
            .get_plugin_configuration(TMDB.id)
            .await
            .expect("config"),
    )
    .expect("json");
    assert_eq!(stored["ImportMissingEpisodes"], true);
    assert_eq!(stored["MaxCastMembers"], 25);
    assert_eq!(
        stored["IncludeAdult"], false,
        "a member left out keeps its default"
    );
    assert!(
        config
            .config_dir
            .join("plugins")
            .join(TMDB.id.to_string())
            .join("config.json")
            .is_file()
    );
}

/// The Debian migration copies every bundled plugin's document, so a plugin
/// added to the table is not silently left behind by it.
#[test]
fn the_migration_script_copies_every_bundled_plugins_document() {
    let script = include_str!("../../../scripts/migrate-jellyfin.sh");
    for plugin in ferrofin_providers::builtin_plugins::ALL {
        assert!(
            script.contains(plugin.configuration_file_name),
            "scripts/migrate-jellyfin.sh does not copy {}",
            plugin.configuration_file_name
        );
    }
}
