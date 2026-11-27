//! The plugin settings an export writes for Jellyfin (gl-1556, D7): the
//! settings XML of every plugin with a Jellyfin counterpart, in
//! `plugins/configurations/`, where the counterpart reads it.
//!
//! The counterparts are the six metadata plugins Jellyfin builds in, which
//! read their files at once, and LDAP-Auth, Intro Skipper, Merge Versions,
//! File Transformation and OpenSubtitles, which read theirs once installed.
//! Each one's members are its `PluginConfiguration`'s, at the release
//! `docs/PLUGINS_UPSTREAM.md` pins (the bundled six: `UPSTREAM_TAG`).
//!
//! The same table names the files adoption imports ([`adopted_documents`]).
//!
//! A file is written only for a plugin Ferrofin holds settings for: a saved
//! or imported `config.json`, or a server setting that outranks the plugin's
//! (MusicBrainz's server, Studio Images' repository). Otherwise a file the
//! source still holds (Jellyfin's, from before adoption, which Ferrofin could
//! not import) is kept as it is, and none is written: Ferrofin ran on the
//! plugin's defaults, which are the counterpart's too. A plugin with settings and no
//! counterpart (Single Sign-On, Ferrofin Settings, a WASM plugin) is reported.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ferrofin_core::config_export::{
    ConfigExport, PLUGIN_CONFIGURATIONS, PluginDocument, export_plugin,
};
use ferrofin_db::export::{IdentityLink, PathRewrite};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::config::Config;

/// A plugin with a Jellyfin counterpart, and how its settings are written.
struct Counterpart {
    /// Its plugin id, the counterpart's too.
    id: Uuid,
    /// How its file is written.
    document: PluginDocument,
}

/// `PluginConfiguration` of `MediaBrowser.Providers/Plugins/Tmdb` at `v12.4`.
const TMDB: &[&str] = &[
    "TmdbApiKey",
    "IncludeAdult",
    "ExcludeTagsSeries",
    "ExcludeTagsMovies",
    "ImportSeasonName",
    "ImportUnairedEpisodes",
    "ImportMissingEpisodes",
    "ImportSpecials",
    "EnabledMissingEpisodeLibraries",
    "MissingEpisodeRefreshIntervalDays",
    "UpcomingEpisodeGracePeriodDays",
    "MaxCastMembers",
    "MaxCrewMembers",
    "HideMissingCastMembers",
    "HideMissingCrewMembers",
    "PosterSize",
    "BackdropSize",
    "LogoSize",
    "ProfileSize",
    "StillSize",
    "SimilarItemsCacheDays",
];

/// `LDAP-Auth/Config/PluginConfiguration.cs` of `jellyfin-plugin-ldapauth` `v24`.
const LDAP_AUTH: &[&str] = &[
    "LdapUsers",
    "LdapServer",
    "LdapPort",
    "UseSsl",
    "UseStartTls",
    "SkipSslVerify",
    "LdapBindUser",
    "LdapBindPassword",
    "LdapBaseDn",
    "LdapSearchFilter",
    "LdapAdminBaseDn",
    "LdapAdminFilter",
    "EnableLdapAdminFilterMemberUid",
    "LdapSearchAttributes",
    "LdapClientCertPath",
    "LdapClientKeyPath",
    "LdapRootCaPath",
    "CreateUsersFromLdap",
    "AllowPassChange",
    "LdapUidAttribute",
    "LdapUsernameAttribute",
    "LdapPasswordAttribute",
    "EnableLdapProfileImageSync",
    "RemoveImagesNotInLdap",
    "LdapProfileImageAttribute",
    "LdapProfileImageFormat",
    "EnableAllFolders",
    "EnabledFolders",
    "PasswordResetUrl",
];

/// LDAP-Auth's settings that name files on the server: moved onto the layout
/// with the files the export copies, reported where they lie outside.
const LDAP_HOST_PATHS: [&str; 3] = ["LdapClientCertPath", "LdapClientKeyPath", "LdapRootCaPath"];

