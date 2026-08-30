//! Late-bound access to the dashboard configuration of Jellyfin's five in-tree
//! provider plugins.
//!
//! `MediaBrowser.Providers/Plugins/{Tmdb,Omdb,MusicBrainz,AudioDb,StudioImages}`
//! each read their settings through the static `Plugin.Instance.Configuration`
//! at call time, so an admin's save on the plugin's settings page takes effect
//! on the next lookup. Ferrofin's equivalent is
//! [`PluginManager::get_plugin_configuration`], but the composition root builds
//! the metadata clients long before the plugin manager exists — so each client
//! holds one of these, and the composition root [`attach`](ConfigSource::attach)es
//! the manager to it once the manager is built.
//!
//! Reads happen at call time, not at construction: the config file is a few
//! hundred bytes and every caller is already on a network round trip, which is
//! the same trade [`OpenSubtitlesProvider`] makes. A manager that was never
//! attached, an unreadable file, or a body that will not deserialize all yield
//! the C# defaults — a provider must never fail a lookup because a settings
//! page was saved wrong.
//!
//! [`PluginManager::get_plugin_configuration`]: ferrofin_traits::plugins::PluginManager::get_plugin_configuration
//! [`OpenSubtitlesProvider`]: crate::opensubtitles::OpenSubtitlesProvider

use std::sync::{Arc, OnceLock};

use ferrofin_traits::plugins::PluginManager;
use serde::Deserialize;
use uuid::Uuid;

/// The plugin manager a metadata client reads its dashboard settings through,
/// bound after construction.
#[derive(Default)]
pub struct ConfigSource {
    manager: OnceLock<Arc<dyn PluginManager>>,
}

impl std::fmt::Debug for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigSource")
            .field("attached", &self.manager.get().is_some())
            .finish()
    }
}

impl ConfigSource {
    /// An unbound source: every read returns the C# defaults until
    /// [`attach`](Self::attach) is called.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the plugin manager. Idempotent — a second call is ignored, so a
    /// client that has already answered a request cannot have its settings
    /// source swapped underneath it.
    pub fn attach(&self, manager: Arc<dyn PluginManager>) {
        let _ = self.manager.set(manager);
    }

    /// Whether a manager has been bound (the dashboard settings are live).
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.manager.get().is_some()
    }

    /// The plugin's configuration, deserialized into `T`.
    ///
    /// Falls back to `T::default()` — which every config type in this module
    /// defines as the C# `PluginConfiguration` defaults — when no manager is
    /// bound, the read fails, or the stored JSON does not fit `T`. The manager
    /// already overlays a partial saved config onto the plugin's registered
    /// defaults, so a body saved by an older dashboard still carries every key.
    pub async fn load<T: serde::de::DeserializeOwned + Default>(&self, plugin_id: Uuid) -> T {
        let Some(manager) = self.manager.get() else {
            return T::default();
        };
        match manager.get_plugin_configuration(plugin_id).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(plugin = %plugin_id, error = %e,
                               "plugin configuration is unreadable; using defaults");
                T::default()
            }),
            Err(e) => {
                tracing::warn!(plugin = %plugin_id, error = %e,
                               "plugin configuration read failed; using defaults");
                T::default()
            }
        }
    }
}

/// `Tmdb/Configuration/PluginConfiguration.cs`.
// Five bools, because the C# `PluginConfiguration` has five checkboxes and the
// dashboard page saves them by name. Collapsing them into enums would break the
// wire shape this type exists to mirror.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TmdbConfig {
    /// A personal API key; empty uses the built-in project key
    /// (`TmdbClientManager` ctor: `IsNullOrEmpty(apiKey) ? TmdbUtils.ApiKey : apiKey`).
    pub tmdb_api_key: String,
    /// Include adult titles in searches (`SearchMovieAsync(includeAdult:)`).
    pub include_adult: bool,
    /// Drop `keywords` from the series append-to-response
    /// (`TmdbClientManager.cs:142`), so TMDb keywords stop becoming tags.
    pub exclude_tags_series: bool,
    /// Drop `keywords` from the movie append-to-response (`:68`).
    pub exclude_tags_movies: bool,
    /// Overwrite a season's name with TMDb's (`TmdbSeasonProvider.cs:73-76`).
    pub import_season_name: bool,
    /// Cap on cast members imported per item (C# default 15).
    pub max_cast_members: usize,
    /// Cap on (wanted-kind) crew members imported per item (C# default 15).
    pub max_crew_members: usize,
    /// Skip cast members with no profile image.
    pub hide_missing_cast_members: bool,
    /// Skip crew members with no profile image.
    pub hide_missing_crew_members: bool,
    /// The image CDN size segment for posters; empty/absent means `original`
    /// (`TmdbClientManager.GetUrl`).
    pub poster_size: Option<String>,
    /// The image CDN size segment for backdrops.
    pub backdrop_size: Option<String>,
    /// The image CDN size segment for logos.
    pub logo_size: Option<String>,
    /// The image CDN size segment for person profiles.
    pub profile_size: Option<String>,
    /// The image CDN size segment for episode stills.
    pub still_size: Option<String>,
}

