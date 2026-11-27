//! The adopted settings of the plugins with a Jellyfin counterpart (gl-1574,
//! gl-1935), end to end through the composition root: a Jellyfin data
//! directory's `plugins/configurations/<assembly>.xml` is that plugin's
//! configuration once the server has wired itself.
//!
//! An integration test because the wiring lives in `build_app_state`: the
//! import's own tests pass it the paths and the plugin list, so dropping the
//! call, or handing it the wrong directory or list, would leave them all green.

use ferrofin_providers::builtin_plugins::TMDB;
use ferrofin_server::config::Config;
use ferrofin_server::state::{WiredApp, build_app_state};
use uuid::Uuid;

/// A stock Jellyfin 12.4 TMDb document, with missing episodes turned on.
const TMDB_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <TmdbApiKey />
  <ImportMissingEpisodes>true</ImportMissingEpisodes>
  <EnabledMissingEpisodeLibraries />
  <MaxCastMembers>25</MaxCastMembers>
  <SimilarItemsCacheDays>90</SimilarItemsCacheDays>
</PluginConfiguration>"#;

/// A server wired over a fresh data directory whose
/// `plugins/configurations` holds `documents`, with its configuration.
async fn boot(temp: &std::path::Path, documents: &[(&str, &str)]) -> (Config, WiredApp) {
    for d in ["config", "data", "cache"] {
        std::fs::create_dir_all(temp.join(d)).expect("dir");
    }
    let config = Config::test_stub(temp);
    let configurations = config.data_dir.join("plugins").join("configurations");
    std::fs::create_dir_all(&configurations).expect("dir");
    for (name, xml) in documents {
        std::fs::write(configurations.join(name), xml).expect("write");
    }
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
    (config, wired)
}

/// The configuration the plugin `id` serves once wired.
async fn configuration(wired: &WiredApp, id: Uuid) -> serde_json::Value {
    serde_json::from_slice(
        &wired
            .state
            .plugins
            .get_plugin_configuration(id)
            .await
            .expect("config"),
    )
    .expect("json")
}

#[tokio::test]
async fn an_adopted_tmdb_document_is_the_plugins_configuration_at_boot() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (config, wired) = boot(temp.path(), &[(TMDB.configuration_file_name, TMDB_XML)]).await;

    let stored = configuration(&wired, TMDB.id).await;
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

/// `IntroSkipper.xml` as Intro Skipper `db09359` writes it (trimmed): its
/// `ExclusionList`s as `<string>` entries, enums by name, a double.
const INTRO_SKIPPER_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <ExcludeSeries />
  <SeriesExclusions>
    <string>Late Night, Live</string>
    <string>1999</string>
  </SeriesExclusions>
  <MovieExclusions />
  <PathExclusions>
    <string>/media/tv/kids</string>
  </PathExclusions>
  <AutoDetectIntros>false</AutoDetectIntros>
  <CacheCompressionLevel>SmallestSize</CacheCompressionLevel>
  <MaximumIntroDuration>90</MaximumIntroDuration>
  <EndSnapThreshold>1.5</EndSnapThreshold>
  <ProcessPriority>Idle</ProcessPriority>
</PluginConfiguration>"#;

/// `Jellyfin.Plugin.MergeVersions.xml` as Merge Versions `e6f58d6` writes it.
const MERGE_VERSIONS_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <LocationsExcluded>
    <string>/media/movies/kept-apart</string>
  </LocationsExcluded>
</PluginConfiguration>"#;

/// `Jellyfin.Plugin.FileTransformation.xml` as File Transformation `f4f01c3`
/// writes it, with one transformation from its settings page.
const FILE_TRANSFORMATION_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <DebugLoggingState>Enabled</DebugLoggingState>
  <Transformations>
    <PluginDefinedTransformation>
      <Id>0f8fad5b-d9cb-469f-a165-70867728950e</Id>
      <FilenamePattern>index.html</FilenamePattern>
      <SearchText>&lt;title&gt;Jellyfin</SearchText>
      <ReplaceText>&lt;title&gt;Basement</ReplaceText>
    </PluginDefinedTransformation>
  </Transformations>
</PluginConfiguration>"#;

/// `Jellyfin.Plugin.OpenSubtitles.xml` as OpenSubtitles `23d00cb` writes it
/// after a refused sign-in. `CredentialsInvalid` has no Ferrofin member (the
/// refusal is kept per process), and Ferrofin's `ApiKey` no upstream one.
const OPENSUBTITLES_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <Username>basement</Username>
  <Password>hunter2</Password>
  <CredentialsInvalid>true</CredentialsInvalid>
</PluginConfiguration>"#;

