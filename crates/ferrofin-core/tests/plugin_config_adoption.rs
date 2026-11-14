//! Adopting the settings of the metadata plugins Jellyfin builds in (gl-1574):
//! each `plugins/configurations/<assembly>.xml` comes across once into the
//! plugin's `config.json`, through a real plugin manager.

use std::path::Path;

use ferrofin_core::plugin_config_adoption::import_adopted_plugin_configs;
use ferrofin_core::{FerrofinPluginManager, RegisteredPlugin};
use ferrofin_providers::builtin_plugins::{ALL, BuiltinPlugin, MUSICBRAINZ, TMDB};
use ferrofin_traits::plugins::{PluginDescriptor, PluginManager};
use serde_json::{Value, json};

/// `Jellyfin.Plugin.Tmdb.xml` exactly as a stock Jellyfin 12.4 writes it at its
/// first start, then edited the way its settings page would: missing episodes
/// on for two libraries, a cast cap of 20, an API key of the admin's own.
/// `SimilarItemsCacheDays` is upstream's and has no Ferrofin setting.
const TMDB_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <TmdbApiKey>admins-own-key</TmdbApiKey>
  <IncludeAdult>false</IncludeAdult>
  <ExcludeTagsSeries>false</ExcludeTagsSeries>
  <ExcludeTagsMovies>false</ExcludeTagsMovies>
  <ImportSeasonName>false</ImportSeasonName>
  <ImportUnairedEpisodes>false</ImportUnairedEpisodes>
  <ImportMissingEpisodes>true</ImportMissingEpisodes>
  <ImportSpecials>false</ImportSpecials>
  <EnabledMissingEpisodeLibraries>
    <string>4f9e2c1a7b3d4e5f8a6b9c0d1e2f3a4b</string>
    <string>0123456789abcdef0123456789abcdef</string>
  </EnabledMissingEpisodeLibraries>
  <MissingEpisodeRefreshIntervalDays>7</MissingEpisodeRefreshIntervalDays>
  <UpcomingEpisodeGracePeriodDays>7</UpcomingEpisodeGracePeriodDays>
  <MaxCastMembers>20</MaxCastMembers>
  <MaxCrewMembers>15</MaxCrewMembers>
  <HideMissingCastMembers>false</HideMissingCastMembers>
  <HideMissingCrewMembers>false</HideMissingCrewMembers>
  <SimilarItemsCacheDays>90</SimilarItemsCacheDays>
</PluginConfiguration>"#;

/// `Jellyfin.Plugin.MusicBrainz.xml` as Jellyfin 12.4 writes it, pointed at a
/// self-hosted mirror with a faster rate.
const MUSICBRAINZ_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
  <Server>http://musicbrainz.lan:5000</Server>
  <RateLimit>0</RateLimit>
  <ReplaceArtistName>true</ReplaceArtistName>
</PluginConfiguration>"#;

/// A plugin manager holding the bundled `plugins`, as the composition root
/// registers them.
fn manager(state_dir: &Path, plugins: &[BuiltinPlugin]) -> FerrofinPluginManager {
    let registered = plugins
        .iter()
        .map(|p| {
            RegisteredPlugin::new(
                PluginDescriptor {
                    id: p.id,
                    name: p.name.to_owned(),
                    version: "12.4.0.0".to_owned(),
                    description: p.description.to_owned(),
                    enabled: true,
                    has_image: false,
                    can_uninstall: false,
                    configuration_file_name: Some(p.configuration_file_name.to_owned()),
                },
                None,
            )
            .with_default_config(p.default_config.as_bytes().to_vec())
        })
        .collect();
    FerrofinPluginManager::new(registered, state_dir.to_owned())
}

/// A data directory's `plugins/configurations` holding `documents`, and the
/// plugin manager's state directory beside it.
struct Adopted {
    dir: tempfile::TempDir,
}

impl Adopted {
    fn new(documents: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("configurations")).unwrap();
        for (name, xml) in documents {
            std::fs::write(dir.path().join("configurations").join(name), xml).unwrap();
        }
        Self { dir }
    }

    fn configurations(&self) -> std::path::PathBuf {
        self.dir.path().join("configurations")
    }

    fn state(&self) -> std::path::PathBuf {
        self.dir.path().join("state")
    }

    async fn import(&self, plugins: &FerrofinPluginManager) {
        let adopted = [TMDB, MUSICBRAINZ].map(|p| (p.id, p.configuration_file_name));
        import_adopted_plugin_configs(&self.configurations(), &self.state(), plugins, &adopted)
            .await;
    }
}