impl Default for TmdbConfig {
    fn default() -> Self {
        Self {
            tmdb_api_key: String::new(),
            include_adult: false,
            exclude_tags_series: false,
            exclude_tags_movies: false,
            import_season_name: false,
            // `PluginConfiguration.cs:39,44` — the only non-falsy C# defaults.
            max_cast_members: 15,
            max_crew_members: 15,
            hide_missing_cast_members: false,
            hide_missing_crew_members: false,
            poster_size: None,
            backdrop_size: None,
            logo_size: None,
            profile_size: None,
            still_size: None,
        }
    }
}

/// Which TMDb image-size setting governs a URL.
///
/// `TmdbClientManager` has one `GetUrl(size, path)` and five callers, each
/// passing a different `Plugin.Instance.Configuration.*Size`
/// (v10.11.8 `TmdbClientManager.cs:530-590`). This is that fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbImageKind {
    /// `PosterSize` — `GetPosterUrl` / `ConvertPostersToRemoteImageInfo`.
    Poster,
    /// `BackdropSize` — `ConvertBackdropsToRemoteImageInfo`.
    Backdrop,
    /// `LogoSize` — `ConvertLogosToRemoteImageInfo`.
    Logo,
    /// `ProfileSize` — `GetProfileUrl` / `ConvertProfilesToRemoteImageInfo`.
    Profile,
    /// `StillSize` — `ConvertStillsToRemoteImageInfo`.
    Still,
}

/// The TMDb image CDN root; the size segment and the file path follow.
const TMDB_IMAGE_ROOT: &str = "https://image.tmdb.org/t/p";

impl TmdbConfig {
    /// The API key to send: the dashboard's when set, else the caller's
    /// built-in one.
    ///
    /// `TmdbClientManager`'s constructor is
    /// `apiKey = string.IsNullOrEmpty(apiKey) ? TmdbUtils.ApiKey : apiKey`
    /// (`:40-42`) — the personal key wins, an empty one means "use the
    /// project key".
    #[must_use]
    pub fn api_key<'a>(&'a self, builtin: &'a str) -> &'a str {
        if self.tmdb_api_key.trim().is_empty() {
            builtin
        } else {
            self.tmdb_api_key.trim()
        }
    }

    /// The configured size segment for `kind`, or `None` for "unset".
    #[must_use]
    pub fn image_size(&self, kind: TmdbImageKind) -> Option<&str> {
        let raw = match kind {
            TmdbImageKind::Poster => &self.poster_size,
            TmdbImageKind::Backdrop => &self.backdrop_size,
            TmdbImageKind::Logo => &self.logo_size,
            TmdbImageKind::Profile => &self.profile_size,
            TmdbImageKind::Still => &self.still_size,
        };
        raw.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// The absolute CDN URL for a TMDb-relative image `path`.
    ///
    /// `GetUrl` (`TmdbClientManager.cs:514-521`): *"Use `original` as default
    /// size if size is null or empty to prevent malformed URLs"*. `path`
    /// already begins with `/`.
    #[must_use]
    pub fn image_url(&self, kind: TmdbImageKind, path: &str) -> String {
        let size = self.image_size(kind).unwrap_or("original");
        format!("{TMDB_IMAGE_ROOT}/{size}{path}")
    }
}

/// `Omdb/Configuration/PluginConfiguration.cs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct OmdbConfig {
    /// Import OMDb's Director/Writer/Actors as people
    /// (`OmdbProvider.cs:417` — off, the C# default, returns before adding any).
    pub cast_and_crew: bool,
}

/// `MusicBrainz/Configuration/PluginConfiguration.cs`.
///
/// Both properties have normalizing setters upstream, applied here by
/// [`MusicBrainzConfig::normalized`] because a JSON body reaches the fields
/// directly.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MusicBrainzConfig {
    /// The MusicBrainz server base URL (`ReloadConfig`: an unparseable value
    /// falls back to the official server with a warning).
    pub server: String,
    /// Seconds between requests to that server (C# default 1).
    pub rate_limit: f64,
    /// Overwrite a matched artist's name with MusicBrainz's
    /// (`MusicBrainzArtistProvider.cs:146`).
    pub replace_artist_name: bool,
}

