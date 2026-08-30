//! The five metadata plugins Jellyfin ships **inside its own server tree**, as
//! Ferrofin plugin registrations.
//!
//! `MediaBrowser.Providers/Plugins/{Tmdb,Omdb,MusicBrainz,AudioDb,StudioImages}/
//! Plugin.cs` are each a `BasePlugin<PluginConfiguration>, IHasWebPages`
//! compiled into `MediaBrowser.Providers` — not external plugins, not optional.
//! Every stock Jellyfin therefore answers `GET /Plugins`,
//! `GET /Plugins/{id}/Configuration`, `GET /web/ConfigurationPages` and
//! `GET /web/ConfigurationPage?name=TMDb` with these five, and an admin has a
//! settings page for each.
//!
//! Ferrofin ports all five as native providers
//! (`tmdb`/`omdb`/`musicbrainz`/`audiodb`/`studios`) but gave none of them a
//! plugin *identity*, so all four of those endpoints came back empty or `404`
//! while Jellyfin returned five entries — the parity row's real content, and
//! the opposite of the "extension route not on the stock Jellyfin surface"
//! story that had been recorded against it.
//!
//! This module is plain data: the ids, names, descriptions, configuration file
//! names, default configuration JSON, and the **verbatim** `config.html` bytes
//! taken from the 10.11.8 tag (both projects are GPL-3.0-only). The composition
//! root turns each [`BuiltinPlugin`] into a `ferrofin_core::RegisteredPlugin`,
//! because `RegisteredPlugin` lives above this crate in the dependency graph.
//!
//! **Open work item — the configurations are stored and served but not yet
//! read.** `POST /Plugins/{id}/Configuration` persists them and the pages
//! render them, but the provider clients (`TmdbClient`, `OmdbClient`,
//! `MusicBrainzClient`, `AudioDbClient`, `StudiosClient`) are constructed in
//! `apps/ferrofin-server/src/state.rs` *before* the plugin manager exists, so
//! none of them holds a [`PluginManager`] to read through the way
//! [`OpenSubtitlesProvider`] does. Un-defer path: move the
//! `registered_plugins`/`FerrofinPluginManager`/`wasm_host` block above the
//! metadata-client block in `state.rs`, inject `Arc<dyn PluginManager>` into
//! the five clients, and read each setting at call time —
//! `IncludeAdult`/`MaxCastMembers`/`MaxCrewMembers`/`Hide*`/`ExcludeTags*`/
//! `ImportSeasonName` and the five image sizes for TMDb, `CastAndCrew` for
//! OMDb, `Server`/`RateLimit`/`ReplaceArtistName` for MusicBrainz,
//! `ReplaceAlbumName` for AudioDB, `RepositoryUrl` for Studio Images. Until
//! then the server-config equivalents Ferrofin already has
//! (`musicbrainz_base_url`, `omdb_api_key`, the studios repo URL) remain the
//! effective knobs.
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
}

/// TMDb — `MediaBrowser.Providers/Plugins/Tmdb/Plugin.cs`.
pub const TMDB: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xb871_5ed1_6c47_4528_9ad3_f72d_eb53_9cd4),
    name: "TMDb",
    description: "Get metadata for movies and other video content from TheMovieDb.",
    configuration_file_name: "Jellyfin.Plugin.Tmdb.xml",
    // `Tmdb/Configuration/PluginConfiguration.cs` defaults. The five image-size
    // properties are `string?` with no default, so upstream omits them.
    default_config: concat!(
        r#"{"TmdbApiKey":"","IncludeAdult":false,"ExcludeTagsSeries":false,"#,
        r#""ExcludeTagsMovies":false,"ImportSeasonName":false,"MaxCastMembers":15,"#,
        r#""MaxCrewMembers":15,"HideMissingCastMembers":false,"HideMissingCrewMembers":false}"#
    ),
    config_page: include_bytes!("../assets/plugins/tmdb.config.html"),
};