/// `IntroSkipper/Configuration/PluginConfiguration.cs` of `intro-skipper`
/// `db09359`, without the `[XmlIgnore]` `FileTransformationPluginEnabled`.
const INTRO_SKIPPER: &[&str] = &[
    "ExcludeSeries",
    "SeriesExclusions",
    "MovieExclusions",
    "PathExclusions",
    "AutoDetectIntros",
    "ReanalyzeSettledSeasons",
    "SettledSeasonDelayHours",
    "AnalyzeSeasonZero",
    "PreferChromaprint",
    "CacheFingerprints",
    "CacheCompressionLevel",
    "UseAlternativeBlackFrameAnalyzer",
    "RefineCreditsBoundary",
    "DetectNonBlackCredits",
    "UpdateMediaSegments",
    "ScanIntroduction",
    "ScanCredits",
    "ScanRecap",
    "ScanPreview",
    "ScanCommercial",
    "AnalysisPercent",
    "AnalysisLengthLimit",
    "FullLengthChapters",
    "EnableSponsorBlockChapterDetection",
    "SkipFirstEpisode",
    "SkipFirstEpisodeAnime",
    "AnimePreviewFromCreditsEnd",
    "MinimumIntroDuration",
    "MaximumIntroDuration",
    "MinimumCreditsDuration",
    "MaximumCreditsDuration",
    "MaximumMovieCreditsDuration",
    "ProbeAudioDuration",
    "MinimumRecapDuration",
    "MaximumRecapDuration",
    "MinimumRecapDetectionDuration",
    "MaximumRecapDetectionDuration",
    "DetectRecapUsingBlackFrames",
    "MinimumPreviewDuration",
    "MaximumPreviewDuration",
    "MinimumCommercialDuration",
    "MaximumCommercialDuration",
    "BlackFrameMinimumPercentage",
    "BlackFrameThreshold",
    "UseChapterMarkersBlackFrame",
    "AdjustIntroBasedOnChapters",
    "AdjustIntroBasedOnSilence",
    "SnapToKeyframe",
    "EndSnapThreshold",
    "AdjustWindowInward",
    "AdjustWindowOutward",
    "ChapterAnalyzerIntroductionPattern",
    "ChapterAnalyzerEndCreditsPattern",
    "ChapterAnalyzerPreviewPattern",
    "ChapterAnalyzerRecapPattern",
    "ChapterAnalyzerCommercialPattern",
    "IntroEndOffset",
    "IntroStartOffset",
    "MaximumFingerprintPointDifferences",
    "MaximumTimeSkip",
    "InvertedIndexShift",
    "SilenceDetectionMaximumNoise",
    "SilenceDetectionMinimumDuration",
    "MaxParallelism",
    "ProcessThreads",
    "ProcessPriority",
    "UseFileTransformationPlugin",
    "SkipbuttonHideDelay",
    "EnableMainMenu",
];

/// `PluginConfiguration` of `MediaBrowser.Providers/Plugins/ListenBrainz` at
/// `v12.4`, without the get-only `AlgorithmString`.
const LISTENBRAINZ: &[&str] = &[
    "LabsServer",
    "Algorithm",
    "RateLimit",
    "SimilarItemsCacheDays",
];

/// `SimilarityAlgorithm` of ListenBrainz at `v12.4`.
const LISTENBRAINZ_ALGORITHMS: &[&str] = &[
    "SessionBased1825Days",
    "SessionBased1800Days",
    "SessionBased7500Days",
    "SessionBased7500DaysHighContribution",
    "SessionBased9000Days",
    "SessionBased75Days",
];

/// `ProfileImageFormat` of LDAP-Auth `v24`.
const LDAP_IMAGE_FORMATS: &[&str] = &["Default", "Binary", "Base64", "Url"];