/// `PluginConfiguration.DefaultServer`.
pub const MUSICBRAINZ_DEFAULT_SERVER: &str = "https://musicbrainz.org";

/// `PluginConfiguration.DefaultRateLimit`.
pub const MUSICBRAINZ_DEFAULT_RATE_LIMIT: f64 = 1.0;

impl Default for MusicBrainzConfig {
    fn default() -> Self {
        Self {
            server: MUSICBRAINZ_DEFAULT_SERVER.to_owned(),
            rate_limit: MUSICBRAINZ_DEFAULT_RATE_LIMIT,
            replace_artist_name: false,
        }
    }
}

impl MusicBrainzConfig {
    /// Applies the C# property setters, which a deserialized body bypasses.
    ///
    /// `Server`'s setter is `value.TrimEnd('/')`; `RateLimit`'s refuses to go
    /// below 1 req/sec **while the official server is selected** — that is
    /// musicbrainz.org's published limit, and hammering it is how an instance
    /// gets blocked. A mirror may be polled faster. An empty/blank server falls
    /// back to the default, matching `ReloadConfig`'s "invalid server specified,
    /// falling back to official server" branch.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.server = self.server.trim().trim_end_matches('/').to_owned();
        if self.server.is_empty() {
            MUSICBRAINZ_DEFAULT_SERVER.clone_into(&mut self.server);
        }
        if self.rate_limit < MUSICBRAINZ_DEFAULT_RATE_LIMIT
            && self.server == MUSICBRAINZ_DEFAULT_SERVER
        {
            self.rate_limit = MUSICBRAINZ_DEFAULT_RATE_LIMIT;
        }
        self
    }
}

/// `AudioDb/Configuration/PluginConfiguration.cs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AudioDbConfig {
    /// Overwrite a matched album's name with `strAlbum`
    /// (`AudioDbAlbumProvider.cs:90`).
    pub replace_album_name: bool,
}

/// `StudioImages/Configuration/PluginConfiguration.cs`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct StudioImagesConfig {
    /// The artwork repository base URL (`StudiosImageProvider.cs:190`).
    pub repository_url: String,
}

impl Default for StudioImagesConfig {
    fn default() -> Self {
        Self {
            repository_url: crate::studios::DEFAULT_REPO_URL.to_owned(),
        }
    }
}

