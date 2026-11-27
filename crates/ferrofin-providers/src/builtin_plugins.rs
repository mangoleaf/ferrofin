//! The six metadata plugins Jellyfin ships **inside its own server tree**, as
//! Ferrofin plugin registrations.
//!
//! `MediaBrowser.Providers/Plugins/{Tmdb,Omdb,MusicBrainz,AudioDb,StudioImages}/
//! Plugin.cs` and `Plugins/ListenBrainz/ListenBrainzPlugin.cs` are each a
//! `BasePlugin<PluginConfiguration>, IHasWebPages` compiled into
//! `MediaBrowser.Providers` — not external plugins, not optional.
//! Every stock Jellyfin therefore answers `GET /Plugins`,
//! `GET /Plugins/{id}/Configuration`, `GET /web/ConfigurationPages` and
//! `GET /web/ConfigurationPage?name=TMDb` with these six, and an admin has a
//! settings page for each.
//!
//! Ferrofin ports all six as native providers
//! (`tmdb`/`omdb`/`musicbrainz`/`audiodb`/`studios`/`listenbrainz`) but gave
//! none of them a plugin *identity*, so all four of those endpoints came back
//! empty or `404` while Jellyfin returned their entries — the parity row's real
//! content, and the opposite of the "extension route not on the stock Jellyfin
//! surface" story that had been recorded against it.
//!
//! This module is plain data: the ids, names, descriptions, configuration file
//! names, default configuration JSON, and the **verbatim** `config.html` bytes
//! (and ListenBrainz's `NOTICE.md`) taken from the `UPSTREAM_TAG` tree (both
//! projects are GPL-3.0-only). The composition root turns each
//! [`BuiltinPlugin`] into a `ferrofin_traits::plugins::RegisteredPlugin`.
//!
//! **The configurations are live.** The composition root
//! (`apps/ferrofin-server/src/state.rs`) hands the plugin manager to
//! `TmdbClient`, `OmdbClient`, `MusicBrainzClient`, `AudioDbClient`,
//! `StudiosClient` and `ListenBrainzClient` through
//! [`ConfigSource`](crate::plugin_config::ConfigSource), and each reads its
//! settings at call time the way `Plugin.Instance.Configuration` is read
//! upstream — an admin's save on a settings page changes the next lookup, with
//! no restart. Wired settings, with the C# read site each was ported from:
//!
//! | plugin | setting | effect |
//! |---|---|---|
//! | TMDb | `TmdbApiKey` | replaces the built-in project key (`TmdbClientManager.cs:40`) |
//! | TMDb | `IncludeAdult` | `include_adult` on every search (`:396,424,467`) |
//! | TMDb | `MaxCastMembers` / `HideMissingCastMembers` | cast cap + profile-less filter (`TmdbMovieProvider.cs:258`, `TmdbSeriesProvider.cs:329`, `TmdbEpisodeProvider.cs:212`) |
//! | TMDb | `MaxCrewMembers` / `HideMissingCrewMembers` | crew cap + filter, after the wanted-kind filter (`TmdbMovieProvider.cs:300`) |
//! | TMDb | `PosterSize`/`BackdropSize`/`LogoSize`/`ProfileSize`/`StillSize` | the CDN size segment, `original` when unset (`TmdbClientManager.cs:514-590`) |
//! | TMDb | `ImportUnairedEpisodes` / `ImportMissingEpisodes` / `ImportSpecials` | which virtual episodes a series refresh creates (`TmdbMissingEpisodeProvider.FetchAsync`) |
//! | TMDb | `EnabledMissingEpisodeLibraries` | the libraries those run for (`IsEnabledForLibrary`) |
//! | TMDb | `UpcomingEpisodeGracePeriodDays` | how long a recently aired placeholder stays while missing episodes are off (`ShouldPrune`) |
//! | TMDb | `MissingEpisodeRefreshIntervalDays` | the `TmdbRefreshUpcomingEpisodes` task's interval (`TmdbUpcomingEpisodesTask.GetDefaultTriggers`) |
//! | TMDb | `ExcludeTagsMovies` / `ExcludeTagsSeries` | leave `keywords` out of the movie / series request, so TMDb's keywords never become Tags (`TmdbClientManager.cs:78-81,152-155`) |
//! | TMDb | `ImportSeasonName` | name a season after TMDb's season name (`TmdbSeasonProvider.cs:76`) |
//! | TMDb | `SimilarItemsCacheDays` | how long similar movies and series are cached, `0` none (`TmdbMovieSimilarProvider.CacheDuration`, `TmdbSeriesSimilarProvider.CacheDuration`) |
//! | OMDb | `CastAndCrew` | whether OMDb's director/writer/actors are imported (`OmdbProvider.cs:417`) |
//! | MusicBrainz | `Server` | the web-service base URL (`MusicBrainzArtistProvider.ReloadConfig`) |
//! | MusicBrainz | `RateLimit` | seconds between requests (same) |
//! | MusicBrainz | `ReplaceArtistName` | rename a searched artist to MusicBrainz's spelling (`:146`) |
//! | AudioDB | `ReplaceAlbumName` | rename an album to `strAlbum` (`AudioDbAlbumProvider.cs:90`) |
//! | Studio Images | `RepositoryUrl` | the artwork repository (`StudiosImageProvider.cs:190`) |
//! | ListenBrainz | `LabsServer` / `Algorithm` / `RateLimit` | the Labs server, its similarity algorithm and the seconds between requests (`ListenBrainzLabsClient.GetSimilarArtistsAsync`) |
//! | ListenBrainz | `SimilarItemsCacheDays` | how long similar artists are cached, `0` none (`ListenBrainzSimilarArtistProvider.CacheDuration`) |
//!
//! For `Server`, `RepositoryUrl` and the OMDb key, an operator who set the
//! pre-existing server config (`FERROFIN_MUSICBRAINZ_URL`,
//! `FERROFIN_STUDIOS_REPO_URL`, `FERROFIN_OMDB_KEY`) still wins over the
//! dashboard — an explicit env/file setting is the more specific instruction,
//! and those knobs shipped before these plugins had an identity.
//!
//! [`PluginManager`]: ferrofin_traits::plugins::PluginManager
//! [`OpenSubtitlesProvider`]: crate::opensubtitles::OpenSubtitlesProvider