/// Every plugin with a Jellyfin counterpart.
#[allow(clippy::too_many_lines)] // one entry per plugin
fn counterparts() -> [Counterpart; 11] {
    use ferrofin_providers::builtin_plugins as builtin;
    let document = |file, members, entries, derived| PluginDocument {
        file,
        members,
        entries,
        derived,
        enums: &[],
    };
    [
        Counterpart {
            id: builtin::TMDB.id,
            document: document(builtin::TMDB.configuration_file_name, TMDB, &[], &[]),
        },
        Counterpart {
            id: builtin::OMDB.id,
            document: document(
                builtin::OMDB.configuration_file_name,
                &["CastAndCrew"],
                &[],
                &[],
            ),
        },
        Counterpart {
            id: builtin::MUSICBRAINZ.id,
            document: document(
                builtin::MUSICBRAINZ.configuration_file_name,
                &["Server", "RateLimit", "ReplaceArtistName"],
                &[],
                &[],
            ),
        },
        Counterpart {
            id: builtin::AUDIODB.id,
            document: document(
                builtin::AUDIODB.configuration_file_name,
                &["ReplaceAlbumName"],
                &[],
                &[],
            ),
        },
        Counterpart {
            id: builtin::STUDIO_IMAGES.id,
            document: document(
                builtin::STUDIO_IMAGES.configuration_file_name,
                &["RepositoryUrl"],
                &[],
                &[],
            ),
        },
        Counterpart {
            id: builtin::LISTENBRAINZ.id,
            document: PluginDocument {
                enums: &[("Algorithm", LISTENBRAINZ_ALGORITHMS)],
                ..document(
                    builtin::LISTENBRAINZ.configuration_file_name,
                    LISTENBRAINZ,
                    &[],
                    &[],
                )
            },
        },
        Counterpart {
            id: ferrofin_extensions::ldap_auth::EXTENSION_ID,
            document: PluginDocument {
                enums: &[("LdapProfileImageFormat", LDAP_IMAGE_FORMATS)],
                ..document(
                    ferrofin_extensions::ldap_auth::adoption::CONFIGURATION_FILE_NAME,
                    LDAP_AUTH,
                    &[("LdapUsers", "LdapUser")],
                    &[],
                )
            },
        },
        Counterpart {
            id: ferrofin_extensions::intro_skipper::EXTENSION_ID,
            document: PluginDocument {
                enums: &[
                    // .NET's `CompressionLevel`.
                    (
                        "CacheCompressionLevel",
                        &["Optimal", "Fastest", "NoCompression", "SmallestSize"],
                    ),
                    // .NET's `ProcessPriorityClass`, whose values are not its
                    // indexes; Ferrofin keeps it as a name, never a number.
                    (
                        "ProcessPriority",
                        &[
                            "Normal",
                            "Idle",
                            "High",
                            "RealTime",
                            "BelowNormal",
                            "AboveNormal",
                        ],
                    ),
                ],
                ..document(
                    ferrofin_extensions::intro_skipper::CONFIGURATION_FILE_NAME,
                    INTRO_SKIPPER,
                    &[],
                    &["FileTransformationPluginEnabled"],
                )
            },
        },
        Counterpart {
            id: ferrofin_extensions::merge_versions::EXTENSION_ID,
            document: document(
                ferrofin_extensions::merge_versions::CONFIGURATION_FILE_NAME,
                &["LocationsExcluded"],
                &[],
                &[],
            ),
        },
        Counterpart {
            id: ferrofin_extensions::file_transformation::EXTENSION_ID,
            document: PluginDocument {
                enums: &[("DebugLoggingState", &["Disabled", "Enabled"])],
                ..document(
                    ferrofin_extensions::file_transformation::CONFIGURATION_FILE_NAME,
                    &["DebugLoggingState", "Transformations"],
                    &[("Transformations", "PluginDefinedTransformation")],
                    &[],
                )
            },
        },
        Counterpart {
            id: ferrofin_providers::opensubtitles::PLUGIN_ID,
            document: document(
                ferrofin_providers::opensubtitles::CONFIGURATION_FILE_NAME,
                &["Username", "Password", "CredentialsInvalid"],
                &[],
                // Set by the plugin when a sign-in fails; it starts false.
                &["CredentialsInvalid"],
            ),
        },
    ]
}