/// A stub [`PluginManager`] serving one plugin's stored configuration, so a
/// client's settings can be exercised without a composition root.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::sync::Arc;

    use async_trait::async_trait;
    use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
    use ferrofin_traits::ServiceError;
    use ferrofin_traits::plugins::{PluginDescriptor, PluginImage, PluginManager};
    use uuid::Uuid;

    struct OnePlugin {
        id: Uuid,
        config: Vec<u8>,
    }

    #[async_trait]
    impl PluginManager for OnePlugin {
        async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_plugin(&self, _id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
            Ok(None)
        }
        async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError> {
            if id == self.id {
                Ok(self.config.clone())
            } else {
                Err(ServiceError::not_found(format!("plugin {id}")))
            }
        }
        async fn set_plugin_configuration(
            &self,
            _id: Uuid,
            _config: Vec<u8>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn plugin_image(&self, _id: Uuid) -> Result<Option<PluginImage>, ServiceError> {
            Ok(None)
        }
        async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn set_repositories(
            &self,
            _repositories: Vec<RepositoryInfo>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
            Ok(Vec::new())
        }
    }

    /// A manager whose `plugin_id` configuration is `config`; every other id is
    /// not found (so a client reading the wrong plugin falls back to defaults
    /// rather than silently picking up another plugin's settings).
    pub(crate) fn manager_with(plugin_id: Uuid, config: Vec<u8>) -> Arc<dyn PluginManager> {
        Arc::new(OnePlugin {
            id: plugin_id,
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioDbConfig, ConfigSource, MusicBrainzConfig, OmdbConfig, StudioImagesConfig, TmdbConfig,
    };

    #[test]
    fn defaults_are_the_csharp_plugin_configuration_defaults() {
        let t = TmdbConfig::default();
        assert_eq!((t.max_cast_members, t.max_crew_members), (15, 15));
        assert!(!t.include_adult && !t.import_season_name && !t.exclude_tags_movies);
        assert!(t.poster_size.is_none(), "unset sizes mean `original`");
        assert_eq!(
            MusicBrainzConfig::default().server,
            "https://musicbrainz.org"
        );
        assert!((MusicBrainzConfig::default().rate_limit - 1.0).abs() < f64::EPSILON);
        assert!(!OmdbConfig::default().cast_and_crew);
        assert!(!AudioDbConfig::default().replace_album_name);
        assert!(
            StudioImagesConfig::default()
                .repository_url
                .ends_with("/studios")
        );
    }

    #[test]
    fn a_partial_body_keeps_the_other_defaults() {
        // The manager overlays defaults already; `serde(default)` is the second
        // net, for a body written by an older dashboard or by hand.
        let cfg: TmdbConfig = serde_json::from_str(r#"{"MaxCastMembers":3}"#).expect("json");
        assert_eq!(cfg.max_cast_members, 3);
        assert_eq!(cfg.max_crew_members, 15);
        assert!(!cfg.hide_missing_cast_members);
    }

    #[test]
    fn the_wire_shape_the_dashboard_saves_round_trips() {
        // The exact key set `builtin_plugins::TMDB.default_config` registers.
        let cfg: TmdbConfig = serde_json::from_str(
            r#"{"TmdbApiKey":"","IncludeAdult":true,"ExcludeTagsSeries":true,
                "ExcludeTagsMovies":false,"ImportSeasonName":true,"MaxCastMembers":5,
                "MaxCrewMembers":0,"HideMissingCastMembers":true,
                "HideMissingCrewMembers":false,"ProfileSize":"w185"}"#,
        )
        .expect("json");
        assert!(cfg.include_adult && cfg.exclude_tags_series && cfg.import_season_name);
        assert_eq!((cfg.max_cast_members, cfg.max_crew_members), (5, 0));
        assert!(cfg.hide_missing_cast_members && !cfg.hide_missing_crew_members);
        assert_eq!(cfg.profile_size.as_deref(), Some("w185"));
    }

    #[test]
    fn tmdb_image_urls_default_to_original_and_honour_a_size() {
        use super::TmdbImageKind;
        let cfg = TmdbConfig::default();
        assert_eq!(
            cfg.image_url(TmdbImageKind::Profile, "/abc.jpg"),
            "https://image.tmdb.org/t/p/original/abc.jpg"
        );
        let cfg = TmdbConfig {
            profile_size: Some("w185".to_owned()),
            // A whitespace-only value is "unset", not a size segment: it would
            // otherwise build `…/t/p/ /abc.jpg`.
            poster_size: Some("   ".to_owned()),
            ..TmdbConfig::default()
        };
        assert_eq!(
            cfg.image_url(TmdbImageKind::Profile, "/abc.jpg"),
            "https://image.tmdb.org/t/p/w185/abc.jpg"
        );
        assert_eq!(
            cfg.image_url(TmdbImageKind::Poster, "/p.jpg"),
            "https://image.tmdb.org/t/p/original/p.jpg"
        );
    }

    #[test]
    fn tmdb_api_key_prefers_the_dashboard_and_falls_back_to_the_builtin() {
        assert_eq!(TmdbConfig::default().api_key("builtin"), "builtin");
        let cfg = TmdbConfig {
            tmdb_api_key: "  personal  ".to_owned(),
            ..TmdbConfig::default()
        };
        assert_eq!(cfg.api_key("builtin"), "personal");
    }

    #[test]
    fn musicbrainz_normalization_ports_the_csharp_setters() {
        // TrimEnd('/') on Server.
        let cfg = MusicBrainzConfig {
            server: "https://mirror.test/".to_owned(),
            rate_limit: 0.1,
            replace_artist_name: true,
        }
        .normalized();
        assert_eq!(cfg.server, "https://mirror.test");
        // A mirror MAY be polled faster than 1/sec.
        assert!((cfg.rate_limit - 0.1).abs() < f64::EPSILON);

        // The official server may not: the setter clamps back to the default.
        let cfg = MusicBrainzConfig {
            server: "https://musicbrainz.org".to_owned(),
            rate_limit: 0.01,
            replace_artist_name: false,
        }
        .normalized();
        assert!((cfg.rate_limit - 1.0).abs() < f64::EPSILON);

        // A blank server is "invalid" -> official (ReloadConfig's fallback).
        let cfg = MusicBrainzConfig {
            server: "   ".to_owned(),
            rate_limit: 5.0,
            replace_artist_name: false,
        }
        .normalized();
        assert_eq!(cfg.server, "https://musicbrainz.org");
        assert!((cfg.rate_limit - 5.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn an_unattached_source_yields_the_defaults() {
        let src = ConfigSource::new();
        assert!(!src.is_attached());
        let cfg: TmdbConfig = src.load(crate::builtin_plugins::TMDB.id).await;
        assert_eq!(cfg, TmdbConfig::default());
    }
}