async fn configuration(plugins: &FerrofinPluginManager, plugin: &BuiltinPlugin) -> Value {
    serde_json::from_slice(&plugins.get_plugin_configuration(plugin.id).await.unwrap()).unwrap()
}

#[tokio::test]
async fn a_stock_12_4_document_edited_by_its_admin_comes_across() {
    let adopted = Adopted::new(&[
        (TMDB.configuration_file_name, TMDB_XML),
        (MUSICBRAINZ.configuration_file_name, MUSICBRAINZ_XML),
    ]);
    let plugins = manager(&adopted.state(), &[TMDB, MUSICBRAINZ]);

    adopted.import(&plugins).await;

    let tmdb = configuration(&plugins, &TMDB).await;
    assert_eq!(tmdb["TmdbApiKey"], "admins-own-key");
    assert_eq!(tmdb["ImportMissingEpisodes"], true);
    assert_eq!(tmdb["MaxCastMembers"], 20);
    assert_eq!(
        tmdb["EnabledMissingEpisodeLibraries"],
        json!([
            "4f9e2c1a7b3d4e5f8a6b9c0d1e2f3a4b",
            "0123456789abcdef0123456789abcdef"
        ])
    );
    // Upstream leaves a null image size out; it keeps Ferrofin's default.
    assert_eq!(tmdb.get("PosterSize"), Some(&Value::Null));
    assert!(tmdb.get("SimilarItemsCacheDays").is_none(), "{tmdb}");
    let musicbrainz = configuration(&plugins, &MUSICBRAINZ).await;
    assert_eq!(
        musicbrainz,
        json!({"Server": "http://musicbrainz.lan:5000", "RateLimit": 0, "ReplaceArtistName": true})
    );
    // The documents are left as they were.
    assert_eq!(
        std::fs::read_to_string(adopted.configurations().join(TMDB.configuration_file_name))
            .unwrap(),
        TMDB_XML
    );
}

/// For each bundled plugin, a document as `XmlSerializer` writes its
/// `PluginConfiguration` with a member or two set away from the default, and the
/// configuration it must become.
fn edited_documents() -> Vec<(BuiltinPlugin, String, Value)> {
    let document = |members: &str| {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<PluginConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">
{members}
</PluginConfiguration>"#
        )
    };
    ALL.iter()
        .map(|plugin| {
            let (members, expected) = match plugin.name {
                "TMDb" => (
                    // A size an admin chose, and a one-entry library list whose id
                    // happens to be all digits: still a string.
                    "  <PosterSize>w500</PosterSize>\n  <EnabledMissingEpisodeLibraries>\n    \
                     <string>12345678901234567890123456789012</string>\n  \
                     </EnabledMissingEpisodeLibraries>",
                    json!({"PosterSize": "w500", "EnabledMissingEpisodeLibraries": ["12345678901234567890123456789012"]}),
                ),
                "OMDb" => ("  <CastAndCrew>true</CastAndCrew>", json!({"CastAndCrew": true})),
                "MusicBrainz" => (
                    "  <Server>http://mb.lan</Server>\n  <RateLimit>3</RateLimit>",
                    json!({"Server": "http://mb.lan", "RateLimit": 3}),
                ),
                "AudioDB" => (
                    "  <ReplaceAlbumName>true</ReplaceAlbumName>",
                    json!({"ReplaceAlbumName": true}),
                ),
                "Studio Images" => (
                    "  <RepositoryUrl>https://art.example/studios</RepositoryUrl>",
                    json!({"RepositoryUrl": "https://art.example/studios"}),
                ),
                other => panic!("a bundled plugin with no test document: {other}"),
            };
            (*plugin, document(members), expected)
        })
        .collect()
}