/// The settings file of each plugin with a Jellyfin counterpart, by plugin id,
/// for adoption to import on the first start
/// (`ferrofin_core::plugin_config_adoption`): every counterpart but LDAP-Auth,
/// whose document also holds the uid map and has an import of its own
/// (`ferrofin_extensions::ldap_auth::adoption`).
#[must_use]
pub fn adopted_documents() -> Vec<(Uuid, &'static str)> {
    counterparts()
        .into_iter()
        .filter(|c| c.id != ferrofin_extensions::ldap_auth::EXTENSION_ID)
        .map(|c| (c.id, c.document.file))
        .collect()
}

/// Every plugin the server registers, as `(id, name, default settings)`.
fn registered() -> Vec<(Uuid, String, Vec<u8>)> {
    let mut plugins = vec![(
        ferrofin_providers::opensubtitles::PLUGIN_ID,
        "OpenSubtitles".to_owned(),
        crate::state::OPENSUBTITLES_DEFAULT_CONFIG.to_vec(),
    )];
    plugins.extend(ferrofin_providers::builtin_plugins::ALL.iter().map(|p| {
        (
            p.id,
            p.name.to_owned(),
            p.default_config.as_bytes().to_vec(),
        )
    }));
    plugins.extend(
        ferrofin_extensions::builtin_extensions()
            .iter()
            .map(|e| (e.id(), e.descriptor().name, e.default_config())),
    );
    plugins
}

/// What the plugin export wrote and left, for the loss report.
#[derive(Debug, Default)]
pub(super) struct PluginsExport {
    /// The files written, and the settings they leave out.
    pub(super) files: ConfigExport,
    /// The plugins written, by name, in [`counterparts`] order.
    pub(super) written: Vec<String>,
    /// The LDAP-Auth links written into its `LdapUsers`.
    pub(super) ldap_users: usize,
    /// The plugins whose file from before adoption was kept, by name.
    pub(super) kept: Vec<String>,
    /// Plugins with settings and no Jellyfin counterpart, by name.
    pub(super) without_counterpart: Vec<String>,
    /// LDAP-Auth's settings that name a file outside every directory the
    /// export moves, written as they are.
    pub(super) host_paths: Vec<&'static str>,
}