use uuid::Uuid;

/// One of Jellyfin's in-tree provider plugins, as plain registration data.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinPlugin {
    /// The plugin's stable guid — the `Id` override on its `Plugin.cs`.
    pub id: Uuid,
    /// The display name, which is also the name its config page is fetched by
    /// (C# `GetPages()` yields one page named `Name`).
    pub name: &'static str,
    /// The `Description` override on its `Plugin.cs`.
    pub description: &'static str,
    /// The `ConfigurationFileName` override — the XML file the C# plugin
    /// persists into, reported verbatim on `GET /Plugins`.
    pub configuration_file_name: &'static str,
    /// The plugin's default configuration, as the JSON
    /// `GET /Plugins/{id}/Configuration` returns before an admin saves.
    pub default_config: &'static str,
    /// The `Configuration/config.html` embedded resource, byte-for-byte.
    pub config_page: &'static [u8],
    /// The `Configuration/NOTICE.md` embedded resource, byte-for-byte, which
    /// `GetPages()` yields as a second page named `Name + "Notice"`.
    pub notice: Option<&'static [u8]>,
    /// Adds the configuration's get-only members to every read of it
    /// (`RegisteredPlugin::config_view`).
    pub config_view: Option<fn(&mut serde_json::Map<String, serde_json::Value>)>,
}

/// TMDb — `MediaBrowser.Providers/Plugins/Tmdb/Plugin.cs`.
pub const TMDB: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xb871_5ed1_6c47_4528_9ad3_f72d_eb53_9cd4),
    name: "TMDb",
    description: "Get metadata for movies and other video content from TheMovieDb.",
    configuration_file_name: "Jellyfin.Plugin.Tmdb.xml",
    // `Tmdb/Configuration/PluginConfiguration.cs` defaults. The five image-size
    // properties are `string?` with no default, so they start `null`; they must
    // be in the document anyway, because a save keeps only the keys it holds
    // (`project_config`).
    default_config: concat!(
        r#"{"TmdbApiKey":"","IncludeAdult":false,"ExcludeTagsSeries":false,"#,
        r#""ExcludeTagsMovies":false,"ImportSeasonName":false,"MaxCastMembers":15,"#,
        r#""MaxCrewMembers":15,"HideMissingCastMembers":false,"HideMissingCrewMembers":false,"#,
        r#""PosterSize":null,"BackdropSize":null,"LogoSize":null,"ProfileSize":null,"#,
        r#""StillSize":null,"#,
        r#""ImportUnairedEpisodes":false,"ImportMissingEpisodes":false,"ImportSpecials":false,"#,
        r#""EnabledMissingEpisodeLibraries":[],"MissingEpisodeRefreshIntervalDays":7,"#,
        r#""UpcomingEpisodeGracePeriodDays":7,"SimilarItemsCacheDays":90}"#
    ),
    config_page: include_bytes!("../assets/plugins/tmdb.config.html"),
    notice: None,
    config_view: None,
};