#[tokio::test]
async fn every_bundled_plugins_document_comes_across() {
    let documents = edited_documents();
    let files: Vec<(&str, &str)> = documents
        .iter()
        .map(|(plugin, xml, _)| (plugin.configuration_file_name, xml.as_str()))
        .collect();
    let adopted = Adopted::new(&files);
    let plugins = manager(&adopted.state(), ALL);
    let all: Vec<_> = ALL
        .iter()
        .map(|p| (p.id, p.configuration_file_name))
        .collect();

    import_adopted_plugin_configs(&adopted.configurations(), &adopted.state(), &plugins, &all)
        .await;

    for (plugin, _, expected) in &documents {
        let stored = configuration(&plugins, plugin).await;
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&stored[key], value, "{} {key}", plugin.name);
        }
    }
    let tmdb = configuration(&plugins, &TMDB).await;
    assert_eq!(
        tmdb.get("BackdropSize"),
        Some(&Value::Null),
        "a size the document leaves out stays null"
    );
}

#[tokio::test]
async fn an_empty_or_absent_list_stays_empty() {
    let adopted = Adopted::new(&[(
        TMDB.configuration_file_name,
        "<PluginConfiguration><EnabledMissingEpisodeLibraries /></PluginConfiguration>",
    )]);
    let plugins = manager(&adopted.state(), &[TMDB]);

    adopted.import(&plugins).await;

    assert_eq!(
        configuration(&plugins, &TMDB).await["EnabledMissingEpisodeLibraries"],
        json!([])
    );
    let other = Adopted::new(&[(
        TMDB.configuration_file_name,
        "<PluginConfiguration><MaxCastMembers>4</MaxCastMembers></PluginConfiguration>",
    )]);
    let plugins = manager(&other.state(), &[TMDB]);
    other.import(&plugins).await;
    assert_eq!(
        configuration(&plugins, &TMDB).await["EnabledMissingEpisodeLibraries"],
        json!([])
    );
}

#[tokio::test]
async fn a_configuration_already_there_is_never_imported_over() {
    let adopted = Adopted::new(&[(TMDB.configuration_file_name, TMDB_XML)]);
    let plugins = manager(&adopted.state(), &[TMDB]);
    plugins
        .set_plugin_configuration(TMDB.id, br#"{"MaxCastMembers":3}"#.to_vec())
        .await
        .unwrap();

    adopted.import(&plugins).await;

    let tmdb = configuration(&plugins, &TMDB).await;
    assert_eq!(tmdb["MaxCastMembers"], 3);
    assert_eq!(tmdb["ImportMissingEpisodes"], false);
}

#[tokio::test]
async fn a_second_boot_does_not_import_again() {
    let adopted = Adopted::new(&[(TMDB.configuration_file_name, TMDB_XML)]);
    let plugins = manager(&adopted.state(), &[TMDB]);
    adopted.import(&plugins).await;
    // The admin changes a setting after the swap.
    plugins
        .set_plugin_configuration(TMDB.id, br#"{"MaxCastMembers":5}"#.to_vec())
        .await
        .unwrap();

    adopted.import(&plugins).await;

    assert_eq!(configuration(&plugins, &TMDB).await["MaxCastMembers"], 5);
}

#[tokio::test]
async fn no_document_an_unreadable_one_or_no_such_plugin_imports_nothing() {
    let adopted = Adopted::new(&[(
        MUSICBRAINZ.configuration_file_name,
        "<ServerConfiguration><Server>x</Server></ServerConfiguration>",
    )]);
    // TMDb has no document; MusicBrainz's has the wrong root; OMDb's is not
    // registered at all.
    std::fs::write(
        adopted.configurations().join("Jellyfin.Plugin.Omdb.xml"),
        "<PluginConfiguration><CastAndCrew>true</CastAndCrew></PluginConfiguration>",
    )
    .unwrap();
    let plugins = manager(&adopted.state(), &[TMDB, MUSICBRAINZ]);

    adopted.import(&plugins).await;
    import_adopted_plugin_configs(
        &adopted.configurations(),
        &adopted.state(),
        &plugins,
        &[(uuid::Uuid::from_u128(1), "Jellyfin.Plugin.Omdb.xml")],
    )
    .await;

    for id in [TMDB.id, MUSICBRAINZ.id, uuid::Uuid::from_u128(1)] {
        let file = adopted.state().join(id.to_string()).join("config.json");
        assert!(!file.exists(), "{} was written", file.display());
    }
    assert_eq!(
        configuration(&plugins, &MUSICBRAINZ).await,
        serde_json::from_str::<Value>(MUSICBRAINZ.default_config).unwrap()
    );
}