/// Exports the settings of `config`'s plugins with a counterpart, with
/// LDAP-Auth's uid map from `links` and its certificate paths moved by
/// `paths`; `out` is the output so far, whose copied plugin files tell which a
/// source kept.
///
/// # Errors
/// A plugin's settings or its directory cannot be read, or a value cannot be
/// written as XML.
pub(super) async fn export(
    config: &Config,
    links: &[IdentityLink],
    paths: &PathRewrite,
    out: &Path,
) -> anyhow::Result<PluginsExport> {
    let registered = registered();
    let state = config.config_dir.join("plugins");
    let mut export = PluginsExport::default();
    for counterpart in counterparts() {
        let Some((_, name, defaults)) = registered.iter().find(|(id, ..)| *id == counterpart.id)
        else {
            continue;
        };
        let defaults = object(defaults);
        let stored = settings_file(&state, counterpart.id);
        let mut settings = defaults.clone();
        let mut holds = match tokio::fs::read(&stored).await {
            // The server's own reading of a saved document
            // (`FerrofinPluginManager::get_plugin_configuration`).
            Ok(bytes) => {
                if let Ok(Value::Object(saved)) = serde_json::from_slice(&bytes) {
                    settings.extend(saved);
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(e).with_context(|| format!("could not read {}", stored.display()));
            }
        };
        holds |= outranked(config, counterpart.id, &mut settings);
        if counterpart.id == ferrofin_extensions::ldap_auth::EXTENSION_ID && holds {
            export.ldap_users = ldap_users(links, &mut settings);
            for setting in LDAP_HOST_PATHS {
                let Some(Value::String(path)) = settings.get_mut(setting) else {
                    continue;
                };
                match paths.moved(path) {
                    Some(moved) => *path = moved,
                    None if !path.is_empty() => export.host_paths.push(setting),
                    None => {}
                }
            }
        }
        let kept = out
            .join(PLUGIN_CONFIGURATIONS)
            .join(counterpart.document.file);
        if holds {
            export_plugin(
                &counterpart.document,
                &settings,
                &defaults,
                &mut export.files,
            )
            .with_context(|| format!("the {name} plugin's settings cannot be written"))?;
            export.written.push(name.clone());
        } else if tokio::fs::try_exists(&kept)
            .await
            .with_context(|| format!("could not check {}", kept.display()))?
        {
            export.kept.push(name.clone());
        }
    }
    export.without_counterpart = without_counterpart(&state, &registered).await?;
    Ok(export)
}

/// The settings files [`export`] writes as XML wherever one exists, so the
/// copy leaves them out (gl-1949): the `config.json` of each registered
/// plugin with a counterpart, in `config`'s plugin directory. A later adoption
/// by Ferrofin reads a plugin's XML only while that file is absent.
pub(super) fn exported_sources(config: &Config) -> Vec<PathBuf> {
    let (state, registered) = (config.config_dir.join("plugins"), registered());
    counterparts()
        .iter()
        .filter(|counterpart| registered.iter().any(|(id, ..)| *id == counterpart.id))
        .map(|counterpart| settings_file(&state, counterpart.id))
        .collect()
}

/// The settings file the plugin manager keeps for the plugin `id` in its
/// directory `state` (`FerrofinPluginManager::get_plugin_configuration`).
fn settings_file(state: &Path, id: Uuid) -> PathBuf {
    state.join(id.to_string()).join("config.json")
}

/// Applies the server settings that outrank a bundled plugin's own
/// (`builtin_plugins`: `FERROFIN_MUSICBRAINZ_URL`, `FERROFIN_STUDIOS_REPO_URL`),
/// so Jellyfin is given the value in effect. Whether one applied.
fn outranked(config: &Config, id: Uuid, settings: &mut Map<String, Value>) -> bool {
    use ferrofin_providers::builtin_plugins::{MUSICBRAINZ, STUDIO_IMAGES};
    let (setting, value) = match id {
        id if id == MUSICBRAINZ.id => ("Server", &config.musicbrainz_base_url),
        id if id == STUDIO_IMAGES.id => ("RepositoryUrl", &config.studios_repo_url),
        _ => return false,
    };
    if value.is_empty() {
        return false;
    }
    settings.insert(setting.to_owned(), Value::String(value.clone()));
    true
}

/// Fills LDAP-Auth's `LdapUsers` from its identity links (the import reversed,
/// `ldap_auth::adoption`): the user's id as `Guid` writes it, the uid and the
/// profile image hash. How many it wrote.
fn ldap_users(links: &[IdentityLink], settings: &mut Map<String, Value>) -> usize {
    let users: Vec<Value> = links
        .iter()
        .filter(|link| link.provider_id == ferrofin_extensions::ldap_auth::provider::PROVIDER_ID)
        .map(|link| {
            let user = Uuid::parse_str(&link.user_id).map_or_else(
                |_| link.user_id.to_ascii_lowercase(),
                |id| id.hyphenated().to_string(),
            );
            serde_json::json!({
                "LinkedJellyfinUserId": user,
                "LdapUid": link.subject,
                "ProfileImageHash": link.avatar_hash.clone().unwrap_or_default(),
            })
        })
        .collect();
    let count = users.len();
    settings.insert("LdapUsers".to_owned(), Value::Array(users));
    count
}

/// The plugins in `state` (the plugin manager's directory) with saved
/// settings and no counterpart, by name; an id no plugin registers is a WASM
/// plugin's. Sorted.
async fn without_counterpart(
    state: &Path,
    registered: &[(Uuid, String, Vec<u8>)],
) -> anyhow::Result<Vec<String>> {
    let counterparts: Vec<Uuid> = counterparts().iter().map(|c| c.id).collect();
    let mut found = Vec::new();
    let mut entries = match tokio::fs::read_dir(state).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e).with_context(|| format!("could not list {}", state.display())),
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("could not list {}", state.display()))?
    {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| Uuid::parse_str(n).ok())
        else {
            continue;
        };
        let mut saved = false;
        for file in ["config.json", "private.json"] {
            let file = entry.path().join(file);
            saved |= tokio::fs::try_exists(&file)
                .await
                .with_context(|| format!("could not check {}", file.display()))?;
        }
        if !saved || counterparts.contains(&id) {
            continue;
        }
        found.push(
            registered
                .iter()
                .find(|(known, ..)| *known == id)
                .map_or_else(
                    || format!("a WASM plugin ({id})"),
                    |(_, name, _)| name.clone(),
                ),
        );
    }
    found.sort();
    Ok(found)
}