#[tokio::test]
async fn the_compiled_in_counterparts_documents_are_their_configuration_at_boot() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (_config, wired) = boot(
        temp.path(),
        &[
            ("IntroSkipper.xml", INTRO_SKIPPER_XML),
            ("Jellyfin.Plugin.MergeVersions.xml", MERGE_VERSIONS_XML),
            (
                "Jellyfin.Plugin.FileTransformation.xml",
                FILE_TRANSFORMATION_XML,
            ),
            ("Jellyfin.Plugin.OpenSubtitles.xml", OPENSUBTITLES_XML),
        ],
    )
    .await;

    let intro = configuration(&wired, ferrofin_extensions::intro_skipper::EXTENSION_ID).await;
    assert_eq!(
        intro["SeriesExclusions"],
        serde_json::json!(["Late Night, Live", "1999"])
    );
    assert_eq!(intro["MovieExclusions"], serde_json::json!([]));
    assert_eq!(
        intro["PathExclusions"],
        serde_json::json!(["/media/tv/kids"])
    );
    assert_eq!(intro["AutoDetectIntros"], false);
    assert_eq!(intro["CacheCompressionLevel"], "SmallestSize");
    assert_eq!(intro["MaximumIntroDuration"], 90);
    assert_eq!(intro["EndSnapThreshold"], 1.5);
    assert_eq!(intro["ProcessPriority"], "Idle");
    assert_eq!(
        intro["ScanCredits"], true,
        "a member left out keeps its default"
    );

    let merge = configuration(&wired, ferrofin_extensions::merge_versions::EXTENSION_ID).await;
    assert_eq!(
        merge["LocationsExcluded"],
        serde_json::json!(["/media/movies/kept-apart"])
    );

    let transformation = configuration(
        &wired,
        ferrofin_extensions::file_transformation::EXTENSION_ID,
    )
    .await;
    assert_eq!(
        transformation,
        serde_json::json!({
            "DebugLoggingState": "Enabled",
            "Transformations": [{
                "Id": "0f8fad5b-d9cb-469f-a165-70867728950e",
                "FilenamePattern": "index.html",
                "SearchText": "<title>Jellyfin",
                "ReplaceText": "<title>Basement"
            }]
        })
    );

    let subtitles = configuration(&wired, ferrofin_providers::opensubtitles::PLUGIN_ID).await;
    assert_eq!(
        subtitles,
        serde_json::json!({"ApiKey": "", "Username": "basement", "Password": "hunter2"})
    );
}

/// The ListenBrainz plugin as a stock 12.4 serves it once its admin saved the
/// settings page (`Jellyfin.Plugin.ListenBrainz.xml` from `jellyfin/jellyfin:12.4`):
/// the document comes across, a read adds the get-only `AlgorithmString`, and
/// the dashboard lists its two pages where Jellyfin does, the notice as
/// `text/markdown`.
#[tokio::test]
async fn an_adopted_listenbrainz_document_is_served_with_its_pages() {
    use ferrofin_providers::builtin_plugins::LISTENBRAINZ;

    let xml = include_str!("../src/export_cmd/testdata/Jellyfin.Plugin.ListenBrainz.xml");
    let temp = tempfile::tempdir().expect("temp dir");
    let (_config, wired) = boot(temp.path(), &[(LISTENBRAINZ.configuration_file_name, xml)]).await;

    assert_eq!(
        configuration(&wired, LISTENBRAINZ.id).await,
        serde_json::json!({
            "LabsServer": "https://labs.example.org",
            "Algorithm": "SessionBased75Days",
            "RateLimit": 0.25,
            "SimilarItemsCacheDays": 30,
            "AlgorithmString": "session_based_days_75_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30"
        })
    );
    let plugins = &wired.state.plugins;
    let pages: Vec<String> = plugins
        .get_configuration_pages()
        .await
        .expect("pages")
        .into_iter()
        .filter(|page| {
            ferrofin_providers::builtin_plugins::ALL
                .iter()
                .any(|p| Some(p.id) == page.plugin_id)
        })
        .map(|page| page.name)
        .collect();
    assert_eq!(
        pages,
        [
            "TMDb",
            "Studio Images",
            "OMDb",
            "MusicBrainz",
            "ListenBrainz Similarity Provider",
            "ListenBrainz Similarity ProviderNotice",
            "AudioDB"
        ]
    );
    let notice = plugins
        .get_configuration_page("listenbrainz similarity providernotice")
        .await
        .expect("lookup")
        .expect("the notice page");
    assert_eq!(notice.content_type, Some("text/markdown"));
    assert!(notice.bytes.starts_with(b"# ListenBrainz logo attribution"));
}

/// Adoption imports every bundled plugin's document, and the Debian migration
/// copies every document adoption imports, so a plugin added to either table
/// is not silently left behind.
#[test]
fn the_migration_script_copies_every_document_adoption_imports() {
    let adopted = ferrofin_server::export_cmd::plugins::adopted_documents();
    // LDAP-Auth's document also holds the uid map, which only its own import reads.
    assert!(
        !adopted
            .iter()
            .any(|(id, _)| *id == ferrofin_extensions::ldap_auth::EXTENSION_ID)
    );
    for plugin in ferrofin_providers::builtin_plugins::ALL {
        assert!(
            adopted.contains(&(plugin.id, plugin.configuration_file_name)),
            "adoption does not import {}",
            plugin.configuration_file_name
        );
    }
    let script = include_str!("../../../scripts/migrate-jellyfin.sh");
    for (_, file) in adopted {
        assert!(
            script.contains(file),
            "scripts/migrate-jellyfin.sh does not copy {file}"
        );
    }
}

/// `/Plugins` reports each counterpart's settings file, as upstream's
/// `BasePlugin<T>.GetPluginInfo` does (`BasePluginOfT.cs:179`): the file
/// adoption imports and the export writes.
#[tokio::test]
async fn each_counterpart_reports_its_settings_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let (_config, wired) = boot(temp.path(), &[]).await;
    let mut documents = ferrofin_server::export_cmd::plugins::adopted_documents();
    documents.push((
        ferrofin_extensions::ldap_auth::EXTENSION_ID,
        ferrofin_extensions::ldap_auth::adoption::CONFIGURATION_FILE_NAME,
    ));
    for (id, file) in documents {
        let plugin = wired
            .state
            .plugins
            .get_plugin(id)
            .await
            .expect("lookup")
            .unwrap_or_else(|| panic!("{file}: no plugin {id}"));
        assert_eq!(
            plugin.configuration_file_name.as_deref(),
            Some(file),
            "{id}"
        );
    }
}