/// OMDb — `MediaBrowser.Providers/Plugins/Omdb/Plugin.cs`.
pub const OMDB: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xa628_c0da_fac5_4c7e_9d1a_7134_223f_14c8),
    name: "OMDb",
    description: "Get metadata for movies and other video content from OMDb.",
    configuration_file_name: "Jellyfin.Plugin.Omdb.xml",
    default_config: r#"{"CastAndCrew":false}"#,
    config_page: include_bytes!("../assets/plugins/omdb.config.html"),
};

/// MusicBrainz — `MediaBrowser.Providers/Plugins/MusicBrainz/Plugin.cs`.
pub const MUSICBRAINZ: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0x8c95_c4d2_e50c_4fb0_a4f3_6c06_ff0f_9a1a),
    name: "MusicBrainz",
    description: "Get artist and album metadata from any MusicBrainz server.",
    configuration_file_name: "Jellyfin.Plugin.MusicBrainz.xml",
    default_config: r#"{"Server":"https://musicbrainz.org","RateLimit":1,"ReplaceArtistName":false}"#,
    config_page: include_bytes!("../assets/plugins/musicbrainz.config.html"),
};

/// `AudioDB` — `MediaBrowser.Providers/Plugins/AudioDb/Plugin.cs`.
pub const AUDIODB: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0xa629_c0da_fac5_4c7e_931a_7174_223f_14c8),
    name: "AudioDB",
    description: "Get artist and album metadata or images from AudioDB.",
    configuration_file_name: "Jellyfin.Plugin.AudioDb.xml",
    default_config: r#"{"ReplaceAlbumName":false}"#,
    config_page: include_bytes!("../assets/plugins/audiodb.config.html"),
};

/// Studio Images — `MediaBrowser.Providers/Plugins/StudioImages/Plugin.cs`.
pub const STUDIO_IMAGES: BuiltinPlugin = BuiltinPlugin {
    id: Uuid::from_u128(0x872a_7849_1171_458d_a6fb_3de3_d442_ad30),
    name: "Studio Images",
    description: "Get artwork for studios from any Jellyfin-compatible repository.",
    configuration_file_name: "Jellyfin.Plugin.StudioImages.xml",
    default_config: r#"{"RepositoryUrl":"https://raw.githubusercontent.com/jellyfin/emby-artwork/master/studios"}"#,
    config_page: include_bytes!("../assets/plugins/studioimages.config.html"),
};

/// Every in-tree provider plugin, in the order Jellyfin's plugin manager
/// enumerates them (`GetExports<IPlugin>` reflection order, measured stable on
/// 10.11.8). `GET /web/ConfigurationPages` does not sort, so the order is part
/// of the response.
pub const ALL: &[BuiltinPlugin] = &[TMDB, STUDIO_IMAGES, OMDB, MUSICBRAINZ, AUDIODB];

#[cfg(test)]
mod tests {
    use super::{ALL, TMDB};

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
    }

    #[test]
    fn config_pages_are_the_vendored_bytes() {
        // The exact byte lengths of the 10.11.8 `Configuration/config.html`
        // resources, which the live server serves verbatim.
        let sizes: Vec<usize> = ALL.iter().map(|p| p.config_page.len()).collect();
        assert_eq!(sizes, vec![11182, 2491, 2179, 3828, 2244]);
        assert!(TMDB.config_page.starts_with(b"<!DOCTYPE html>"));
    }

    #[test]
    fn tmdb_defaults_match_the_csharp_plugin_configuration() {
        let v: serde_json::Value = serde_json::from_str(TMDB.default_config).expect("json");
        assert_eq!(v["MaxCastMembers"], 15);
        assert_eq!(v["MaxCrewMembers"], 15);
        assert_eq!(v["IncludeAdult"], false);
        assert_eq!(v["TmdbApiKey"], "");
        assert!(v.get("PosterSize").is_none(), "unset sizes are omitted");
    }
}