/// `bytes` as a JSON object, or an empty one.
fn object(bytes: &[u8]) -> Map<String, Value> {
    match serde_json::from_slice(bytes) {
        Ok(Value::Object(fields)) => fields,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every counterpart's members are what Ferrofin keeps, but for the
    /// differences named here: the settings a Jellyfin export leaves out or
    /// reports. A setting added to a plugin fails here until it is mapped.
    #[test]
    fn each_counterpart_names_the_settings_ferrofin_keeps() {
        let registered = registered();
        for counterpart in counterparts() {
            let (_, name, defaults) = registered
                .iter()
                .find(|(id, ..)| *id == counterpart.id)
                .expect("registered");
            let keys = object(defaults);
            let document = counterpart.document;
            let only_ours: Vec<&str> = keys
                .keys()
                .map(String::as_str)
                .filter(|k| !document.members.contains(k) && !document.derived.contains(k))
                .collect();
            let only_theirs: Vec<&str> = document
                .members
                .iter()
                .copied()
                .filter(|m| !keys.contains_key(*m) && !document.derived.contains(m))
                .collect();
            let (ours, theirs): (&[&str], &[&str]) = match name.as_str() {
                // The uid map is kept in the identity store, and filled at export.
                "LDAP-Auth" => (&[], &["LdapUsers"]),
                // Ferrofin's own key, for its anonymous search.
                "OpenSubtitles" => (&["ApiKey"], &[]),
                _ => (&[], &[]),
            };
            assert_eq!(only_ours, ours, "{name}: settings Jellyfin's plugin lacks");
            assert_eq!(only_theirs, theirs, "{name}: settings Ferrofin lacks");
        }
    }

    /// TMDb's settings at Ferrofin's defaults are written as the document
    /// Jellyfin 12.4 wrote for its own (`testdata/Jellyfin.Plugin.Tmdb.xml`,
    /// from `jellyfin/jellyfin:12.4`): the same members and values; the image
    /// sizes, null in both, are left out by both.
    #[test]
    fn tmdb_settings_have_the_shape_jellyfin_writes() {
        use ferrofin_providers::builtin_plugins::TMDB;

        let export = export_defaults(TMDB.id, TMDB.default_config);
        assert_eq!(
            members(&export.files[0].contents),
            members(include_str!("testdata/Jellyfin.Plugin.Tmdb.xml"))
        );
        assert!(export.not_exported.is_empty(), "{:?}", export.not_exported);
    }

    /// ListenBrainz's settings are written as Jellyfin 12.4 writes its own
    /// (`testdata/Jellyfin.Plugin.ListenBrainz.xml`, saved through
    /// `POST /Plugins/{id}/Configuration` on `jellyfin/jellyfin:12.4`), in its
    /// order, with the algorithm by name however it was stored.
    #[test]
    fn listenbrainz_settings_have_the_shape_jellyfin_writes() {
        use ferrofin_providers::builtin_plugins::LISTENBRAINZ;

        for algorithm in [r#""sessionbased75days""#, "5", r#""5""#] {
            let saved = format!(
                r#"{{"LabsServer":"https://labs.example.org","Algorithm":{algorithm},"RateLimit":0.25,"SimilarItemsCacheDays":30}}"#
            );
            let export = export_defaults(LISTENBRAINZ.id, &saved);
            let theirs = include_str!("testdata/Jellyfin.Plugin.ListenBrainz.xml");
            assert_eq!(
                members(&export.files[0].contents),
                members(theirs),
                "{algorithm}"
            );
            let order = |xml: &str| -> Vec<String> {
                ferrofin_common::config_import::parse(xml)
                    .expect("parses")
                    .children
                    .into_iter()
                    .map(|c| c.name)
                    .collect()
            };
            assert_eq!(order(&export.files[0].contents), order(theirs));
            assert!(export.not_exported.is_empty(), "{:?}", export.not_exported);
        }
    }

    /// The export of the plugin `id`'s settings `saved`, over its registered
    /// defaults.
    fn export_defaults(id: Uuid, saved: &str) -> ConfigExport {
        let counterpart = counterparts()
            .into_iter()
            .find(|c| c.id == id)
            .expect("a counterpart");
        let registered = registered();
        let (_, _, defaults) = registered
            .iter()
            .find(|(i, ..)| *i == id)
            .expect("registered");
        let defaults = object(defaults);
        let mut settings = defaults.clone();
        settings.extend(object(saved.as_bytes()));
        let mut export = ConfigExport::default();
        export_plugin(&counterpart.document, &settings, &defaults, &mut export).expect("writes");
        export
    }

    /// A settings document's members, by name.
    fn members(xml: &str) -> std::collections::BTreeMap<String, String> {
        let root = ferrofin_common::config_import::parse(xml).expect("parses");
        assert_eq!(root.name, "PluginConfiguration");
        root.children
            .into_iter()
            .map(|c| (c.name, c.text))
            .collect()
    }

    /// A server setting outranks the plugin's own MusicBrainz server and
    /// Studio Images repository, as the providers read them; an empty one
    /// leaves the saved value.
    #[test]
    fn a_server_setting_outranks_the_plugins_own() {
        use ferrofin_providers::builtin_plugins::{MUSICBRAINZ, STUDIO_IMAGES, TMDB};

        let scratch = tempfile::tempdir().expect("tempdir");
        let mut config = Config::test_stub(scratch.path());
        config.musicbrainz_base_url = "https://mirror.example".to_owned();
        config.studios_repo_url = String::new();
        let mut settings = object(MUSICBRAINZ.default_config.as_bytes());
        assert!(outranked(&config, MUSICBRAINZ.id, &mut settings));
        assert_eq!(settings["Server"], "https://mirror.example");
        let mut settings = object(STUDIO_IMAGES.default_config.as_bytes());
        let saved = settings.clone();
        assert!(!outranked(&config, STUDIO_IMAGES.id, &mut settings));
        assert_eq!(settings, saved);
        assert!(!outranked(&config, TMDB.id, &mut settings));
    }

    /// MusicBrainz's settings are written with the server setting in effect,
    /// `Server` before `RateLimit`: upstream's `RateLimit` setter raises a
    /// value under 1 to 1 while `Server` is still the default, and
    /// `XmlSerializer` sets members in document order.
    #[tokio::test]
    async fn an_outranking_server_setting_is_written_before_the_rate_limit() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let mut config = Config::test_stub(scratch.path());
        config.musicbrainz_base_url = "https://mirror.example".to_owned();
        config.studios_repo_url = String::new();
        let state = config.config_dir.join("plugins");
        let id = ferrofin_providers::builtin_plugins::MUSICBRAINZ
            .id
            .to_string();
        std::fs::create_dir_all(state.join(&id)).expect("dir");
        std::fs::write(state.join(&id).join("config.json"), r#"{"RateLimit":0.5}"#).expect("write");
        let export = export(&config, &[], &PathRewrite::default(), scratch.path())
            .await
            .expect("exports");
        let file = export
            .files
            .files
            .iter()
            .find(|f| f.path.ends_with("Jellyfin.Plugin.MusicBrainz.xml"))
            .expect("written");
        let server = file
            .contents
            .find("<Server>https://mirror.example</Server>");
        let rate = file.contents.find("<RateLimit>0.5</RateLimit>");
        assert!(
            server.is_some() && rate.is_some() && server < rate,
            "{}",
            file.contents
        );
        assert_eq!(export.written, ["MusicBrainz"]);
    }

    /// LDAP-Auth's certificate under a directory the export copies moves with
    /// it; one outside stays and is reported.
    #[tokio::test]
    async fn ldap_certificate_paths_move_with_the_copy() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let config = Config::test_stub(scratch.path());
        let id = ferrofin_extensions::ldap_auth::EXTENSION_ID.to_string();
        let dir = config.config_dir.join("plugins").join(&id);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("config.json"),
            r#"{"LdapClientCertPath":"/srv/ff/certs/client.pem","LdapRootCaPath":"/etc/ssl/ca.pem"}"#,
        )
        .expect("write");
        let paths = PathRewrite {
            moves: vec![("/srv/ff".to_owned(), "/config".to_owned())],
            ..PathRewrite::default()
        };
        let export = export(&config, &[], &paths, scratch.path())
            .await
            .expect("exports");
        let ldap = export
            .files
            .files
            .iter()
            .find(|f| f.path.ends_with("LDAP-Auth.xml"))
            .expect("written");
        assert!(
            ldap.contents
                .contains("<LdapClientCertPath>/config/certs/client.pem</LdapClientCertPath>"),
            "moved"
        );
        assert!(
            ldap.contents
                .contains("<LdapRootCaPath>/etc/ssl/ca.pem</LdapRootCaPath>")
        );
        assert_eq!(export.host_paths, ["LdapRootCaPath"]);
    }

    /// The uid map takes the form `Guid` writes; a link of another provider
    /// is not LDAP-Auth's.
    #[test]
    fn ldap_users_are_the_ldap_links() {
        let link = |provider: &str, user: &str, subject: &str| IdentityLink {
            provider_id: provider.to_owned(),
            user_id: user.to_owned(),
            subject: subject.to_owned(),
            avatar_hash: None,
        };
        let mut settings = Map::new();
        let count = ldap_users(
            &[
                link(
                    ferrofin_extensions::ldap_auth::provider::PROVIDER_ID,
                    "6D0B2C3C-3B8C-4D5E-8F9A-0B1C2D3E4F50",
                    "ann",
                ),
                link(
                    "Ferrofin.Sso",
                    "6D0B2C3C-3B8C-4D5E-8F9A-0B1C2D3E4F51",
                    "bob",
                ),
            ],
            &mut settings,
        );
        assert_eq!(count, 1);
        assert_eq!(
            settings["LdapUsers"],
            serde_json::json!([{
                "LinkedJellyfinUserId": "6d0b2c3c-3b8c-4d5e-8f9a-0b1c2d3e4f50",
                "LdapUid": "ann",
                "ProfileImageHash": "",
            }])
        );
    }
}