/// OMDb — `MediaBrowser.Providers/Plugins/Omdb/Plugin.cs`.
pub const OMDB: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xa628_c0da_fac5_4c7e_9d1a_7134_223f_14c8),
    name: "OMDb",
    description: "Get metadata for movies and other video content from OMDb.",
    configuration_file_name: "Jellyfin.Plugin.Omdb.xml",
    default_config: r#"{"CastAndCrew":false}"#,
    config_page: include_bytes!("../assets/plugins/omdb.config.html"),
    notice: None,
    config_view: None,
};

/// MusicBrainz — `MediaBrowser.Providers/Plugins/MusicBrainz/Plugin.cs`.
pub const MUSICBRAINZ: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0x8c95_c4d2_e50c_4fb0_a4f3_6c06_ff0f_9a1a),
    name: "MusicBrainz",
    description: "Get artist and album metadata from any MusicBrainz server.",
    configuration_file_name: "Jellyfin.Plugin.MusicBrainz.xml",
    default_config: r#"{"Server":"https://musicbrainz.org","RateLimit":1,"ReplaceArtistName":false}"#,
    config_page: include_bytes!("../assets/plugins/musicbrainz.config.html"),
    notice: None,
    config_view: None,
};

/// `AudioDB` — `MediaBrowser.Providers/Plugins/AudioDb/Plugin.cs`.
pub const AUDIODB: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xa629_c0da_fac5_4c7e_931a_7174_223f_14c8),
    name: "AudioDB",
    description: "Get artist and album metadata or images from AudioDB.",
    configuration_file_name: "Jellyfin.Plugin.AudioDb.xml",
    default_config: r#"{"ReplaceAlbumName":false}"#,
    config_page: include_bytes!("../assets/plugins/audiodb.config.html"),
    notice: None,
    config_view: None,
};

/// Studio Images — `MediaBrowser.Providers/Plugins/StudioImages/Plugin.cs`.
pub const STUDIO_IMAGES: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0x872a_7849_1171_458d_a6fb_3de3_d442_ad30),
    name: "Studio Images",
    description: "Get artwork for studios from any Jellyfin-compatible repository.",
    configuration_file_name: "Jellyfin.Plugin.StudioImages.xml",
    default_config: r#"{"RepositoryUrl":"https://raw.githubusercontent.com/jellyfin/emby-artwork/master/studios"}"#,
    config_page: include_bytes!("../assets/plugins/studioimages.config.html"),
    notice: None,
    config_view: None,
};

/// ListenBrainz — `MediaBrowser.Providers/Plugins/ListenBrainz/ListenBrainzPlugin.cs`.
pub const LISTENBRAINZ: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xa5b2_e8c1_9d4f_4a3b_8c7e_6f1a_2b3c_4d5e),
    name: "ListenBrainz Similarity Provider",
    description: "Get similar artist recommendations from ListenBrainz Labs.",
    configuration_file_name: "Jellyfin.Plugin.ListenBrainz.xml",
    // `ListenBrainz/Configuration/PluginConfiguration.cs` defaults. The
    // get-only `AlgorithmString` is computed on every read instead, so a save
    // ignores it, as upstream's deserializer does.
    default_config: concat!(
        r#"{"LabsServer":"https://labs.api.listenbrainz.org","#,
        r#""Algorithm":"SessionBased1825Days","RateLimit":1,"SimilarItemsCacheDays":14}"#
    ),
    config_page: include_bytes!("../assets/plugins/listenbrainz.config.html"),
    notice: Some(include_bytes!("../assets/plugins/listenbrainz.NOTICE.md")),
    config_view: Some(crate::listenbrainz::add_algorithm_string),
};

/// Every in-tree provider plugin, in the order Jellyfin's plugin manager
/// enumerates them (`GetExports<IPlugin>` reflection order, measured stable on
/// 10.11.8, and with ListenBrainz on 12.4). `GET /web/ConfigurationPages` does
/// not sort, so the order is part of the response.
pub const ALL: &[BuiltinPlugin] = &[
    TMDB,
    STUDIO_IMAGES,
    OMDB,
    MUSICBRAINZ,
    LISTENBRAINZ,
    AUDIODB,
];

#[cfg(test)]
mod tests {
    use super::{ALL, LISTENBRAINZ, TMDB};

    #[test]
    fn every_default_config_is_valid_json() {
        for plugin in ALL {
            let value: serde_json::Value = serde_json::from_str(plugin.default_config)
                .unwrap_or_else(|e| panic!("{}: {e}", plugin.name));
            assert!(value.is_object(), "{}", plugin.name);
        }
    }

    #[test]
    fn ids_are_the_csharp_guids() {
        assert_eq!(TMDB.id.to_string(), "b8715ed1-6c47-4528-9ad3-f72deb539cd4");
        assert_eq!(
            super::STUDIO_IMAGES.id.to_string(),
            "872a7849-1171-458d-a6fb-3de3d442ad30"
        );
        assert_eq!(
            super::OMDB.id.to_string(),
            "a628c0da-fac5-4c7e-9d1a-7134223f14c8"
        );
        assert_eq!(
            super::MUSICBRAINZ.id.to_string(),
            "8c95c4d2-e50c-4fb0-a4f3-6c06ff0f9a1a"
        );
        assert_eq!(
            super::AUDIODB.id.to_string(),
            "a629c0da-fac5-4c7e-931a-7174223f14c8"
        );
        assert_eq!(
            LISTENBRAINZ.id.to_string(),
            "a5b2e8c1-9d4f-4a3b-8c7e-6f1a2b3c4d5e"
        );
    }

    #[test]
    fn config_pages_are_the_vendored_bytes() {
        // The exact byte lengths of the vendored `Configuration/config.html`
        // resources at v12.4, which the live server serves verbatim.
        let sizes: Vec<usize> = ALL.iter().map(|p| p.config_page.len()).collect();
        assert_eq!(sizes, vec![19135, 2491, 2179, 3828, 6708, 2244]);
        assert!(TMDB.config_page.starts_with(b"<!DOCTYPE html>"));
        let notices: Vec<Option<usize>> = ALL.iter().map(|p| p.notice.map(<[u8]>::len)).collect();
        assert_eq!(notices, vec![None, None, None, None, Some(1119), None]);
    }

    #[test]
    fn tmdb_defaults_match_the_csharp_plugin_configuration() {
        let v: serde_json::Value = serde_json::from_str(TMDB.default_config).expect("json");
        assert_eq!(v["MaxCastMembers"], 15);
        assert_eq!(v["MaxCrewMembers"], 15);
        assert_eq!(v["IncludeAdult"], false);
        assert_eq!(v["TmdbApiKey"], "");
        for size in [
            "PosterSize",
            "BackdropSize",
            "LogoSize",
            "ProfileSize",
            "StillSize",
        ] {
            assert!(
                v.get(size).is_some_and(serde_json::Value::is_null),
                "{size} is in the schema, unset"
            );
        }
        // `PluginConfiguration.cs:43-81`: the unaired/missing episode options.
        assert_eq!(v["ImportUnairedEpisodes"], false);
        assert_eq!(v["ImportMissingEpisodes"], false);
        assert_eq!(v["ImportSpecials"], false);
        assert_eq!(v["EnabledMissingEpisodeLibraries"], serde_json::json!([]));
        assert_eq!(v["MissingEpisodeRefreshIntervalDays"], 7);
        assert_eq!(v["UpcomingEpisodeGracePeriodDays"], 7);
        // `PluginConfiguration.cs:131`.
        assert_eq!(v["SimilarItemsCacheDays"], 90);
    }

    /// What a stock 12.4 answers `GET /Plugins/{id}/Configuration` with for
    /// ListenBrainz before a save, once the view has added `AlgorithmString`.
    #[test]
    fn listenbrainz_defaults_match_a_stock_12_4() {
        let mut config: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(LISTENBRAINZ.default_config).expect("json");
        (LISTENBRAINZ.config_view.expect("a view"))(&mut config);
        assert_eq!(
            serde_json::Value::Object(config),
            serde_json::json!({
                "LabsServer": "https://labs.api.listenbrainz.org",
                "Algorithm": "SessionBased1825Days",
                "RateLimit": 1,
                "SimilarItemsCacheDays": 14,
                "AlgorithmString": "session_based_days_1825_session_300_contribution_3_threshold_10_limit_100_filter_True_skip_30"
            })
        );
    }
}
